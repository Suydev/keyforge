//! Local Wi-Fi link quality, sampled from Android.
//!
//! # Why the gateway cares about the radio link
//!
//! Every latency number this gateway measures is end-to-end: it includes the
//! local Wi-Fi hop. When that hop degrades, TTFB rises for reasons the provider
//! is not responsible for — and the router responds by marking keys slow,
//! rotating them, and tripping breakers on hosts that were answering perfectly
//! well. The provider gets blamed for the tablet's antenna.
//!
//! This module supplies the missing signal so that case can be told apart. It is
//! read-only observation; nothing here can block a request.
//!
//! # Where the numbers come from
//!
//! Two sources, best first, because they are not equally informative:
//!
//! 1. **`rish`** (Shizuku shell, uid 2000) running `dumpsys wifi`. This is the
//!    same data Android's own connectivity logic uses:
//!
//!    ```text
//!    mWifiInfo SSID: "...", RSSI: -55, Link speed: 86Mbps, Tx Link speed: 86Mbps,
//!              Rx Link speed: 72Mbps, Frequency: 2442MHz, score: 60, isUsable: true
//!    ```
//!
//!    `score` and `isUsable` are Android's *own verdict* on the link, which is
//!    strictly better than inventing RSSI thresholds here. The per-poll history
//!    additionally carries `tx`/`rx` throughput and retry counters — retransmits
//!    are the direct measure of a flaky link, and RSSI can look healthy while
//!    they spike.
//!
//! 2. **`termux-wifi-connectioninfo`**, which needs no Shizuku and always works,
//!    but exposes only `rssi` / `link_speed_mbps` / `frequency_mhz`. Used when
//!    rish is absent or Shizuku has not been restarted after a reboot.
//!
//! # Cost
//!
//! `dumpsys wifi` is ~200KB of text and spawns a JVM per call, so it is polled
//! on a slow interval and cached. Nothing on the request path ever shells out;
//! callers read a cached snapshot behind a mutex. On a device where the whole
//! battery budget is ~5%/hour this matters more than the freshness would.
//!
//! # What is deliberately NOT done
//!
//! A weak link never refuses or delays a request. A -70dBm link still moves
//! ~500KB/s perfectly well, so gating traffic on signal strength would invent an
//! outage that does not exist. The only outputs are (a) a multiplier that widens
//! head budgets and (b) a hint that retry cadence should ease off.

use crate::config;
#[cfg(unix)]
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

/// A point-in-time reading of the local Wi-Fi link.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LinkInfo {
    /// Signal strength in dBm. Negative; closer to zero is better.
    pub rssi: Option<i32>,
    /// Negotiated link rate in Mbps (not throughput).
    pub link_mbps: Option<u32>,
    /// Band, e.g. 2442 (2.4GHz) or 5220 (5GHz).
    pub freq_mhz: Option<u32>,
    /// Android's own link score. Observed range ~0-60; 60 is a healthy link.
    pub score: Option<i32>,
    /// Android's own usability verdict.
    pub usable: Option<bool>,
    /// Measured transmit throughput in Mbps at the last poll.
    pub tx_mbps: Option<f64>,
    /// Measured receive throughput in Mbps at the last poll.
    pub rx_mbps: Option<f64>,
    /// `true` when the AP is a phone hotspot / metered connection. Worth
    /// surfacing: a hotspot's variance is much higher than a router's.
    pub metered: bool,
    /// Which source produced this: `"rish"`, `"termux-api"`, or `""` if none.
    pub source: &'static str,
    /// Unix seconds when this was sampled. 0 means never sampled.
    pub sampled_at: u64,
}

impl LinkInfo {
    /// Coarse quality bucket, for display and for the slowdown multiplier.
    ///
    /// Prefers Android's `score` when present, because it already folds in
    /// retries and throughput. Falls back to RSSI thresholds, which are the
    /// conventional ones: better than -60 is good, worse than -75 is poor.
    pub fn quality(&self) -> LinkQuality {
        if self.sampled_at == 0 {
            return LinkQuality::Unknown;
        }
        if let Some(false) = self.usable {
            return LinkQuality::Poor;
        }
        if let Some(s) = self.score {
            return match s {
                50.. => LinkQuality::Good,
                30..=49 => LinkQuality::Fair,
                _ => LinkQuality::Poor,
            };
        }
        match self.rssi {
            Some(r) if r >= -60 => LinkQuality::Good,
            Some(r) if r >= -75 => LinkQuality::Fair,
            Some(_) => LinkQuality::Poor,
            None => LinkQuality::Unknown,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkQuality {
    /// Never sampled, or no usable signal fields. Treated exactly like `Good`
    /// so that a missing sampler can never penalise a request.
    Unknown,
    Good,
    Fair,
    Poor,
}

impl LinkQuality {
    pub fn as_str(self) -> &'static str {
        match self {
            LinkQuality::Unknown => "unknown",
            LinkQuality::Good => "good",
            LinkQuality::Fair => "fair",
            LinkQuality::Poor => "poor",
        }
    }

    /// Multiplier applied to the head budget.
    ///
    /// Deliberately gentle. The budget already carries a size term and a safety
    /// factor; this only has to cover the extra latency a degraded local hop
    /// adds. `Unknown` must be 1.0 — an absent sampler is not evidence of a bad
    /// link, and defaulting otherwise would silently change routing on any
    /// device without Shizuku.
    pub fn budget_multiplier(self) -> f64 {
        match self {
            LinkQuality::Unknown | LinkQuality::Good => 1.0,
            LinkQuality::Fair => 1.2,
            LinkQuality::Poor => 1.5,
        }
    }
}

/// Process-wide cache. `Mutex` rather than `RwLock`: writes are once a minute
/// and reads are cheap, so contention is irrelevant and `Mutex` is simpler.
fn cache() -> &'static Mutex<LinkInfo> {
    static CACHE: OnceLock<Mutex<LinkInfo>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(LinkInfo::default()))
}

/// Latest cached reading. Never blocks on I/O.
pub fn current() -> LinkInfo {
    cache().lock().map(|g| g.clone()).unwrap_or_default()
}

/// Multiplier for the current link, for callers that want only the number.
pub fn budget_multiplier() -> f64 {
    current().quality().budget_multiplier()
}

/// Run a command with a hard timeout, returning stdout on success.
///
/// `Command` has no built-in timeout, and an unkillable `dumpsys` on a wedged
/// system service would otherwise hang the sampler thread forever.
///
/// On Unix the child is killed with `kill(2)` via a watchdog thread — `Child::kill`
/// needs `&mut`, which the reader thread already holds. On other platforms the
/// sampler never runs (see `sample_once`), so a portable fallback that merely
/// waits is sufficient and keeps the module compiling everywhere.
#[cfg(unix)]
fn run_with_timeout(program: &str, args: &[&str], secs: u64) -> Option<String> {
    let child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let killer = {
        // The killer thread gets the raw pid rather than the handle, since
        // `wait_with_output` below consumes `child`.
        let pid = child.id();
        std::thread::spawn(move || {
            if rx.recv_timeout(Duration::from_secs(secs)).is_err() {
                // SAFETY: pid is a live direct child; worst case it has already
                // exited and the signal goes nowhere. Never reaped by anyone
                // else, so the pid cannot have been recycled yet.
                unsafe {
                    libc_kill(pid as i32, 9);
                }
            }
        })
    };

    let out = child.wait_with_output().ok();
    let _ = tx.send(());
    let _ = killer.join();

    let out = out?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Non-Unix stub. Unreachable in practice because `sample_once` short-circuits
/// on non-Android targets; present so the module type-checks on Windows.
#[cfg(not(unix))]
fn run_with_timeout(_program: &str, _args: &[&str], _secs: u64) -> Option<String> {
    None
}

// Minimal `kill(2)` binding, so this module needs no `libc` dependency.
// A doc comment here is silently dropped by rustc, hence a plain comment.
#[cfg(unix)]
extern "C" {
    #[link_name = "kill"]
    fn libc_kill(pid: i32, sig: i32) -> i32;
}

/// Pull `key: value` out of a `dumpsys` line, tolerating `,`/`"` terminators.
fn field<'a>(hay: &'a str, key: &str) -> Option<&'a str> {
    let at = hay.find(key)? + key.len();
    let rest = hay[at..].trim_start_matches([':', '=', ' ']);
    let end = rest.find([',', '"', '\n']).unwrap_or(rest.len());
    let v = rest[..end].trim();
    if v.is_empty() {
        None
    } else {
        Some(v)
    }
}

/// Leading numeric prefix of `s`, e.g. `"86Mbps"` -> `86`.
fn num_prefix<T: std::str::FromStr>(s: &str) -> Option<T> {
    let end = s
        .find(|c: char| !(c.is_ascii_digit() || c == '-' || c == '+' || c == '.'))
        .unwrap_or(s.len());
    s[..end].parse().ok()
}

/// Parse the `mWifiInfo` line and the newest RSSI-poll record out of
/// `dumpsys wifi` output.
pub fn parse_dumpsys(out: &str) -> Option<LinkInfo> {
    let info_line = out.lines().find(|l| l.contains("mWifiInfo"))?;

    let mut li = LinkInfo {
        source: "rish",
        sampled_at: config::now_secs(),
        ..Default::default()
    };

    li.rssi = field(info_line, "RSSI").and_then(num_prefix);
    li.link_mbps = field(info_line, "Link speed").and_then(num_prefix);
    li.freq_mhz = field(info_line, "Frequency").and_then(num_prefix);
    li.score = field(info_line, "score").and_then(num_prefix);
    li.usable = field(info_line, "isUsable").map(|v| v.eq_ignore_ascii_case("true"));
    li.metered = field(info_line, "Metered hint")
        .map(|v| v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);

    // Throughput lives only in the poll history:
    //   rec[99]: ... rssi=-59 f=2442 sc=60 link=86 tx=96.1, 0.0, 0.0 rx=254.4 ...
    // Take the LAST such record; earlier ones can be many minutes stale.
    if let Some(rec) = out
        .lines()
        .rfind(|l| l.contains("rssi=") && l.contains(" tx="))
    {
        li.tx_mbps = field(rec, " tx=").and_then(num_prefix);
        li.rx_mbps = field(rec, " rx=").and_then(num_prefix);
        // A fresher RSSI than the summary line, when present.
        if let Some(r) = field(rec, "rssi=").and_then(num_prefix) {
            li.rssi = Some(r);
        }
    }

    // A reading with no signal fields at all is worse than no reading: it would
    // register as "sampled" and suppress the fallback.
    if li.rssi.is_none() && li.score.is_none() && li.link_mbps.is_none() {
        return None;
    }
    Some(li)
}

/// Parse `termux-wifi-connectioninfo` JSON.
///
/// Hand-rolled rather than via serde_json's `Value` because the fields are three
/// flat integers and the error path must stay allocation-light.
pub fn parse_termux_json(out: &str) -> Option<LinkInfo> {
    let grab = |key: &str| -> Option<i64> {
        let at = out.find(&format!("\"{key}\""))? + key.len() + 2;
        let rest = out[at..].trim_start_matches([':', ' ']);
        let end = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '-'))
            .unwrap_or(rest.len());
        rest[..end].parse().ok()
    };

    let rssi = grab("rssi");
    let link = grab("link_speed_mbps");
    if rssi.is_none() && link.is_none() {
        return None;
    }
    Some(LinkInfo {
        rssi: rssi.map(|v| v as i32),
        link_mbps: link.map(|v| v as u32),
        freq_mhz: grab("frequency_mhz").map(|v| v as u32),
        score: None,
        usable: None,
        tx_mbps: None,
        rx_mbps: None,
        metered: false,
        source: "termux-api",
        sampled_at: config::now_secs(),
    })
}

/// Sample once, preferring rish. Returns `None` when no source is available.
///
/// Both sources are Android-specific: `dumpsys` is an Android system service and
/// `termux-wifi-connectioninfo` ships with Termux:API. On every other target this
/// returns `None` immediately, the cache stays `Default`, `quality()` reports
/// `Unknown`, and the budget multiplier stays exactly 1.0 — so a Linux, macOS, or
/// Windows deployment behaves precisely as it would if this module did not exist.
pub fn sample_once() -> Option<LinkInfo> {
    if !cfg!(target_os = "android") {
        return None;
    }
    let rish = format!(
        "{}/.local/rish/rish",
        std::env::var("HOME").unwrap_or_default()
    );
    if std::path::Path::new(&rish).exists() {
        if let Some(out) = run_with_timeout(&rish, &["-c", "dumpsys wifi"], 20) {
            if let Some(li) = parse_dumpsys(&out) {
                return Some(li);
            }
        }
    }
    // Fallback: fewer fields, but no Shizuku dependency.
    if let Some(out) = run_with_timeout("termux-wifi-connectioninfo", &[], 12) {
        if let Some(li) = parse_termux_json(&out) {
            return Some(li);
        }
    }
    None
}

/// Start the background sampler.
///
/// A plain OS thread, not a tokio task: this blocks on process spawns and would
/// otherwise occupy a runtime worker for seconds at a time. Logs only on a
/// *change* of quality bucket, so a stable link is silent in the log.
///
/// Returns immediately without spawning on non-Android targets — no sources
/// exist there, so a thread would only sleep forever.
pub fn spawn(app: Arc<crate::state::App>) {
    if !cfg!(target_os = "android") {
        return;
    }
    std::thread::spawn(move || {
        let mut last = LinkQuality::Unknown;
        loop {
            if let Some(li) = sample_once() {
                let q = li.quality();
                if let Ok(mut g) = cache().lock() {
                    *g = li.clone();
                }
                if q != last {
                    let detail = match (li.rssi, li.score) {
                        (Some(r), Some(s)) => format!("rssi {r}dBm, score {s}"),
                        (Some(r), None) => format!("rssi {r}dBm"),
                        _ => li.source.to_string(),
                    };
                    // Poor is a warning because it changes routing behaviour;
                    // recovery is info.
                    app.event(
                        if q == LinkQuality::Poor {
                            "warn"
                        } else {
                            "info"
                        },
                        format!(
                            "wifi link {} ({detail}) — head budgets x{:.2}",
                            q.as_str(),
                            q.budget_multiplier()
                        ),
                    );
                    last = q;
                }
            }
            std::thread::sleep(Duration::from_secs(config::link_poll_secs()));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real `dumpsys wifi` output from the device, trimmed to the lines parsed.
    const DUMPSYS: &str = r#"
 rec[95]: time=09-04 19:02:01.969 processed=L3ConnectedState org=L3ConnectedState dest=<null> what=CMD_UNWANTED_NETWORK screen=on 1 0 "OnePlus 11R 5G" a2:b4:a9:77:18:3a rssi=-68 f=2442 sc=60 link=78 tx=4.1, 0.0, 0.0 rx=1.5 bcn=0 [on:0 tx:0 rx:0 period:4397743] from screen [on:0 period:523040] score=60
 rec[99]: time=09-04 19:08:30.633 processed=L2ConnectedState org=L3ConnectedState dest=<null> what=CMD_ONESHOT_RSSI_POLL screen=on 0 0 "OnePlus 11R 5G" a2:b4:a9:77:18:3a rssi=-59 f=2442 sc=60 link=86 tx=96.1, 0.0, 0.0 rx=254.4 bcn=0 [on:0 tx:0 rx:0 period:388664] from screen [on:0 period:911704] score=60
mWifiInfo SSID: "OnePlus 11R 5G", BSSID: a2:b4:a9:77:18:3a, MAC: 1e:9e:e0:fc:8e:6e, IP: /10.32.184.148, Security type: 2, Supplicant state: COMPLETED, Wi-Fi standard: 11n, RSSI: -55, Link speed: 86Mbps, Tx Link speed: 86Mbps, Max Supported Tx Link speed: 72Mbps, Calculated Tx : 0Mbps, Rx Link speed: 72Mbps, Frequency: 2442MHz, Net ID: 15, Metered hint: true, score: 60, isUsable: true, CarrierMerged: false
"#;

    #[test]
    fn parses_real_dumpsys_output() {
        let li = parse_dumpsys(DUMPSYS).expect("must parse");
        assert_eq!(li.link_mbps, Some(86));
        assert_eq!(li.freq_mhz, Some(2442));
        assert_eq!(li.score, Some(60));
        assert_eq!(li.usable, Some(true));
        assert!(li.metered, "phone hotspot must be flagged metered");
        assert_eq!(li.source, "rish");
        // RSSI comes from the newest poll record, not the summary line.
        assert_eq!(li.rssi, Some(-59), "newest poll wins over mWifiInfo");
        assert_eq!(li.tx_mbps, Some(96.1));
        assert_eq!(li.rx_mbps, Some(254.4));
    }

    #[test]
    fn dumpsys_without_signal_fields_is_rejected() {
        // Registering an empty reading as "sampled" would suppress the fallback.
        assert!(parse_dumpsys("mWifiInfo SSID: \"x\", Net ID: 15").is_none());
        assert!(parse_dumpsys("no wifi info at all here").is_none());
    }

    #[test]
    fn parses_termux_api_json() {
        let js = r#"{
          "bssid": "02:00:00:00:00:00", "frequency_mhz": 2442, "ip": "10.32.184.148",
          "link_speed_mbps": 86, "network_id": -1, "rssi": -60,
          "supplicant_state": "COMPLETED" }"#;
        let li = parse_termux_json(js).expect("must parse");
        assert_eq!(li.rssi, Some(-60));
        assert_eq!(li.link_mbps, Some(86));
        assert_eq!(li.freq_mhz, Some(2442));
        assert_eq!(li.source, "termux-api");
        // Fields dumpsys alone provides must stay absent, not be invented.
        assert_eq!(li.score, None);
        assert_eq!(li.usable, None);
    }

    #[test]
    fn termux_json_without_signal_is_rejected() {
        assert!(parse_termux_json(r#"{"error":"permission denied"}"#).is_none());
    }

    #[test]
    fn quality_prefers_android_score_over_rssi() {
        // Score says healthy while RSSI looks poor: Android's verdict wins,
        // because it already accounts for retries and throughput.
        let li = LinkInfo {
            rssi: Some(-80),
            score: Some(60),
            sampled_at: 1,
            ..Default::default()
        };
        assert_eq!(li.quality(), LinkQuality::Good);
    }

    #[test]
    fn quality_falls_back_to_rssi_thresholds() {
        let mk = |r: i32| LinkInfo {
            rssi: Some(r),
            sampled_at: 1,
            ..Default::default()
        };
        assert_eq!(mk(-45).quality(), LinkQuality::Good);
        assert_eq!(mk(-60).quality(), LinkQuality::Good);
        assert_eq!(mk(-70).quality(), LinkQuality::Fair);
        assert_eq!(mk(-85).quality(), LinkQuality::Poor);
    }

    #[test]
    fn unusable_link_is_poor_regardless_of_score() {
        let li = LinkInfo {
            rssi: Some(-50),
            score: Some(60),
            usable: Some(false),
            sampled_at: 1,
            ..Default::default()
        };
        assert_eq!(li.quality(), LinkQuality::Poor);
    }

    #[test]
    fn never_sampled_is_unknown_and_costs_nothing() {
        // The load-bearing property: a device without Shizuku, or one where the
        // sampler has not run yet, must behave exactly as before this module
        // existed. Any other default would silently change routing everywhere.
        let li = LinkInfo::default();
        assert_eq!(li.quality(), LinkQuality::Unknown);
        assert_eq!(li.quality().budget_multiplier(), 1.0);
        assert_eq!(LinkQuality::Good.budget_multiplier(), 1.0);
    }

    #[test]
    fn multipliers_are_ordered_and_bounded() {
        let good = LinkQuality::Good.budget_multiplier();
        let fair = LinkQuality::Fair.budget_multiplier();
        let poor = LinkQuality::Poor.budget_multiplier();
        assert!(good < fair && fair < poor, "worse link => more time");
        // A local link problem must not be able to inflate the budget without
        // bound; the ceiling clamp is the backstop but this keeps it sane.
        assert!(poor <= 2.0, "multiplier must stay modest, got {poor}");
    }
}
