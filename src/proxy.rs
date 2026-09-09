//! Outbound proxy support: HTTP CONNECT tunnelling with health-aware rotation.
//!
//! # Why
//! All 1745 keys currently reach tabitoken/gorouter from one device IP. That is
//! exactly the pattern Cloudflare answers with an HTML 502/524 interstitial —
//! observed repeatedly in the error log as `UPSTREAM ... <!DOCTYPE html>`.
//! Spreading requests over the proxy pool makes the traffic look like many
//! clients instead of one hammering device.
//!
//! # How
//! A custom connector performs the `CONNECT` handshake to a proxy, then hands the
//! raw tunnel to `hyper-rustls` for TLS. Because it is a real connector, the
//! normal pooled `Client` still applies — connections stay warm, which matters
//! for battery.
//!
//! Verified live against the real pool: a CONNECT tunnel returns HTTP 200 from
//! tabitoken and the provider sees the proxy IP, not the device IP.
//!
//! # Failure policy
//! A proxy that fails is cooled down, not deleted. If every proxy is cooling,
//! the connector falls back to a **direct** connection rather than failing the
//! request — availability beats IP hygiene.

use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::task::{Context, Poll};

use hyper::Uri;
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Base64, implemented inline to avoid a dependency for ~20 lines of work.
fn b64(input: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(T[((n >> 18) & 63) as usize] as char);
        out.push(T[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            T[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// One proxy endpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProxyEndpoint {
    pub host: String,
    pub port: u16,
    pub user: Option<String>,
    pub pass: Option<String>,
}

impl ProxyEndpoint {
    pub fn addr(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    /// Pre-encoded `Proxy-Authorization` value, when credentials are present.
    fn auth_header(&self) -> Option<String> {
        match (&self.user, &self.pass) {
            (Some(u), Some(p)) => Some(b64(format!("{u}:{p}").as_bytes())),
            (Some(u), None) => Some(b64(format!("{u}:").as_bytes())),
            _ => None,
        }
    }

    /// Parse either accepted format:
    ///   `host:port:user:pass`            (zingproxy / webshare style)
    ///   `http://user:pass@host:port`     (URL style)
    ///   `host:port`                      (no auth)
    pub fn parse(line: &str) -> Option<Self> {
        // Strip a trailing comment before anything else.
        //
        // `save_file` annotates each entry with `  # ok=4 fail=0 490ms`, and
        // without this the port parse sees "3001  # ok=4 …" and fails, so every
        // line the gateway wrote was silently dropped on reload. The pool emptied
        // itself and egress fell back to direct — a writer and reader that
        // disagreed about their own format.
        //
        // Handled here rather than in the writer because a trailing comment is a
        // reasonable thing for a human to add to a config file too.
        let s = match line.find('#') {
            Some(0) => "",
            Some(i) => &line[..i],
            None => line,
        };
        let s = s.trim();
        if s.is_empty() {
            return None;
        }

        if let Some(rest) = s
            .strip_prefix("http://")
            .or_else(|| s.strip_prefix("https://"))
        {
            let (creds, hostport) = match rest.rsplit_once('@') {
                Some((c, h)) => (Some(c), h),
                None => (None, rest),
            };
            let (host, port) = hostport.rsplit_once(':')?;
            let (user, pass) = match creds {
                Some(c) => match c.split_once(':') {
                    Some((u, p)) => (Some(u.to_string()), Some(p.to_string())),
                    None => (Some(c.to_string()), None),
                },
                None => (None, None),
            };
            return Some(Self {
                host: host.to_string(),
                port: port.parse().ok()?,
                user,
                pass,
            });
        }

        let parts: Vec<&str> = s.split(':').collect();
        match parts.len() {
            2 => Some(Self {
                host: parts[0].to_string(),
                port: parts[1].parse().ok()?,
                user: None,
                pass: None,
            }),
            4 => Some(Self {
                host: parts[0].to_string(),
                port: parts[1].parse().ok()?,
                user: Some(parts[2].to_string()),
                pass: Some(parts[3].to_string()),
            }),
            _ => None,
        }
    }
}


/// Parse a ProxyScrape-style JSON list into endpoints.
///
/// Only `http` proxies with `ssl: true` are accepted, and that is not a
/// preference — an HTTPS origin needs a `CONNECT` tunnel, which SOCKS entries in
/// this file cannot provide and plaintext-only HTTP proxies refuse. Of 1480
/// records in a live download, 749 were http and 229 of those had ssl.
///
/// Accepts either the wrapped shape (`{"proxies": [...]}`) or a bare array, so a
/// hand-trimmed file still works.
pub fn parse_proxyscrape_json(bytes: &[u8]) -> Vec<ProxyEndpoint> {
    let v: serde_json::Value = match serde_json::from_slice(bytes) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let arr = match v.get("proxies").and_then(|p| p.as_array()) {
        Some(a) => a.clone(),
        None => match v.as_array() {
            Some(a) => a.clone(),
            None => return Vec::new(),
        },
    };

    let mut out: Vec<(f64, f64, ProxyEndpoint)> = Vec::new();
    for r in arr {
        if r.get("protocol").and_then(|x| x.as_str()) != Some("http") {
            continue;
        }
        if r.get("ssl").and_then(|x| x.as_bool()) != Some(true) {
            continue;
        }
        // `alive` is the provider's own last check; absent means unknown, which
        // we accept because vetting will settle it either way.
        if r.get("alive").and_then(|x| x.as_bool()) == Some(false) {
            continue;
        }
        let Some(ip) = r.get("ip").and_then(|x| x.as_str()) else { continue };
        let port = match r.get("port") {
            Some(serde_json::Value::Number(n)) => n.as_u64().unwrap_or(0),
            Some(serde_json::Value::String(s)) => s.parse().unwrap_or(0),
            _ => 0,
        };
        if port == 0 || port > u16::MAX as u64 {
            continue;
        }
        let uptime = r.get("uptime").and_then(|x| x.as_f64()).unwrap_or(0.0);
        let timeout = r.get("timeout").and_then(|x| x.as_f64()).unwrap_or(f64::MAX);
        out.push((
            uptime,
            timeout,
            ProxyEndpoint {
                host: ip.to_string(),
                port: port as u16,
                user: None,
                pass: None,
            },
        ));
    }

    // Highest uptime first, then lowest reported latency. Vetting is expensive
    // (a real request each), so the order candidates are tried in decides how
    // much of the list has to be walked before enough working ones are found.
    out.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
    });
    out.into_iter().map(|(_, _, e)| e).collect()
}

#[derive(Clone, Debug, Default)]
pub struct ProxyHealth {
    pub ok: u64,
    pub fail: u64,
    /// Unix seconds until which this proxy is skipped.
    pub cool_until: u64,
    /// Consecutive failures with no success in between.
    ///
    /// Distinct from `fail`: a proxy that works once every ten tries is useless
    /// but would never look bad on a lifetime ratio. This is what eviction reads,
    /// because free proxies do not recover — they are simply gone.
    pub consecutive_fail: u32,
    /// When this endpoint last passed a real provider request. 0 = never vetted.
    pub vetted_at: u64,
    pub last_used: u64,
    /// Same instant in milliseconds, used only for LRU ordering.
    ///
    /// Kept separate from `last_used` rather than replacing it: `last_used` is
    /// what the dashboard displays, so changing its unit would silently
    /// reinterpret a number that is already being read elsewhere.
    pub last_used_ms: u64,
    pub last_error: String,
    /// EWMA connect latency in ms.
    pub ewma_ms: Option<f64>,
}

/// Outcome of loading a pool file.
#[derive(Clone, Debug)]
pub struct LoadReport {
    /// Endpoints in the pool after the attempt.
    pub loaded: usize,
    /// Non-comment lines that could not be parsed.
    pub skipped: usize,
    /// True when the file was rejected and the previous pool was kept.
    pub kept_existing: bool,
    pub note: String,
}

/// The pool: parses `proxies.txt`, rotates least-recently-used, tracks health.
pub struct ProxyPool {
    endpoints: Mutex<Vec<ProxyEndpoint>>,
    health: Mutex<HashMap<String, ProxyHealth>>,
    /// Master switch. When false the connector always goes direct.
    enabled: std::sync::atomic::AtomicBool,
    rotations: AtomicU64,
    direct_fallbacks: AtomicU64,
}

/// Wall clock in whole seconds, for cooldowns.
fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Wall clock in milliseconds, for LRU ordering.
///
/// Second resolution is not enough here and the failure is not subtle. `pick()`
/// selects the minimum `last_used` and stamps the winner with the current time,
/// so when several sessions connect inside the same second they all read
/// identical values, the tie breaks by iteration order, and **they all receive
/// the same proxy**. Seven concurrent sessions then egress from one IP instead of
/// seven.
///
/// That is not hypothetical. Upstream rate limits are per-IP as well as
/// per-account, and ten separate accounts were rate-limited together because
/// every request left from one address.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Base cooldown after a failure. Multiplied by the consecutive-failure streak,
/// so a transient blip is retried soon and a dead endpoint is left alone.
const COOL_SECS: u64 = 90;

/// Consecutive failures before an endpoint is dropped from the pool entirely.
///
/// Free proxies are not like paid ones: they do not usually recover. Keeping a
/// dead entry costs a connect timeout on every rotation that reaches it, so the
/// pool has to shed them rather than cool them forever. Three is deliberate —
/// one is noise, two could be a bad minute, three with growing backoff between
/// them is gone.
const EVICT_AFTER_FAILS: u32 = 3;

/// Below this many usable endpoints, the pool asks to be refilled.
const REFILL_BELOW: usize = 8;

impl ProxyPool {
    pub fn new() -> Self {
        Self {
            endpoints: Mutex::new(Vec::new()),
            health: Mutex::new(HashMap::new()),
            enabled: std::sync::atomic::AtomicBool::new(false),
            rotations: AtomicU64::new(0),
            direct_fallbacks: AtomicU64::new(0),
        }
    }

    pub fn set_enabled(&self, v: bool) {
        self.enabled.store(v, Ordering::Relaxed);
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// Load from a file, replacing the endpoint list. Health is preserved for
    /// endpoints that are still present.
    pub fn load_file(&self, path: &std::path::Path) -> usize {
        self.load_file_detailed(path).loaded
    }

    /// Load with a report of what happened, so a bad file is visible rather than
    /// silent.
    ///
    /// # Refusing to wipe
    /// This used to assign the parsed list unconditionally, which meant a file
    /// that yielded nothing emptied a working pool — and that is exactly what
    /// happened: `save_file` annotates each line with `# ok=4 fail=0 490ms`,
    /// `parse` did not strip the comment, so every line the gateway had written
    /// was rejected and "Reload" set the pool to zero. Egress silently fell back
    /// to direct, into the Cloudflare block the pool exists to avoid.
    ///
    /// The parser bug is fixed, but the destructive behaviour was the larger
    /// fault: a reload that parses nothing is almost always an error — missing
    /// file, bad edit, truncated write — and discarding a live pool in response is
    /// the worst available outcome. So an empty result over a non-empty pool is
    /// now refused and reported.
    pub fn load_file_detailed(&self, path: &std::path::Path) -> LoadReport {
        let text = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) => {
                return LoadReport {
                    loaded: self.len(),
                    skipped: 0,
                    kept_existing: true,
                    note: format!("cannot read {}: {e}", path.display()),
                }
            }
        };

        let mut list: Vec<ProxyEndpoint> = Vec::new();
        let mut skipped = 0usize;
        for line in text.lines() {
            // Blank and comment-only lines are not failures, so they are not
            // counted as skipped — otherwise the header alone reads as 3 errors.
            let stripped = match line.find('#') {
                Some(0) => "",
                Some(i) => &line[..i],
                None => line,
            };
            if stripped.trim().is_empty() {
                continue;
            }
            match ProxyEndpoint::parse(line) {
                Some(e) => list.push(e),
                None => skipped += 1,
            }
        }

        if list.is_empty() && self.len() > 0 {
            return LoadReport {
                loaded: self.len(),
                skipped,
                kept_existing: true,
                note: format!(
                    "{} parsed no usable entries ({skipped} unparseable) — keeping the {} already loaded",
                    path.display(),
                    self.len()
                ),
            };
        }

        let n = list.len();
        let keys: std::collections::HashSet<String> = list.iter().map(|e| e.addr()).collect();
        {
            let mut h = self.health.lock().unwrap_or_else(|e| e.into_inner());
            // Health survives for endpoints still present, so a reload does not
            // discard the success/failure record that ranks them.
            h.retain(|k, _| keys.contains(k));
            for e in &list {
                h.entry(e.addr()).or_default();
            }
        }
        *self.endpoints.lock().unwrap_or_else(|e| e.into_inner()) = list;
        LoadReport {
            loaded: n,
            skipped,
            kept_existing: false,
            note: if skipped > 0 {
                format!("{n} loaded, {skipped} lines unparseable")
            } else {
                format!("{n} loaded")
            },
        }
    }

    pub fn len(&self) -> usize {
        self.endpoints
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len()
    }

    /// Pick the least-recently-used healthy proxy, or `None` to go direct.
    pub fn pick(&self) -> Option<ProxyEndpoint> {
        if !self.is_enabled() {
            // Counted. "Am I actually rotating?" is unanswerable otherwise, and
            // a disabled pool is the single most important case to see: it is
            // exactly the state the gateway was in while ten accounts were
            // rate-limited together from one egress IP.
            self.direct_fallbacks.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        let eps = self
            .endpoints
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if eps.is_empty() {
            // Also counted, same reason.
            self.direct_fallbacks.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        let t = now();
        let mut h = self.health.lock().unwrap_or_else(|e| e.into_inner());

        // Least-recently-used among those not cooling. LRU rather than random so
        // the load spreads evenly and no single proxy IP gets hot.
        //
        // Ordering is in MILLISECONDS. With second resolution, sessions arriving
        // inside the same second all read the same `last_used`, tie-break by
        // iteration order, and every one of them gets the same proxy — which is
        // how ten separate accounts came to share one egress IP and were
        // rate-limited together. Cooldowns stay in seconds; only the ordering key
        // is fine-grained.
        let mut best: Option<(u64, ProxyEndpoint)> = None;
        for e in &eps {
            let st = h.entry(e.addr()).or_default();
            if st.cool_until > t {
                continue;
            }
            // Past the eviction threshold but not yet swept: skip rather than
            // spend a connect timeout on something already known dead.
            if st.consecutive_fail >= EVICT_AFTER_FAILS {
                continue;
            }
            if best
                .as_ref()
                .map(|(lu, _)| st.last_used_ms < *lu)
                .unwrap_or(true)
            {
                best = Some((st.last_used_ms, e.clone()));
            }
        }

        match best {
            Some((_, e)) => {
                if let Some(st) = h.get_mut(&e.addr()) {
                    // Stamp immediately, while the lock is still held, so a
                    // concurrent `pick()` cannot observe the pre-selection value
                    // and choose the same endpoint.
                    st.last_used_ms = now_ms();
                    st.last_used = t;
                }
                self.rotations.fetch_add(1, Ordering::Relaxed);
                Some(e)
            }
            None => {
                // Everything is cooling. Going direct beats failing the request.
                self.direct_fallbacks.fetch_add(1, Ordering::Relaxed);
                None
            }
        }
    }

    pub fn note_ok(&self, addr: &str, ms: u64) {
        let mut h = self.health.lock().unwrap_or_else(|e| e.into_inner());
        let st = h.entry(addr.to_string()).or_default();
        st.ok += 1;
        st.cool_until = 0;
        st.consecutive_fail = 0;
        st.vetted_at = now();
        st.last_error.clear();
        st.ewma_ms = Some(match st.ewma_ms {
            Some(p) => p * 0.7 + ms as f64 * 0.3,
            None => ms as f64,
        });
    }

    pub fn note_fail(&self, addr: &str, err: &str) {
        let mut h = self.health.lock().unwrap_or_else(|e| e.into_inner());
        let st = h.entry(addr.to_string()).or_default();
        st.fail += 1;
        st.consecutive_fail = st.consecutive_fail.saturating_add(1);
        // Back off further each time rather than retrying a corpse every 90s.
        // Free proxies mostly die permanently, so the cooldown grows with the
        // streak and eviction takes over past EVICT_AFTER_FAILS.
        let mult = st.consecutive_fail.min(6) as u64;
        st.cool_until = now() + COOL_SECS * mult;
        st.last_error = err.chars().take(160).collect();
    }


    /// Endpoints that are neither cooling nor evicted, i.e. actually pickable.
    pub fn healthy_count(&self) -> usize {
        let t = now();
        let eps = self
            .endpoints
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let h = self.health.lock().unwrap_or_else(|e| e.into_inner());
        eps.iter()
            .filter(|e| {
                h.get(&e.addr())
                    .map(|s| s.cool_until <= t && s.consecutive_fail < EVICT_AFTER_FAILS)
                    .unwrap_or(true)
            })
            .count()
    }

    /// Drop endpoints that have failed `EVICT_AFTER_FAILS` times in a row.
    ///
    /// Returns the addresses removed, so the caller can log what it lost rather
    /// than silently shrinking the pool.
    pub fn evict_dead(&self) -> Vec<String> {
        let mut removed = Vec::new();
        {
            let mut eps = self.endpoints.lock().unwrap_or_else(|e| e.into_inner());
            let h = self.health.lock().unwrap_or_else(|e| e.into_inner());
            eps.retain(|e| {
                let dead = h
                    .get(&e.addr())
                    .map(|s| s.consecutive_fail >= EVICT_AFTER_FAILS)
                    .unwrap_or(false);
                if dead {
                    removed.push(e.addr());
                }
                !dead
            });
        }
        if !removed.is_empty() {
            let mut h = self.health.lock().unwrap_or_else(|e| e.into_inner());
            for a in &removed {
                h.remove(a);
            }
        }
        removed
    }

    /// Add endpoints that are not already present. Returns how many were new.
    ///
    /// Deduplicated by `host:port` so a refill that overlaps the current pool
    /// cannot create a duplicate that would then be picked twice as often.
    pub fn add_endpoints(&self, list: Vec<ProxyEndpoint>) -> usize {
        let mut eps = self.endpoints.lock().unwrap_or_else(|e| e.into_inner());
        let mut h = self.health.lock().unwrap_or_else(|e| e.into_inner());
        let have: std::collections::HashSet<String> = eps.iter().map(|e| e.addr()).collect();
        let mut added = 0;
        for e in list {
            if have.contains(&e.addr()) {
                continue;
            }
            h.entry(e.addr()).or_default();
            eps.push(e);
            added += 1;
        }
        added
    }

    /// Does the pool need refilling?
    pub fn needs_refill(&self) -> bool {
        self.is_enabled() && self.healthy_count() < REFILL_BELOW
    }

    /// Persist the current endpoint list, best-first, so a restart keeps the
    /// vetted set instead of re-testing hundreds of candidates from scratch.
    ///
    /// Ordering is by success count then latency: the file doubles as a ranking,
    /// and `load_file` preserves order.
    pub fn save_file(&self, path: &std::path::Path) -> std::io::Result<usize> {
        let mut rows = self.snapshot();
        rows.sort_by(|a, b| {
            b.1.ok
                .cmp(&a.1.ok)
                .then_with(|| {
                    a.1.ewma_ms
                        .unwrap_or(f64::MAX)
                        .partial_cmp(&b.1.ewma_ms.unwrap_or(f64::MAX))
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
        });
        let mut out = String::new();
        out.push_str("# Vetted proxy pool, written by tabi-gateway.\n");
        out.push_str("# Best first: most successes, then lowest connect latency.\n");
        out.push_str("# Regenerated automatically — edits are kept but may be reordered.\n");
        for (e, hst) in &rows {
            match (&e.user, &e.pass) {
                (Some(u), Some(pw)) => out.push_str(&format!("{}:{}:{}:{}", e.host, e.port, u, pw)),
                _ => out.push_str(&format!("{}:{}", e.host, e.port)),
            }
            out.push_str(&format!(
                "  # ok={} fail={} {}\n",
                hst.ok,
                hst.fail,
                hst.ewma_ms.map(|v| format!("{}ms", v.round())).unwrap_or_else(|| "unmeasured".into())
            ));
        }
        // Temp + rename so a crash mid-write cannot truncate the pool file.
        let tmp = path.with_extension("txt.tmp");
        std::fs::write(&tmp, out.as_bytes())?;
        std::fs::rename(&tmp, path)?;
        Ok(rows.len())
    }

    /// Snapshot for the dashboard.
    pub fn snapshot(&self) -> Vec<(ProxyEndpoint, ProxyHealth)> {
        let eps = self
            .endpoints
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let h = self.health.lock().unwrap_or_else(|e| e.into_inner());
        eps.into_iter()
            .map(|e| {
                let st = h.get(&e.addr()).cloned().unwrap_or_default();
                (e, st)
            })
            .collect()
    }

    pub fn stats(&self) -> (u64, u64, usize) {
        let t = now();
        let h = self.health.lock().unwrap_or_else(|e| e.into_inner());
        let cooling = h.values().filter(|s| s.cool_until > t).count();
        (
            self.rotations.load(Ordering::Relaxed),
            self.direct_fallbacks.load(Ordering::Relaxed),
            cooling,
        )
    }
}

/// Perform the CONNECT handshake.
///
/// Reads the response header **one byte at a time** on purpose: over-reading
/// would consume the first bytes of the TLS handshake that follows on the same
/// socket.
async fn connect_tunnel(
    stream: &mut TcpStream,
    target: &str,
    auth: Option<&str>,
) -> io::Result<()> {
    let mut req = format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n");
    if let Some(a) = auth {
        req.push_str(&format!("Proxy-Authorization: Basic {a}\r\n"));
    }
    req.push_str("Proxy-Connection: Keep-Alive\r\n\r\n");
    stream.write_all(req.as_bytes()).await?;
    stream.flush().await?;

    let mut buf: Vec<u8> = Vec::with_capacity(128);
    let mut one = [0u8; 1];
    loop {
        let n = stream.read(&mut one).await?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "proxy closed during CONNECT",
            ));
        }
        buf.push(one[0]);
        if buf.len() >= 4 && &buf[buf.len() - 4..] == b"\r\n\r\n" {
            break;
        }
        if buf.len() > 8192 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "CONNECT response header too long",
            ));
        }
    }

    let head = String::from_utf8_lossy(&buf);
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);
    if status != 200 {
        let first = head.lines().next().unwrap_or("").trim().to_string();
        return Err(io::Error::other(format!("CONNECT refused: {first}")));
    }
    Ok(())
}

// Which egress address served the current request, if any.
//
// The connector runs inside the request future on the same task, so a
// task-local is the correct channel — no locks, no races. If the future ever
// migrates to a task where the scope is absent, readers get `None`, which the
// training data treats as "unknown" rather than failing.
//
// Plain comments, not doc comments: rustc silently discards a `///` on a macro
// invocation, so it would never have reached the docs anyway.
tokio::task_local! {
    pub(crate) static USED_EGRESS: std::cell::RefCell<Option<String>>;
}

/// Egress used by the current request, if the scope is present.
pub fn used_egress() -> Option<String> {
    USED_EGRESS.try_with(|s| s.borrow().clone()).ok().flatten()
}

/// Connector that tunnels through a proxy, or connects directly when the pool
/// says so. Plugged under `hyper-rustls` so TLS and pooling work unchanged.
#[derive(Clone)]
pub struct ProxyConnector {
    pool: std::sync::Arc<ProxyPool>,
}

impl ProxyConnector {
    pub fn new(pool: std::sync::Arc<ProxyPool>) -> Self {
        Self { pool }
    }
}

impl tower_service::Service<Uri> for ProxyConnector {
    type Response = TokioIo<TcpStream>;
    type Error = io::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, uri: Uri) -> Self::Future {
        let pool = self.pool.clone();
        Box::pin(async move {
            let host = uri
                .host()
                .ok_or_else(|| io::Error::other("uri has no host"))?
                .to_string();
            let port = uri.port_u16().unwrap_or(443);
            let target = format!("{host}:{port}");

            // Try one proxy, then fall back to direct. One retry only: the
            // caller already retries across keys and providers, and stacking
            // retries here would multiply latency.
            if let Some(p) = pool.pick() {
                let addr = p.addr();
                let started = std::time::Instant::now();
                match TcpStream::connect(&addr).await {
                    Ok(mut s) => {
                        let _ = s.set_nodelay(true);
                        match connect_tunnel(&mut s, &target, p.auth_header().as_deref()).await {
                            Ok(()) => {
                                pool.note_ok(&addr, started.elapsed().as_millis() as u64);
                                USED_EGRESS
                                    .try_with(|slot| *slot.borrow_mut() = Some(addr.clone()))
                                    .ok();
                                return Ok(TokioIo::new(s));
                            }
                            Err(e) => pool.note_fail(&addr, &e.to_string()),
                        }
                    }
                    Err(e) => pool.note_fail(&addr, &e.to_string()),
                }
            }

            // Direct.
            let s = TcpStream::connect((host.as_str(), port)).await?;
            let _ = s.set_nodelay(true);
            Ok(TokioIo::new(s))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── ProxyScrape JSON ingestion ──────────────────────────────────────────

    const SCRAPE_JSON: &[u8] = br#"{
      "shown_records": 5,
      "proxies": [
        {"ip":"1.1.1.1","port":8080,"protocol":"http","ssl":true,"alive":true,"uptime":99.0,"timeout":50.0},
        {"ip":"2.2.2.2","port":3128,"protocol":"http","ssl":true,"alive":true,"uptime":100.0,"timeout":20.0},
        {"ip":"3.3.3.3","port":1080,"protocol":"socks5","ssl":true,"alive":true,"uptime":100.0,"timeout":5.0},
        {"ip":"4.4.4.4","port":8080,"protocol":"http","ssl":false,"alive":true,"uptime":100.0,"timeout":5.0},
        {"ip":"5.5.5.5","port":9999,"protocol":"http","ssl":true,"alive":false,"uptime":100.0,"timeout":5.0}
      ]
    }"#;

    #[test]
    fn scrape_json_keeps_only_http_with_ssl() {
        // An HTTPS origin needs a CONNECT tunnel. SOCKS entries cannot provide
        // one through this connector, and ssl:false proxies refuse it — so both
        // must be dropped at parse time rather than wasting a vet attempt.
        let got = parse_proxyscrape_json(SCRAPE_JSON);
        let addrs: Vec<String> = got.iter().map(|e| e.addr()).collect();
        assert!(addrs.contains(&"1.1.1.1:8080".to_string()));
        assert!(addrs.contains(&"2.2.2.2:3128".to_string()));
        assert!(!addrs.iter().any(|a| a.starts_with("3.3.3.3")), "socks5 must be dropped");
        assert!(!addrs.iter().any(|a| a.starts_with("4.4.4.4")), "ssl:false must be dropped");
        assert!(!addrs.iter().any(|a| a.starts_with("5.5.5.5")), "alive:false must be dropped");
        assert_eq!(got.len(), 2);
    }

    #[test]
    fn scrape_json_orders_by_uptime_then_latency() {
        // Vetting costs one real request per candidate, so the order decides how
        // far down the list we have to walk. 100% uptime must come first.
        let got = parse_proxyscrape_json(SCRAPE_JSON);
        assert_eq!(got[0].addr(), "2.2.2.2:3128", "100% uptime should lead");
    }

    #[test]
    fn scrape_json_accepts_a_bare_array_and_string_ports() {
        let raw = br#"[{"ip":"9.9.9.9","port":"8080","protocol":"http","ssl":true,"alive":true}]"#;
        let got = parse_proxyscrape_json(raw);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].port, 8080);
    }

    #[test]
    fn scrape_json_survives_garbage() {
        // A truncated download or an HTML error page must yield nothing rather
        // than panicking inside a background task.
        assert!(parse_proxyscrape_json(b"not json").is_empty());
        assert!(parse_proxyscrape_json(b"").is_empty());
        assert!(parse_proxyscrape_json(br#"{"proxies":"wrong type"}"#).is_empty());
        assert!(parse_proxyscrape_json(br#"{"proxies":[{"ip":"1.2.3.4"}]}"#).is_empty());
    }

    #[test]
    fn scrape_json_rejects_out_of_range_ports() {
        let raw = br#"[{"ip":"1.2.3.4","port":70000,"protocol":"http","ssl":true,"alive":true},
                      {"ip":"1.2.3.5","port":0,"protocol":"http","ssl":true,"alive":true}]"#;
        assert!(parse_proxyscrape_json(raw).is_empty());
    }

    // ── eviction and refill ─────────────────────────────────────────────────

    #[test]
    fn cooldown_grows_with_the_failure_streak() {
        // A dead free proxy retried every 90s costs a connect timeout each time.
        let p = pool_with(1);
        let e = p.pick().unwrap();
        p.note_fail(&e.addr(), "refused");
        let first = p.health.lock().unwrap().get(&e.addr()).unwrap().cool_until;
        p.note_fail(&e.addr(), "refused");
        let second = p.health.lock().unwrap().get(&e.addr()).unwrap().cool_until;
        assert!(second > first, "backoff must grow ({first} -> {second})");
    }

    #[test]
    fn three_consecutive_failures_evicts() {
        let p = pool_with(2);
        let victim = p.pick().unwrap().addr();
        for _ in 0..EVICT_AFTER_FAILS {
            p.note_fail(&victim, "dead");
        }
        let removed = p.evict_dead();
        assert_eq!(removed, vec![victim.clone()]);
        assert_eq!(p.len(), 1, "pool must shrink");
        // And its health row goes too, so a re-add starts clean.
        assert!(p.health.lock().unwrap().get(&victim).is_none());
    }

    #[test]
    fn a_success_between_failures_prevents_eviction() {
        // Lifetime ratios hide intermittency; the streak is what matters.
        let p = pool_with(1);
        let a = p.pick().unwrap().addr();
        p.note_fail(&a, "blip");
        p.note_fail(&a, "blip");
        p.note_ok(&a, 100);
        p.note_fail(&a, "blip");
        assert!(p.evict_dead().is_empty(), "streak was broken by a success");
    }

    #[test]
    fn evicted_endpoints_are_never_picked_even_before_sweeping() {
        let p = pool_with(2);
        let victim = p.pick().unwrap().addr();
        for _ in 0..EVICT_AFTER_FAILS {
            p.note_fail(&victim, "dead");
        }
        // No evict_dead() call: pick must already refuse it.
        for _ in 0..6 {
            if let Some(g) = p.pick() {
                assert_ne!(g.addr(), victim);
            }
        }
    }

    #[test]
    fn add_endpoints_deduplicates() {
        // A duplicate would be picked twice as often, concentrating egress on one
        // IP — the exact thing the pool exists to avoid.
        let p = pool_with(2);
        let dup = ProxyEndpoint::parse("10.0.0.0:8000:u:p").unwrap();
        let fresh = ProxyEndpoint::parse("10.9.9.9:8000:u:p").unwrap();
        assert_eq!(p.add_endpoints(vec![dup, fresh]), 1);
        assert_eq!(p.len(), 3);
    }

    #[test]
    fn healthy_count_excludes_cooling_and_evicted() {
        let p = pool_with(4);
        let all: Vec<String> = (0..4).map(|i| format!("10.0.0.{i}:8000")).collect();
        p.note_fail(&all[0], "x"); // cooling
        for _ in 0..EVICT_AFTER_FAILS {
            p.note_fail(&all[1], "dead"); // evicted
        }
        assert_eq!(p.healthy_count(), 2, "only the untouched two are usable");
    }

    #[test]
    fn needs_refill_tracks_the_healthy_floor() {
        // The threshold is strict `<`, so exactly REFILL_BELOW healthy is still
        // fine and REFILL_BELOW - 1 is not. Pinning the boundary rather than a
        // vague "some failures" keeps an off-by-one from slipping in later.
        let p = pool_with(REFILL_BELOW);
        assert_eq!(p.healthy_count(), REFILL_BELOW);
        assert!(!p.needs_refill(), "exactly at the floor is still enough");
        p.note_fail("10.0.0.0:8000", "x");
        assert_eq!(p.healthy_count(), REFILL_BELOW - 1);
        assert!(p.needs_refill(), "one below the floor must request a refill");
        // A disabled pool never asks, or a user who turned proxies off would
        // still see background refetching.
        p.set_enabled(false);
        assert!(!p.needs_refill());
    }

    #[test]
    fn save_file_round_trips_and_orders_best_first() {
        let p = pool_with(3);
        p.note_ok("10.0.0.2:8000", 50);
        p.note_ok("10.0.0.2:8000", 50);
        p.note_ok("10.0.0.1:8000", 900);
        let f = std::env::temp_dir().join(format!("tabi-save-{}.txt", std::process::id()));
        let n = p.save_file(&f).unwrap();
        assert_eq!(n, 3);
        let text = std::fs::read_to_string(&f).unwrap();
        let first_entry = text
            .lines()
            .find(|l| !l.starts_with('#'))
            .unwrap();
        assert!(first_entry.starts_with("10.0.0.2:8000"), "most successes first, got {first_entry}");
        // Reloading must preserve the set.
        let p2 = ProxyPool::new();
        assert_eq!(p2.load_file(&f), 3);
        let _ = std::fs::remove_file(&f);
    }

    #[test]
    fn base64_matches_known_values() {
        assert_eq!(b64(b""), "");
        assert_eq!(b64(b"f"), "Zg==");
        assert_eq!(b64(b"fo"), "Zm8=");
        assert_eq!(b64(b"foo"), "Zm9v");
        assert_eq!(b64(b"user:pass"), "dXNlcjpwYXNz");
    }

    #[test]
    fn parses_the_webshare_colon_format() {
        // Same SHAPE as the live proxies.txt (host:port:user:pass) with dummy
        // values. Never put a real credential in a test — this file is public and
        // git history is forever.
        let e = ProxyEndpoint::parse("203.0.113.7:6754:exampleuser:examplepass").unwrap();
        assert_eq!(e.host, "203.0.113.7");
        assert_eq!(e.port, 6754);
        assert_eq!(e.user.as_deref(), Some("exampleuser"));
        assert_eq!(e.pass.as_deref(), Some("examplepass"));
    }

    #[test]
    fn a_file_that_parses_to_nothing_does_not_wipe_a_live_pool() {
        // The failure this guards: "Reload proxies.txt" set the pool to zero and
        // egress silently went direct. An empty parse is almost always an error,
        // so the previous pool is kept and the caller is told.
        let p = pool_with(5);
        let f = std::env::temp_dir().join(format!("tabi-bad-{}.txt", std::process::id()));
        std::fs::write(&f, "# only comments\n\n   \ngarbage line\n").unwrap();
        let r = p.load_file_detailed(&f);
        assert!(r.kept_existing, "must refuse the file");
        assert_eq!(r.loaded, 5, "pool must be intact");
        assert_eq!(p.len(), 5);
        assert_eq!(r.skipped, 1, "only the junk line counts as skipped");
        assert!(r.note.contains("keeping"));
        let _ = std::fs::remove_file(&f);
    }

    #[test]
    fn a_missing_file_keeps_the_pool() {
        let p = pool_with(3);
        let r = p.load_file_detailed(std::path::Path::new("/nonexistent/proxies.txt"));
        assert!(r.kept_existing);
        assert_eq!(p.len(), 3);
        assert!(r.note.contains("cannot read"));
    }

    #[test]
    fn an_empty_file_into_an_empty_pool_is_not_an_error() {
        // Cold start with no file yet: loading zero is the honest answer, and the
        // refusal must not fire because there is nothing to protect.
        let p = ProxyPool::new();
        let f = std::env::temp_dir().join(format!("tabi-empty-{}.txt", std::process::id()));
        std::fs::write(&f, "").unwrap();
        let r = p.load_file_detailed(&f);
        assert!(!r.kept_existing);
        assert_eq!(r.loaded, 0);
        let _ = std::fs::remove_file(&f);
    }

    #[test]
    fn reload_preserves_health_for_surviving_endpoints() {
        // The success record is what ranks the pool; a reload must not reset it.
        let p = pool_with(3);
        p.note_ok("10.0.0.1:8000", 100);
        p.note_ok("10.0.0.1:8000", 100);
        let f = std::env::temp_dir().join(format!("tabi-keep-{}.txt", std::process::id()));
        p.save_file(&f).unwrap();
        let r = p.load_file_detailed(&f);
        assert!(!r.kept_existing);
        assert_eq!(r.skipped, 0, "save_file output must fully parse");
        let ok = p
            .snapshot()
            .into_iter()
            .find(|(e, _)| e.addr() == "10.0.0.1:8000")
            .map(|(_, h)| h.ok)
            .unwrap();
        assert_eq!(ok, 2, "health must survive the reload");
        let _ = std::fs::remove_file(&f);
    }

    #[test]
    fn skipped_count_ignores_comments_and_blanks() {
        let p = ProxyPool::new();
        let f = std::env::temp_dir().join(format!("tabi-skip-{}.txt", std::process::id()));
        std::fs::write(&f, "# header\n\n10.0.0.1:8000\nnonsense\n  # indented\n").unwrap();
        let r = p.load_file_detailed(&f);
        assert_eq!(r.loaded, 1);
        assert_eq!(r.skipped, 1, "comments and blanks are not failures");
        let _ = std::fs::remove_file(&f);
    }

    #[test]
    fn parse_ignores_the_annotation_save_file_writes() {
        // The exact round-trip that was broken: save_file appends health stats as
        // a comment, and parse must read its own output back.
        let e = ProxyEndpoint::parse("65.1.240.131:3001  # ok=4 fail=0 490ms").unwrap();
        assert_eq!(e.host, "65.1.240.131");
        assert_eq!(e.port, 3001);
        assert!(e.user.is_none());

        let e = ProxyEndpoint::parse("1.2.3.4:8080:u:p  # ok=1 fail=0 120ms").unwrap();
        assert_eq!(e.port, 8080);
        assert_eq!(e.user.as_deref(), Some("u"));
        assert_eq!(e.pass.as_deref(), Some("p"));
    }

    #[test]
    fn save_file_output_reloads_to_the_same_pool() {
        // Property, not example: whatever save_file writes, load_file must accept.
        // This is the regression test for the pool emptying itself.
        let p = pool_with(4);
        p.note_ok("10.0.0.1:8000", 120);
        p.note_ok("10.0.0.2:8000", 90);
        p.note_fail("10.0.0.3:8000", "some error: with colons: in it");
        let f = std::env::temp_dir().join(format!("tabi-rt-{}.txt", std::process::id()));
        let saved = p.save_file(&f).unwrap();

        let p2 = ProxyPool::new();
        let loaded = p2.load_file(&f);
        assert_eq!(loaded, saved, "every saved line must parse back");
        assert_eq!(loaded, 4);

        // And the endpoints are the same set, not just the same count.
        let mut a: Vec<String> = p.snapshot().into_iter().map(|(e, _)| e.addr()).collect();
        let mut b: Vec<String> = p2.snapshot().into_iter().map(|(e, _)| e.addr()).collect();
        a.sort();
        b.sort();
        assert_eq!(a, b);
        let _ = std::fs::remove_file(&f);
    }

    #[test]
    fn parse_still_rejects_comment_only_and_blank_lines() {
        assert!(ProxyEndpoint::parse("# a comment").is_none());
        assert!(ProxyEndpoint::parse("   # indented comment").is_none());
        assert!(ProxyEndpoint::parse("").is_none());
        assert!(ProxyEndpoint::parse("   ").is_none());
    }

    #[test]
    fn parses_url_format_and_bare_hostport() {
        let e = ProxyEndpoint::parse("http://u:p@1.2.3.4:8080").unwrap();
        assert_eq!((e.host.as_str(), e.port), ("1.2.3.4", 8080));
        assert_eq!(e.user.as_deref(), Some("u"));

        let e = ProxyEndpoint::parse("10.0.0.1:3128").unwrap();
        assert_eq!(e.port, 3128);
        assert!(e.user.is_none());
    }

    #[test]
    fn ignores_comments_and_blank_lines() {
        assert!(ProxyEndpoint::parse("").is_none());
        assert!(ProxyEndpoint::parse("   ").is_none());
        assert!(ProxyEndpoint::parse("# a comment").is_none());
        assert!(ProxyEndpoint::parse("garbage").is_none());
    }

    #[test]
    fn auth_header_is_base64_of_user_colon_pass() {
        let e = ProxyEndpoint::parse("h:1:user:pass").unwrap();
        assert_eq!(e.auth_header().as_deref(), Some("dXNlcjpwYXNz"));
        let e = ProxyEndpoint::parse("h:1").unwrap();
        assert!(e.auth_header().is_none());
    }

    fn pool_with(n: usize) -> ProxyPool {
        let p = ProxyPool::new();
        let list: Vec<ProxyEndpoint> = (0..n)
            .map(|i| ProxyEndpoint::parse(&format!("10.0.0.{i}:8000:u:p")).unwrap())
            .collect();
        {
            let mut h = p.health.lock().unwrap();
            for e in &list {
                h.entry(e.addr()).or_default();
            }
        }
        *p.endpoints.lock().unwrap() = list;
        p.set_enabled(true);
        p
    }

    #[test]
    fn disabled_pool_always_goes_direct() {
        let p = pool_with(3);
        p.set_enabled(false);
        assert!(p.pick().is_none());
    }

    #[test]
    fn rotation_is_least_recently_used() {
        let p = pool_with(3);
        let a = p.pick().unwrap();
        let b = p.pick().unwrap();
        let c = p.pick().unwrap();
        // Three distinct proxies before any repeat: load must spread.
        let mut seen = vec![a.addr(), b.addr(), c.addr()];
        seen.sort();
        seen.dedup();
        assert_eq!(seen.len(), 3, "LRU must not reuse a proxy early");
    }

    #[test]
    fn rapid_picks_within_one_second_still_rotate() {
        // The bug this pins. `pick()` used second-resolution timestamps, so
        // several sessions starting inside the same second read identical
        // `last_used` values, tie-broke by iteration order, and every one of them
        // received the SAME proxy. Seven concurrent sessions egressed from one IP
        // instead of seven — and because upstream rate limits are per-IP as well
        // as per-account, ten separate accounts were rate-limited together.
        //
        // This loop completes far inside one second, which is exactly the
        // condition that used to fail.
        let p = pool_with(10);
        let picks: Vec<String> = (0..10).map(|_| p.pick().unwrap().addr()).collect();

        let mut unique = picks.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(
            unique.len(),
            10,
            "10 rapid picks over 10 proxies must use each exactly once, got {picks:?}"
        );
    }

    #[test]
    fn lru_wraps_around_in_order() {
        // After exhausting the pool, the next pick must return to the endpoint
        // used longest ago rather than restarting at whichever the iterator
        // happens to reach first.
        let p = pool_with(3);
        let first = p.pick().unwrap().addr();
        let _ = p.pick().unwrap();
        let _ = p.pick().unwrap();
        assert_eq!(
            p.pick().unwrap().addr(),
            first,
            "the fourth pick must recycle the oldest endpoint"
        );
    }

    #[test]
    fn a_failed_proxy_is_cooled_and_skipped() {
        let p = pool_with(2);
        let first = p.pick().unwrap();
        p.note_fail(&first.addr(), "connection refused");
        for _ in 0..4 {
            let got = p.pick();
            if let Some(g) = got {
                assert_ne!(g.addr(), first.addr(), "cooling proxy must be skipped");
            }
        }
    }

    #[test]
    fn all_cooling_falls_back_to_direct_not_failure() {
        // Availability beats IP hygiene: a request must still go out.
        let p = pool_with(2);
        for _ in 0..2 {
            if let Some(e) = p.pick() {
                p.note_fail(&e.addr(), "dead");
            }
        }
        assert!(p.pick().is_none(), "should signal direct");
        let (_, direct, cooling) = p.stats();
        assert!(direct >= 1, "direct fallback must be counted");
        assert_eq!(cooling, 2);
    }

    #[test]
    fn success_clears_the_cooldown() {
        let p = pool_with(1);
        let e = p.pick().unwrap();
        p.note_fail(&e.addr(), "blip");
        assert!(p.pick().is_none());
        p.note_ok(&e.addr(), 120);
        assert!(p.pick().is_some(), "a healthy proxy must return to service");
    }

    #[test]
    fn loading_a_file_parses_and_registers_health() {
        use std::io::Write;
        let f = std::env::temp_dir().join(format!("tabi-proxies-{}.txt", std::process::id()));
        {
            let mut fh = std::fs::File::create(&f).unwrap();
            writeln!(fh, "# comment").unwrap();
            writeln!(fh, "10.0.0.1:8000:u:p").unwrap();
            writeln!(fh, "http://a:b@10.0.0.2:9000").unwrap();
            writeln!(fh, "not-a-proxy").unwrap();
        }
        let p = ProxyPool::new();
        assert_eq!(p.load_file(&f), 2, "comments and junk must be ignored");
        p.set_enabled(true);
        assert!(p.pick().is_some());
        let _ = std::fs::remove_file(&f);
    }
}
