//! Configuration: providers, tunables, paths.
//!
//! Everything here is data-driven — no hardcoded keys. Key material lives only
//! in the key files under `~/git_gorouter_keyforge-token/` and is loaded at runtime.

use std::path::PathBuf;

/// A browser User-Agent is REQUIRED. All three upstreams sit behind Cloudflare
/// and return an HTML 403 for non-browser agents. Verified by direct probe.
pub const BROWSER_UA: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) \
     Chrome/131.0.0.0 Safari/537.36";

pub const DEFAULT_PORT: u16 = 8787;
pub const BIND_ADDR: [u8; 4] = [127, 0, 0, 1]; // loopback only, never network-exposed

/// Timeout waiting for the response HEAD on a STREAMING request.
///
/// Adaptive streaming head budget (see `App::head_budget_secs`).
///
/// A fixed timeout is always wrong eventually: TTFB here ranged from ~2s on
/// small contexts to ~99s on a 1M-token one. So the budget has three terms:
///
///   estimate = ewma_ttfb * HEAD_BUDGET_MULT
///            + (HEAD_BUDGET_SECS_PER_MB * req_MB + HEAD_BUDGET_OVERHEAD_SECS)
///              * HEAD_BUDGET_SIZE_SAFETY
///   budget   = clamp(estimate, HEAD_BUDGET_MIN, HEAD_BUDGET_MAX)
///
/// The size term exists because the EWMA alone is **blind to request size**, and
/// that blindness was a real, total outage for large contexts. There is one TTFB
/// EWMA per provider, trained on whatever traffic dominates (small turns, free
/// `/v1/models` probes), so a 281KB request and a 2.7MB request were handed the
/// identical budget. The 1M case was not slow, it was *arithmetically
/// impossible*: the budget could never reach the requirement, on any key, on any
/// provider, forever. Worse, only successes feed the EWMA, so those timeouts
/// could never teach it either.
///
/// ── Calibration, measured 2026-09-04 ─────────────────────────────────────────
///
/// Direct to gorouter.app, real key, browser UA, streaming, one proxy:
///
///   281,305 B (~100k tok)   -> 200 OK, TTFB 11.9s
///   952,057 B (~340k tok)   -> 200 OK, TTFB 35.5s
///   2,702,917 B (~965k tok) -> 200 OK, TTFB 95.3s, then 99.4s on repeat
///
/// Fit through the two small points: `ttfb ~= 2.0s + 35.2s/MB`. It predicts
/// 97.0s at 2.70MB against 95.3/99.4 observed — within 2s, hence SECS_PER_MB.
///
/// An earlier revision used 20.3 s/MB. That came from a fit whose large point
/// was a *failure*, not a measurement, and it understated the slope by 42%. It
/// appeared to work only because SIZE_SAFETY was 2.0 and absorbed the error; the
/// documented purpose of the env override is tuning, so a correct slope with a
/// modest safety factor is not interchangeable with a wrong slope and a large
/// one.
///
/// OVERHEAD is the gateway's own cost, measured by paired A/B on one 952KB
/// payload with both arms pinned to gorouter, alternating so upstream drift hit
/// both equally:
///
///   DIRECT  40.2s  47.1s  41.5s   mean 42.9s, spread  7s
///   GATEWAY 53.8s  43.1s  70.6s   mean 55.8s, spread 28s   -> +12.9s
///
/// It is a constant, not a multiplier: `router.rs` collects the whole body
/// before opening the upstream connection and clones it per key attempt, so the
/// cost tracks body size but not provider speed. Folding it into the per-MB term
/// would double-count it against the EWMA base.
///
/// The bounds are guardrails, not guesses:
/// - MIN exists so a fast provider fails over quickly instead of hanging.
/// - MAX must clear the measured worst case: ~97s direct plus ~13s gateway
///   overhead is ~110s typical, with an observed tail near 140s. The previous
///   130s ceiling clipped requests that would otherwise have succeeded — a 1M
///   body computed *to* the clamp and only completed because failover to a
///   second provider bought extra wall-clock. 170s clears the tail while
///   staying near Cloudflare's edge, which answers for the origin at ~128-140s;
///   waiting far beyond that is dead air.
/// - Unmeasured providers get HEAD_BUDGET_DEFAULT as the first term.
///
/// All seven are overridable at startup via `KEYFORGE_HEAD_MULT`,
/// `KEYFORGE_HEAD_MIN_SECS`, `KEYFORGE_HEAD_MAX_SECS`, `KEYFORGE_HEAD_DEFAULT_SECS`,
/// `KEYFORGE_HEAD_SECS_PER_MB`, `KEYFORGE_HEAD_OVERHEAD_SECS`, `KEYFORGE_HEAD_SIZE_SAFETY`
/// — no rebuild needed.
pub const HEAD_BUDGET_MULT: f64 = 3.0;
pub const HEAD_BUDGET_MIN_SECS: u64 = 45;
pub const HEAD_BUDGET_MAX_SECS: u64 = 170;
pub const HEAD_BUDGET_DEFAULT_SECS: u64 = 120;
pub const HEAD_BUDGET_SECS_PER_MB: f64 = 35.2;
pub const HEAD_BUDGET_OVERHEAD_SECS: f64 = 15.0;
pub const HEAD_BUDGET_SIZE_SAFETY: f64 = 1.35;

pub fn env_u64(name: &str, dflt: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(dflt)
}

pub fn env_f64(name: &str, dflt: f64) -> f64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(dflt)
}

/// Multiplier applied to measured TTFB EWMA. 3x absorbs normal variance
/// without mistaking a slow start for a stuck one.
pub fn head_mult() -> f64 {
    env_f64("KEYFORGE_HEAD_MULT", HEAD_BUDGET_MULT)
}

/// Floor: a fast provider fails over fast instead of hanging to the ceiling.
pub fn head_min_secs() -> u64 {
    env_u64("KEYFORGE_HEAD_MIN_SECS", HEAD_BUDGET_MIN_SECS)
}

/// Ceiling: past Cloudflare's edge timeout, waiting is dead air.
pub fn head_max_secs() -> u64 {
    env_u64("KEYFORGE_HEAD_MAX_SECS", HEAD_BUDGET_MAX_SECS)
}

/// Budget for a provider with no TTFB samples yet. Generous on purpose —
/// killing a first-seen provider on a tight budget would punish exploration.
pub fn head_default_secs() -> u64 {
    env_u64("KEYFORGE_HEAD_DEFAULT_SECS", HEAD_BUDGET_DEFAULT_SECS)
}

/// Seconds of prefill+upload per MB of request body. Measured on this device
/// (see the fit above), NOT guessed. Raise it if the device's link degrades.
pub fn head_secs_per_mb() -> f64 {
    env_f64("KEYFORGE_HEAD_SECS_PER_MB", HEAD_BUDGET_SECS_PER_MB)
}

/// The gateway's own overhead: buffer-the-body-then-forward, plus a per-attempt
/// clone of it. Measured at +12.9s mean on a 952KB payload; 15s carries a little
/// margin. Constant rather than per-MB because it does not scale with provider
/// speed. Streaming the body through instead of collecting it would largely
/// remove this, at which point lower the value — do not treat it as fixed.
pub fn head_overhead_secs() -> f64 {
    env_f64("KEYFORGE_HEAD_OVERHEAD_SECS", HEAD_BUDGET_OVERHEAD_SECS)
}

/// Safety factor on the size term. 1.35x, not 2x: the slope it multiplies is now
/// measured against three payload sizes including two completed 1M runs, so the
/// factor covers variance rather than compensating for a wrong slope.
pub fn head_size_safety() -> f64 {
    env_f64("KEYFORGE_HEAD_SIZE_SAFETY", HEAD_BUDGET_SIZE_SAFETY)
}

/// Key validation: prefix and minimum length. A key is only accepted if it
/// starts with KEY_PREFIX and is at least KEY_MIN_LEN characters long.
pub const KEY_PREFIX: &str = "sk-";
pub const KEY_MIN_LEN: usize = 20;

pub fn key_prefix() -> String {
    std::env::var("KEYFORGE_KEY_PREFIX").unwrap_or_else(|_| KEY_PREFIX.into())
}

pub fn key_min_len() -> usize {
    env_u64("KEYFORGE_KEY_MIN_LEN", KEY_MIN_LEN as u64) as usize
}
///
/// This is a provider-wide outage with a healthy host: new-api answered, the
/// site was up, the keys and balances were fine, but no backend existed for the
/// model. Because the answer is identical for every sibling key, the correct
/// response is to stop asking for a while — not to rotate keys.
///
/// 45s is chosen so a channel that comes back is picked up quickly, while a
/// sustained outage cannot generate more than ~1 attempt per provider per 45s.
/// On 2026-09-04 the old behaviour produced ~57 attempts of 282-540KB in 8
/// minutes across both providers and drew a Cloudflare 429 rate-limit strike.
pub const NO_CHANNEL_COOLDOWN_SECS: u64 = 45;

pub fn no_channel_cooldown_secs() -> u64 {
    env_u64("KEYFORGE_NO_CHANNEL_COOLDOWN_SECS", NO_CHANNEL_COOLDOWN_SECS)
}

/// Timeout for a NON-streaming request, where the head only arrives once the
/// whole answer has been generated — legitimately minutes for a long completion.
pub const UPSTREAM_TIMEOUT_SECS: u64 = 300;

pub fn upstream_timeout_secs() -> u64 {
    env_u64("KEYFORGE_UPSTREAM_TIMEOUT_SECS", UPSTREAM_TIMEOUT_SECS)
}

/// Context/output limits declared for the gateway models in `opencode.jsonc`.
///
/// OpenCode triggers auto-compaction against a model's *declared* context limit.
/// With no `limit` block it has no threshold, so a session grows unbounded: the
/// 1M session reached 1968 messages / 5408 parts / 6.2MB of stored parts and had
/// never compacted since moving to the gateway, while a sibling provider that
/// did declare `limit` compacted normally.
///
/// Declared AT ~1M rather than lower on purpose: the goal is to *keep* a large
/// working context (one 980,604-input-token turn did complete via tabi), not to
/// compact it away. This only stops growth past what the providers can serve.
pub const OPENCODE_CONTEXT_LIMIT: u64 = 1_000_000;
pub const OPENCODE_OUTPUT_LIMIT: u64 = 65_535;

/// Hard wall-clock ceiling for one client request, across every retry.
///
/// Must sit BELOW the client's own timeout (the Anthropic SDK and Claude Code
/// both default to ~600s). Previously the ladder allowed 600 rounds x ~15s =
/// up to 2.5 hours, so the client gave up long before the gateway did and the
/// user simply saw a hang. Better to return a readable terminal error in time.
pub const REQUEST_DEADLINE_SECS: u64 = 480;

pub fn request_deadline_secs() -> u64 {
    env_u64("KEYFORGE_REQUEST_DEADLINE_SECS", REQUEST_DEADLINE_SECS)
}

/// Cap on a buffered body. Protects against OOM on a memory-constrained tablet
/// when an upstream (or a local caller) sends something enormous.
pub const MAX_BODY_BYTES: usize = 32 * 1024 * 1024;

pub fn max_body_bytes() -> usize {
    env_u64("KEYFORGE_MAX_BODY_BYTES", MAX_BODY_BYTES as u64) as usize
}

/// Free `/v1/models` probe timeout (uptime checks + key verification).
pub const PROBE_TIMEOUT_SECS: u64 = 12;

pub fn probe_timeout_secs() -> u64 {
    env_u64("KEYFORGE_PROBE_TIMEOUT_SECS", PROBE_TIMEOUT_SECS)
}

/// How many keys to try on one upstream before failing over to its sibling.
pub const MAX_KEY_ATTEMPTS: usize = 6;

pub fn max_key_attempts() -> usize {
    env_u64("KEYFORGE_MAX_KEY_ATTEMPTS", MAX_KEY_ATTEMPTS as u64) as usize
}

/// A key must hold at least `hold * this` to be handed to a NEW session, so a
/// fresh conversation does not start on a key that dies mid-flow.
pub const HOLD_SAFETY_FACTOR: f64 = 2.5;

pub fn hold_safety_factor() -> f64 {
    env_f64("KEYFORGE_HOLD_SAFETY_FACTOR", HOLD_SAFETY_FACTOR)
}

/// Cooldowns. Quota is short because balances get topped up; auth is long
/// because an invalid token is effectively permanent.
pub const COOLDOWN_QUOTA_SECS: u64 = 6 * 3600;
pub const COOLDOWN_AUTH_SECS: u64 = 7 * 86_400;
pub const COOLDOWN_RATE_SECS: u64 = 60;

pub fn cooldown_quota_secs() -> u64 {
    env_u64("KEYFORGE_COOLDOWN_QUOTA_SECS", COOLDOWN_QUOTA_SECS)
}

pub fn cooldown_auth_secs() -> u64 {
    env_u64("KEYFORGE_COOLDOWN_AUTH_SECS", COOLDOWN_AUTH_SECS)
}

pub fn cooldown_rate_secs() -> u64 {
    env_u64("KEYFORGE_COOLDOWN_RATE_SECS", COOLDOWN_RATE_SECS)
}

/// A session is "active" (and owns its key exclusively) for this long after its
/// last request. Lets parallel agents get their own keys.
pub const SESSION_ACTIVE_SECS: u64 = 45 * 60;

pub fn session_active_secs() -> u64 {
    env_u64("KEYFORGE_SESSION_ACTIVE_SECS", SESSION_ACTIVE_SECS)
}

/// Balance data older than this is re-probed before the key is handed out.
pub const USAGE_STALE_SECS: u64 = 20 * 60;

pub fn usage_stale_secs() -> u64 {
    env_u64("KEYFORGE_USAGE_STALE_SECS", USAGE_STALE_SECS)
}

// ── Battery budget ───────────────────────────────────────────────────────────
// Target: <=5%/hour on a OnePlus Pad Go. Everything below is deliberately slow.
// The dashboard is push-based (SSE) so an idle dashboard costs ~0 CPU.

/// Uptime probe interval. Uses free `/v1/models`, never spends credits.
///
/// 2 minutes: frequent enough that a provider outage shows on the dashboard
/// almost immediately, cheap enough to be invisible on battery (a probe is one
/// small TLS request per provider, and connections are pooled).
pub const UPTIME_PROBE_SECS: u64 = 120;

pub fn uptime_probe_secs() -> u64 {
    env_u64("KEYFORGE_UPTIME_PROBE_SECS", UPTIME_PROBE_SECS)
}

/// When a probe fails, retry this soon instead of waiting a full interval — so a
/// brief blip is not drawn as a two-minute outage.
pub const UPTIME_RETRY_SECS: u64 = 20;

pub fn uptime_retry_secs() -> u64 {
    env_u64("KEYFORGE_UPTIME_RETRY_SECS", UPTIME_RETRY_SECS)
}

/// Consecutive failed probes before a provider is drawn as DOWN.
///
/// Without this, one dropped packet paints a red bar. Two failures ~20s apart is
/// a real outage; one is noise.
pub const UPTIME_FAIL_THRESHOLD: u32 = 2;

pub fn uptime_fail_threshold() -> u32 {
    env_u64("KEYFORGE_UPTIME_FAIL_THRESHOLD", UPTIME_FAIL_THRESHOLD as u64) as u32
}

/// Wi-Fi link sampling interval (see `link.rs`).
///
/// 60s is a compromise: `dumpsys wifi` spawns a JVM and emits ~200KB, so it is
/// far too costly for the request path, but link quality changes on the scale of
/// walking across a room. Sampling faster would buy no useful precision and
/// would show up in the 5%/hour battery target.
pub const LINK_POLL_SECS: u64 = 60;

pub fn link_poll_secs() -> u64 {
    env_u64("KEYFORGE_LINK_POLL_SECS", LINK_POLL_SECS)
}

/// Full key-pool balance sweep interval.
pub const SWEEP_INTERVAL_SECS: u64 = 30 * 60;

pub fn sweep_interval_secs() -> u64 {
    env_u64("KEYFORGE_SWEEP_INTERVAL_SECS", SWEEP_INTERVAL_SECS)
}

/// Keys probed per sweep, and concurrency. Gentle to avoid tripping Cloudflare
/// and to keep the radio from staying hot.
pub const SWEEP_BATCH: usize = 120;
pub const SWEEP_CONCURRENCY: usize = 4;
pub const SWEEP_PACING_MS: u64 = 120;

pub fn sweep_batch() -> usize {
    env_u64("KEYFORGE_SWEEP_BATCH", SWEEP_BATCH as u64) as usize
}

pub fn sweep_concurrency() -> usize {
    env_u64("KEYFORGE_SWEEP_CONCURRENCY", SWEEP_CONCURRENCY as u64) as usize
}

pub fn sweep_pacing_ms() -> u64 {
    env_u64("KEYFORGE_SWEEP_PACING_MS", SWEEP_PACING_MS)
}

/// GitHub key-sync helper interval (4h, as requested).
pub const KEYSYNC_INTERVAL_SECS: u64 = 4 * 3600;

pub fn keysync_interval_secs() -> u64 {
    env_u64("KEYFORGE_KEYSYNC_INTERVAL_SECS", KEYSYNC_INTERVAL_SECS)
}

/// Snapshot state to disk at most this often (coalesced writes save I/O).
pub const STATE_FLUSH_SECS: u64 = 20;

pub fn state_flush_secs() -> u64 {
    env_u64("KEYFORGE_STATE_FLUSH_SECS", STATE_FLUSH_SECS)
}

/// Rolling history retained for the dashboard charts.
///
/// Three resolutions, because one is always wrong. A 1-minute series is the only
/// thing that shows a burst, but keeping months of it would be megabytes of
/// state and an unusable chart; a daily series shows a trend but hides the
/// burst. So every request bumps all three buckets and the dashboard asks for
/// the resolution that matches the range being viewed.
pub const HISTORY_MINUTES: usize = 360; // 6h of 1-min buckets
pub const HISTORY_HOURS: usize = 720; // 30d of 1-hour buckets
pub const HISTORY_DAYS: usize = 730; // 2y of 1-day buckets

/// 24h of uptime samples at the 2-minute cadence.
pub const UPTIME_SAMPLES: usize = 720;

// ── snapshot slimming ───────────────────────────────────────────────────────
//
// The SSE snapshot is re-serialised and re-parsed on EVERY state change, so its
// size is felt as UI latency, not just bandwidth. Measured before this: 161KB
// per push, of which 88KB (55%) was three full 700-sample uptime arrays that the
// chart draws ~120 bars from anyway.
//
// So the snapshot carries only what is on screen, and the full series live
// behind `/api/history` and `/api/uptime`, fetched on demand when a wider range
// is actually selected.

/// Uptime samples included per provider in the snapshot.
pub const SNAPSHOT_UPTIME_SAMPLES: usize = 120;
/// Minute buckets included in the snapshot (the live chart's default window).
pub const SNAPSHOT_MINUTES: usize = 120;
/// Error records included in the snapshot. The Errors tab pages via /api/errors.
pub const SNAPSHOT_ERRORS: usize = 40;
/// Sessions included in the snapshot, newest first.
pub const SNAPSHOT_SESSIONS: usize = 24;

/// Resolution of a history query.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HistoryRes {
    Minute,
    Hour,
    Day,
}

impl HistoryRes {
    /// Parse the `res=` query parameter. Unknown values fall back to Minute
    /// rather than erroring: a chart with the wrong x-scale is far better than a
    /// chart with no data.
    pub fn parse(s: &str) -> Self {
        match s {
            "hour" | "hours" | "h" => Self::Hour,
            "day" | "days" | "d" => Self::Day,
            _ => Self::Minute,
        }
    }

    /// Seconds per bucket — also the multiplier that turns a bucket index back
    /// into a unix timestamp.
    pub fn secs(self) -> u64 {
        match self {
            Self::Minute => 60,
            Self::Hour => 3600,
            Self::Day => 86400,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Minute => "minute",
            Self::Hour => "hour",
            Self::Day => "day",
        }
    }
}

// ── proxy pool maintenance ──────────────────────────────────────────────────
//
// Free proxies decay within hours: of 100 candidates vetted from one
// ProxyScrape download, 53 passed a real provider request, and that fraction
// falls steadily over a day. So the pool cannot be a static file — it needs a
// maintainer that vets, evicts and refills on its own.

/// Where the vetted pool lives. Overridable with `KEYFORGE_PROXIES`.
pub fn proxies_path() -> std::path::PathBuf {
    std::env::var("KEYFORGE_PROXIES")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| home().join("git_gorouter_keyforge-token/proxies.txt"))
}

/// Candidate list downloaded from ProxyScrape, or dropped in by hand.
///
/// Checked in order; the first that exists is used. The Downloads location is
/// first because that is where a browser download lands.
pub fn proxy_candidates_paths() -> Vec<std::path::PathBuf> {
    if let Ok(v) = std::env::var("KEYFORGE_PROXY_CANDIDATES") {
        return vec![std::path::PathBuf::from(v)];
    }
    vec![
        home().join("downloads/free-proxy-list.json"),
        home().join("Download/free-proxy-list.json"),
        std::path::PathBuf::from("/sdcard/Download/free-proxy-list.json"),
        home().join("tmp/free-proxy-list.json"),
    ]
}

/// How often the maintainer re-checks the pool.
///
/// 15 minutes: frequent enough that a pool decaying to nothing is caught before
/// it starts failing requests, cheap enough to be invisible — a cycle is a
/// handful of free `/v1/models` calls.
pub const PROXY_MAINT_SECS: u64 = 900;

/// Candidates vetted per refill cycle.
///
/// Each is one real request, run with limited concurrency, so this bounds the
/// cost of a cycle. 40 yielded ~20 working endpoints in live testing.
pub const PROXY_VET_BATCH: usize = 40;

pub fn proxy_vet_batch() -> usize {
    env_u64("KEYFORGE_PROXY_VET_BATCH", PROXY_VET_BATCH as u64) as usize
}

/// Concurrent vet attempts. Kept low deliberately: a burst of parallel
/// connections from one device to one provider is the pattern that draws
/// Cloudflare's attention, which is the thing proxies exist to avoid.
pub const PROXY_VET_CONCURRENCY: usize = 6;

pub fn proxy_vet_concurrency() -> usize {
    env_u64("KEYFORGE_PROXY_VET_CONCURRENCY", PROXY_VET_CONCURRENCY as u64) as usize
}

/// Per-attempt timeout when vetting. Free proxies are slow; anything past this
/// is not worth keeping even if it eventually answers.
pub const PROXY_VET_TIMEOUT_SECS: u64 = 12;

pub fn proxy_vet_timeout_secs() -> u64 {
    env_u64("KEYFORGE_PROXY_VET_TIMEOUT_SECS", PROXY_VET_TIMEOUT_SECS)
}

pub fn proxy_maint_secs() -> u64 {
    env_u64("KEYFORGE_PROXY_MAINT_SECS", PROXY_MAINT_SECS)
}

/// Which API surface a request is using. Both are supported for every provider.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ApiFlavor {
    /// `/v1/messages` — Anthropic Messages API (Claude Code, `x-api-key`).
    Anthropic,
    /// `/v1/chat/completions` — OpenAI-compatible (OpenCode, `Authorization`).
    OpenAi,
    /// Anything else (`/v1/models`, etc.) — forwarded verbatim.
    Passthrough,
}

#[derive(Clone)]
pub struct Provider {
    /// Stable id used in routes, state and the dashboard.
    pub id: &'static str,
    /// Human label for the dashboard.
    pub label: &'static str,
    pub host: &'static str,
    /// Flat pre-deduction hold in USD. VERIFIED flat: max_tokens 1 / 4096 /
    /// 64000 all demanded exactly the same amount, so this is not a price
    /// estimate — it is the reservation the upstream takes per request.
    pub hold: f64,
    /// Assumed starting balance for an unseen key (conservative low end).
    /// Self-corrects to an exact value the first time a quota 403 reports the
    /// real remaining balance.
    pub initial_guess: f64,
    /// Key file, one `sk-...` per line.
    pub keys_file: &'static str,
    /// Models this provider claims to serve. Declared in providers.json; the
    /// probed `models_seen` list supplements this. Empty = "unknown, may serve
    /// anything" and routing will not demote it for a missing model.
    pub models: Vec<String>,
    /// Client-asked model → upstream model rename. Applied to the request body
    /// before sending, so a client asking for `claude-opus-5` can land on a
    /// provider whose channel names it something else.
    pub model_map: std::collections::HashMap<String, String>,
}

/// Build the live provider list from the settings file. Everything about a
/// provider — id, label, hosts, hold, keys file, declared models, model
/// renames — is editable from the dashboard and hot-reloads; the only piece
/// still compiled in is this empty fallback for when the settings file itself
/// cannot be read at all.
pub fn providers_from_settings(s: &crate::settings::Settings) -> Vec<Provider> {
    s.providers
        .iter()
        .filter(|p| p.enabled)
        .filter_map(|p| {
            let host = p.hosts.iter().find(|h| h.enabled).map(|h| h.host.as_str())?;
            Some(Provider {
                id: Box::leak(p.id.clone().into_boxed_str()),
                label: Box::leak(p.label.clone().into_boxed_str()),
                host: Box::leak(host.to_string().into_boxed_str()),
                hold: p.hold,
                initial_guess: p.initial_guess,
                keys_file: Box::leak(p.keys_file.clone().into_boxed_str()),
                models: p.models.clone(),
                model_map: p.model_map.clone(),
            })
        })
        .collect()
}

pub fn providers() -> Vec<Provider> {
    let (s, _) = crate::settings::Settings::load_or_init();
    providers_from_settings(&s)
}

pub fn home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/data/data/com.termux/files/home"))
}

/// State file location. Honours `KEYFORGE_STATE` so a test instance can run against
/// a scratch file without touching the live pool.
pub fn state_path() -> PathBuf {
    if let Ok(p) = std::env::var("KEYFORGE_STATE") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    home().join(".config/tabi/gateway-state.json")
}

pub fn keys_path(rel: &str) -> PathBuf {
    home().join(rel)
}

pub fn port() -> u16 {
    std::env::var("KEYFORGE_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_PORT)
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// How long we wait for the CLIENT to finish uploading its request body.
///
/// This is the "slow internet" side of timeouts, and it must not be confused
/// with a slow provider. A big context on a bad connection can legitimately
/// take a while to arrive; killing it early would blame the provider for the
/// user's wifi. Generous on purpose — body size is capped separately.
pub const CLIENT_BODY_TIMEOUT_SECS: u64 = 180;

/// Every N recorded failures, run the failure analysis that refreshes routing
/// estimates (see `App::analyze_failures`). 200 is small enough to adapt within
/// a bad day, large enough that the statistics mean something.
pub const ANALYSIS_EVERY_FAILURES: u64 = 200;
