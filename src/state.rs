//! Persistent state: key pool, session affinity, metrics history.
//!
//! Everything the dashboard shows and every decision the router makes lives
//! here. The whole struct is snapshotted to JSON so a restart loses nothing —
//! balances, which session was on which key, lifetime spend, uptime history.
//!
//! Locking rule: this is a `std::sync::Mutex` and the lock is NEVER held across
//! an `.await`. Callers take a snapshot, release, do I/O, then re-lock to
//! commit. Holding it across await would deadlock the single-threaded paths.

use crate::config::{self, Provider};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, MutexGuard};

// ── per-key ─────────────────────────────────────────────────────────────────

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct KeyState {
    pub provider: String,
    /// `total_usage` from the FREE billing endpoint, in cents spent.
    pub usage_cents: Option<f64>,
    /// Starting balance. Begins as the provider's conservative guess and
    /// becomes exact the first time a quota 403 reports the real remainder.
    pub initial: f64,
    pub initial_exact: bool,
    /// Derived: `initial - usage_cents/100`. `None` until first probe.
    pub balance: Option<f64>,
    /// Unix seconds until which this key must not be used.
    pub dead_until: u64,
    pub reason: String,
    pub last_used: u64,
    pub last_probe: u64,
    /// Successful requests served.
    pub turns: u64,
    /// Sum of `usage.cost` actually billed on this key.
    pub spent: f64,
    /// Live in-flight count — runtime only, never persisted.
    #[serde(skip)]
    pub inflight: u32,
}

impl KeyState {
    fn new(provider: &str, initial: f64) -> Self {
        Self {
            provider: provider.to_string(),
            usage_cents: None,
            initial,
            initial_exact: false,
            balance: None,
            dead_until: 0,
            reason: String::new(),
            last_used: 0,
            last_probe: 0,
            turns: 0,
            spent: 0.0,
            inflight: 0,
        }
    }

    pub fn recompute(&mut self) {
        if let Some(u) = self.usage_cents {
            self.balance = Some((self.initial - u / 100.0).max(0.0));
        }
    }

    /// Usable right now for `hold`?
    ///
    /// An unprobed key returns `true` optimistically — the selector probes it
    /// (free) before actually handing it out.
    pub fn usable(&self, now: u64, hold: f64) -> bool {
        if self.dead_until > now {
            return false;
        }
        match self.balance {
            Some(b) => b >= hold,
            None => true,
        }
    }
}

// ── per-session ─────────────────────────────────────────────────────────────

#[derive(Clone, Serialize, Deserialize, Debug, Default)]
pub struct SessionState {
    pub key: String,
    pub provider: String,
    pub created: u64,
    pub last_seen: u64,
    pub turns: u64,
    pub cost: f64,
    pub in_tokens: u64,
    pub out_tokens: u64,
    pub bytes_up: u64,
    pub bytes_down: u64,
    /// model &id -> request count
    pub models: HashMap<String, u64>,
    /// Friendly label if the client told us (OpenCode session &id, agent name).
    pub label: String,
    /// Recent per-message hashes, used to re-identify this session after the
    /// client compacts its history (see `session.rs`). Bounded.
    #[serde(default)]
    pub msg_hashes: Vec<u64>,
    /// How many times this session has been observed to compact.
    #[serde(default)]
    pub compactions: u64,
    /// How the session was last &identified: header | first-message |
    /// overlap-after-compaction | overlap | new | anonymous.
    #[serde(default)]
    pub via: String,
    /// Peak latency seen on this session, useful for spotting a slow provider.
    #[serde(default)]
    pub max_latency_ms: u64,
    /// Requests that failed within this session (before an internal retry).
    #[serde(default)]
    pub errors: u64,
    /// How many times this session had to move to a different key.
    #[serde(default)]
    pub key_switches: u64,
    /// Which tool this session came from: claude-code | opencode | cursor |
    /// curl | anthropic-api | unknown …  Detected per request in `session.rs`.
    #[serde(default)]
    pub client: String,
    /// Display label for `client`.
    #[serde(default)]
    pub client_label: String,
    /// Which API surface it used: anthropic | openai.
    #[serde(default)]
    pub api: String,
}

// ── metrics ─────────────────────────────────────────────────────────────────

#[derive(Clone, Serialize, Deserialize, Debug, Default)]
pub struct MinuteBucket {
    /// Unix minute (secs / 60).
    pub t: u64,
    pub requests: u64,
    pub errors: u64,
    pub bytes_up: u64,
    pub bytes_down: u64,
    pub cost: f64,
    pub latency_sum_ms: u64,
    pub latency_n: u64,
}

#[derive(Clone, Serialize, Deserialize, Debug, Default)]
pub struct ModelStat {
    pub requests: u64,
    pub errors: u64,
    pub cost: f64,
    pub in_tokens: u64,
    pub out_tokens: u64,
    pub latency_sum_ms: u64,
    pub latency_n: u64,
    /// provider &id -> count, so the dashboard can show a combined model list
    /// that also reveals which provider served it.
    pub by_provider: HashMap<String, u64>,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct UptimeSample {
    pub t: u64,
    pub up: bool,
    pub ms: u32,
}

#[derive(Clone, Serialize, Deserialize, Debug, Default)]
pub struct ProviderRuntime {
    /// Exponentially weighted mean FULL request duration. Informational only.
    ///
    /// NOT used for routing: it includes model thinking time, so a 55s Opus
    /// reasoning turn looks &identical to a slow provider. Ranking on this made
    /// the leaderboard meaningless.
    pub ewma_ms: Option<f64>,
    /// Exponentially weighted mean TIME-TO-FIRST-BYTE — the actual routing
    /// signal. Measures how fast a provider *starts* answering, which is
    /// independent of how long the model then thinks.
    #[serde(default)]
    pub ewma_ttfb_ms: Option<f64>,
    /// Samples behind `ewma_ttfb_ms`, so an unmeasured provider is not demoted
    /// on one unlucky reading.
    #[serde(default)]
    pub ttfb_samples: u32,
    /// Client requests served (not attempts).
    #[serde(default)]
    pub client_requests: u64,
    /// Client requests that failed outright on this provider.
    #[serde(default)]
    pub client_failures: u64,
    /// Keys rotated away from on this provider.
    #[serde(default)]
    pub key_rotations: u64,
    /// Times we failed over FROM this provider to another.
    #[serde(default)]
    pub failovers_out: u64,
    /// Times we failed over TO this provider and succeeded.
    #[serde(default)]
    pub rescues: u64,
    /// Exponentially weighted mean error rate in 0.0..=1.0.
    ///
    /// Every finished request contributes 0.0 (ok) or 1.0 (failed), so this is a
    /// *recent* error rate that decays on its own — unlike `errors/requests`,
    /// which is a lifetime figure and would keep punishing a &host that has since
    /// recovered.
    #[serde(default)]
    pub ewma_err: Option<f64>,
    /// Circuit breaker. While `now < open_until` the provider is skipped
    /// entirely, so a &host returning Cloudflare 502s stops being retried instead
    /// of burning a round on every request.
    #[serde(default)]
    pub open_until: u64,
    pub requests: u64,
    pub errors: u64,
    pub bytes_up: u64,
    pub bytes_down: u64,
    pub cost: f64,
    pub last_fail: u64,
    pub consecutive_fails: u32,
    /// Timeouts and 524s (provider slow, not dead). Trips the breaker only at
    /// 4x the hard threshold, and decays like the hard streak. Without this
    /// split, a provider digesting a huge context looks &identical to a dead one
    /// and gets switched away from while still working.
    #[serde(default)]
    pub slow_streak: u64,
    /// Head timeouts that a later round proved premature (the request eventually
    /// succeeded). Feeds the 200-failure analysis that calibrates budgets.
    #[serde(default)]
    pub timeouts_wasted: u64,
    /// Model &ids this provider currently advertises (free `/v1/models` probe).
    #[serde(default)]
    pub models_seen: Vec<String>,
    /// Pre-deduction hold in USD actually demanded, learned per model from quota
    /// 403s (`需要预扣费额度: ＄x`).
    ///
    /// new-api has two pricing modes (`relay/helper/price.go`):
    ///
    /// * `usePrice`  -> `modelPrice * QuotaPerUnit * groupRatio`  = FLAT per request
    /// * ratio mode  -> `(max(promptTokens,500) + maxTokens) * modelRatio` = VARIABLE
    ///
    /// The providers here use the flat mode, confirmed by live probe,
    /// but we never assume: whatever the 403 reports is recorded here and used
    /// instead of the configured default.
    #[serde(default)]
    pub observed_hold: HashMap<String, f64>,
    /// Learned head-budget multiplier for this provider, from the 200-failure
    /// analysis. Bounded [2.0, 5.0]; 0.0 means "no learned value, use global".
    #[serde(default)]
    pub learned_mult: f64,
    /// Credit-free uptime history (from `/v1/models`).
    pub uptime: VecDeque<UptimeSample>,
}

#[derive(Clone, Serialize, Deserialize, Debug, Default)]
pub struct Totals {
    pub started_at: u64,
    /// Upstream ATTEMPTS. One client request may burn several keys, so this is
    /// always >= `client_requests`.
    pub requests: u64,
    /// Failed attempts. Can exceed `requests` in pathological cases, which is
    /// why every rate calculation must use saturating arithmetic.
    pub errors: u64,
    /// Client requests that entered the gateway. THIS is the number a user
    /// means by "how many requests did I make".
    #[serde(default)]
    pub client_requests: u64,
    /// Client requests that ultimately failed (all retries exhausted).
    #[serde(default)]
    pub client_failures: u64,
    /// Client requests that succeeded only because a key was rotated or a
    /// provider was failed over — i.e. saves the user never had to see.
    #[serde(default)]
    pub client_saved: u64,
    pub bytes_up: u64,
    pub bytes_down: u64,
    pub cost: f64,
    pub rotations: u64,
    pub failovers: u64,
    pub offline_holds: u64,
}

/// Notable events surfaced in the dashboard (and worth a notification sound).
#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct Event {
    pub t: u64,
    /// info | warn | error
    pub level: String,
    pub msg: String,
}

/// A request currently in flight. This is what makes "which sessions are live
/// working" answerable — previously the dashboard could only show sessions by
/// `last_seen`, so a session mid-request looked &identical to one that finished
/// 40 minutes ago.
#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct InFlight {
    pub id: u64,
    pub session: String,
    pub label: String,
    pub provider: String,
    pub host: String,
    pub key: String,
    pub model: String,
    pub client: String,
    /// Unix seconds when the request entered the gateway.
    pub started: u64,
    /// Retry round it is currently on.
    pub round: u32,
    /// How many keys have been tried for this request.
    pub attempts: u32,
    pub streaming: bool,
    /// Bytes of request body.
    pub bytes_up: u64,
    /// Last thing that happened, e.g. "waiting on tabi", "retrying: 524".
    pub phase: String,
}

/// One recorded failure, with everything needed to understand it later.
///
/// Errors were previously only counted, never narrated — the dashboard showed
/// "25% of requests failed" with an empty Events tab and no way to find out why.
/// This is the record behind the Errors tab.
#[derive(Clone, Serialize, Deserialize, Debug, Default)]
pub struct ErrorRecord {
    pub t: u64,
    /// QUOTA | AUTH | RATE | WAF | UPSTREAM | OFFLINE | CLIENT | TRANSPORT
    pub class: String,
    pub provider: String,
    pub host: String,
    /// Masked — never a full secret.
    pub key: String,
    pub model: String,
    /// HTTP status, or 0 for a transport failure.
    pub status: u16,
    /// The upstream's own message, trimmed.
    pub message: String,
    /// What the gateway did: rotated-key | failed-over | held-offline |
    /// cooled-down | passed-through | gave-up
    pub action: String,
    /// Session this happened inside, if known.
    pub session: String,
    /// Which retry round it happened on.
    pub round: u32,
    pub latency_ms: u64,
    /// Balance the upstream reported, when it told us.
    pub remaining: Option<f64>,
    /// Hold the upstream demanded, when it told us.
    pub required: Option<f64>,
    // ── training features (all serde-defaulted so old state files load) ──
    /// Request body bytes. Distinguishes "huge upload on slow wifi" from
    /// "small request, slow provider".
    #[serde(default)]
    pub req_bytes: u64,
    /// Was this a streaming request?
    #[serde(default)]
    pub streaming: bool,
    /// Time-to-first-byte when known (head arrived but body failed, or the
    /// timeout budget that was in force). Lets analysis separate slow starts.
    #[serde(default)]
    pub ttfb_ms: Option<u64>,
    /// Head budget seconds that was in force for this attempt.
    #[serde(default)]
    pub budget_secs: u64,
    /// Egress used: proxy addr or "direct". Empty when unknown.
    #[serde(default)]
    pub proxy: String,
    /// True when this attempt died waiting for the response head.
    #[serde(default)]
    pub head_timeout: bool,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct Persisted {
    pub version: u32,
    pub keys: HashMap<String, KeyState>,
    pub sessions: HashMap<String, SessionState>,
    pub providers: HashMap<String, ProviderRuntime>,
    pub minutes: VecDeque<MinuteBucket>,
    /// Hour and day rollups of the same shape as `minutes`, with `t` being the
    /// unix hour / unix day. Written alongside the minute bucket on every
    /// request, so a wide range never has to aggregate 500k minute rows at query
    /// time — and, unlike the minute series, they survive long enough to answer
    /// "what did last month look like".
    #[serde(default)]
    pub hours: VecDeque<MinuteBucket>,
    #[serde(default)]
    pub days: VecDeque<MinuteBucket>,
    pub models: HashMap<String, ModelStat>,
    pub totals: Totals,
    pub events: VecDeque<Event>,
    /// Ring buffer of recent failures for the Errors tab.
    #[serde(default)]
    pub errors_log: VecDeque<ErrorRecord>,
    /// Completed/expired sessions, kept for history after eviction.
    #[serde(default)]
    pub session_history: VecDeque<SessionState>,
}

impl Default for Persisted {
    fn default() -> Self {
        Self {
            version: 3,
            keys: HashMap::new(),
            sessions: HashMap::new(),
            providers: HashMap::new(),
            minutes: VecDeque::new(),
            hours: VecDeque::new(),
            days: VecDeque::new(),
            models: HashMap::new(),
            totals: Totals {
                started_at: config::now_secs(),
                ..Default::default()
            },
            events: VecDeque::new(),
            errors_log: VecDeque::new(),
            session_history: VecDeque::new(),
        }
    }
}

// ── app state ───────────────────────────────────────────────────────────────

pub struct App {
    inner: Mutex<Persisted>,
    /// Runtime-editable providers + routing policy, hot-reloaded from
    /// ~/.config/tabi/providers.json.
    pub settings: Mutex<crate::settings::Settings>,
    /// mtime of the settings file when last loaded.
    pub settings_mtime: std::sync::atomic::AtomicU64,
    /// Requests currently in flight, keyed by a monotonic &id.
    pub inflight: Mutex<std::collections::BTreeMap<u64, InFlight>>,
    next_inflight: std::sync::atomic::AtomicU64,
    /// Ordered key list per provider, as read from disk.
    pub pools: Mutex<HashMap<String, Vec<String>>>,
    pub providers: std::sync::RwLock<Vec<Provider>>,
    /// Set when we believe the device has no network. Makes the proxy hold
    /// requests instead of erroring, and pauses helper probes.
    pub offline: std::sync::atomic::AtomicBool,
    /// Bumped on every state change; the dashboard SSE loop watches this so it
    /// only pushes when something actually changed (battery).
    pub revision: std::sync::atomic::AtomicU64,
    pub dirty: std::sync::atomic::AtomicBool,
}

/// One request's contribution to a history bucket.
#[derive(Clone, Copy)]
struct BucketSample {
    ok: bool,
    bytes_up: u64,
    bytes_down: u64,
    cost: f64,
    latency_ms: u64,
}

/// Add a sample to the newest bucket of `series`, opening a new bucket when the
/// slot rolls over and evicting past `cap`.
///
/// Buckets are append-only and time-ordered, so eviction is a `pop_front` and a
/// query for the last N is a tail slice — no scanning, no sorting.
fn roll(series: &mut VecDeque<MinuteBucket>, slot: u64, cap: usize, s: BucketSample) {
    if series.back().map(|b| b.t != slot).unwrap_or(true) {
        series.push_back(MinuteBucket {
            t: slot,
            ..Default::default()
        });
        while series.len() > cap {
            series.pop_front();
        }
    }
    let Some(b) = series.back_mut() else { return };
    b.requests += 1;
    if !s.ok {
        b.errors += 1;
    }
    b.bytes_up += s.bytes_up;
    b.bytes_down += s.bytes_down;
    b.cost += s.cost;
    b.latency_sum_ms += s.latency_ms;
    b.latency_n += 1;
}

impl App {
    pub fn new() -> Self {
        let (settings, warn) = crate::settings::Settings::load_or_init();
        if let Some(w) = warn {
            eprintln!("[settings] {w}");
        }
        let mtime = crate::settings::Settings::mtime();
        Self {
            inner: Mutex::new(Persisted::default()),
            settings: Mutex::new(settings),
            settings_mtime: std::sync::atomic::AtomicU64::new(mtime),
            inflight: Mutex::new(std::collections::BTreeMap::new()),
            next_inflight: std::sync::atomic::AtomicU64::new(1),
            pools: Mutex::new(HashMap::new()),
            providers: std::sync::RwLock::new(config::providers()),
            offline: std::sync::atomic::AtomicBool::new(false),
            revision: std::sync::atomic::AtomicU64::new(0),
            dirty: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub fn lock(&self) -> MutexGuard<'_, Persisted> {
        // A poisoned mutex means another thread panicked mid-update. The data is
        // still structurally valid (we only ever write whole fields), so recover
        // rather than cascading the panic and taking the gateway down.
        match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    pub fn touch(&self) {
        use std::sync::atomic::Ordering;
        self.revision.fetch_add(1, Ordering::Relaxed);
        self.dirty.store(true, Ordering::Relaxed);
    }

    pub fn rev(&self) -> u64 {
        self.revision.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn is_offline(&self) -> bool {
        self.offline.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn set_offline(&self, v: bool) {
        use std::sync::atomic::Ordering;
        let prev = self.offline.swap(v, Ordering::Relaxed);
        if prev != v {
            self.event(
                if v { "warn" } else { "info" },
                if v {
                    "network offline — holding requests until it returns"
                } else {
                    "network restored"
                },
            );
            self.touch();
        }
    }

    pub fn provider(&self, id: &str) -> Option<Provider> {
        self.providers.read().unwrap().iter().find(|p| p.id == id).cloned()
    }

    /// Rebuild the provider list from the current settings. Called after a
    /// settings save so added/removed/edited providers take effect without a
    /// restart.
    pub fn reload_providers(&self) {
        let s = self.settings.lock().unwrap().clone();
        let new_providers = config::providers_from_settings(&s);
        let mut prov = self.providers.write().unwrap();
        *prov = new_providers;
        self.touch();
    }

    pub fn event(&self, level: &str, msg: impl Into<String>) {
        let msg = msg.into();
        let mut g = self.lock();
        g.events.push_back(Event {
            t: config::now_secs(),
            level: level.to_string(),
            msg,
        });
        while g.events.len() > 200 {
            g.events.pop_front();
        }
        drop(g);
        // `record_error` touches; this did not, so an event alone neither pushed
        // to the SSE stream nor marked state dirty. Anything that only reports
        // via `event()` — the self-check verdicts, egress alerts, provider
        // recovery notices — was invisible until some unrelated write happened
        // to flush. That is how a dead proxy pool went unnoticed for hours.
        self.touch();
    }

    // ── key files ───────────────────────────────────────────────────────────

    /// Load key files from disk, registering any new keys. Safe to call
    /// repeatedly (the GitHub sync helper does exactly that). Returns the number
    /// of newly-registered keys per provider.
    pub fn load_key_files(&self) -> Vec<(String, usize, usize)> {
        let mut report = Vec::new();
        for p in &*self.providers.read().unwrap() {
            let path = config::keys_path(p.keys_file);
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            let mut seen = std::collections::HashSet::new();
            let mut keys = Vec::new();
            let prefix = crate::config::key_prefix();
            let min_len = crate::config::key_min_len();
            for line in text.lines() {
                let k = line.trim();
                // Ignore comments/blank lines; require the key prefix so a
                // stray README line can never become a "key".
                if k.starts_with(&prefix) && k.len() >= min_len && seen.insert(k.to_string()) {
                    keys.push(k.to_string());
                }
            }
            let total = keys.len();
            let mut added = 0;
            {
                let mut g = self.lock();
                for k in &keys {
                    if !g.keys.contains_key(k) {
                        g.keys
                            .insert(k.clone(), KeyState::new(p.id, p.initial_guess));
                        added += 1;
                    }
                }
                g.providers.entry(p.id.to_string()).or_default();
            }
            self.pools
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(p.id.to_string(), keys);
            report.push((p.id.to_string(), total, added));
        }
        self.touch();
        report
    }

    pub fn pool(&self, provider: &str) -> Vec<String> {
        self.pools
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(provider)
            .cloned()
            .unwrap_or_default()
    }

    /// Append a verified key to its provider's file and register it.
    /// Used by the dashboard "add key" flow and the GitHub sync helper.
    pub fn append_key(&self, provider: &str, key: &str) -> std::io::Result<bool> {
        let Some(p) = self.provider(provider) else {
            return Ok(false);
        };
        {
            let g = self.lock();
            if g.keys.contains_key(key) {
                return Ok(false); // already known
            }
        }
        let path = config::keys_path(p.keys_file);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        // Read-modify-write so we never create a duplicate line even if the file
        // was edited outside the gateway.
        let existing = std::fs::read_to_string(&path).unwrap_or_default();
        if existing.lines().any(|l| l.trim() == key) {
            // present on disk but not in memory: just register it
        } else {
            use std::io::Write;
            let needs_nl = !existing.is_empty() && !existing.ends_with('\n');
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)?;
            if needs_nl {
                f.write_all(b"\n")?;
            }
            f.write_all(key.as_bytes())?;
            f.write_all(b"\n")?;
        }
        {
            let mut g = self.lock();
            g.keys
                .entry(key.to_string())
                .or_insert_with(|| KeyState::new(p.id, p.initial_guess));
        }
        {
            let mut pools = self.pools.lock().unwrap_or_else(|e| e.into_inner());
            pools
                .entry(provider.to_string())
                .or_default()
                .push(key.to_string());
        }
        self.touch();
        Ok(true)
    }

    // ── key selection ───────────────────────────────────────────────────────

    /// Keys exclusively owned by other *active* sessions, so parallel agents do
    /// not collide on one key.
    fn owned_elsewhere(
        g: &Persisted,
        except_fp: &str,
        now: u64,
    ) -> std::collections::HashSet<String> {
        let mut out = std::collections::HashSet::new();
        for (fp, s) in &g.sessions {
            if fp == except_fp {
                continue;
            }
            if now.saturating_sub(s.last_seen) < config::session_active_secs() {
                out.insert(s.key.clone());
            }
        }
        out
    }

    /// Ranked candidate keys for a provider.
    ///
    /// **Drain-first**: among usable keys, prefer the LOWEST balance that still
    /// clears the hold. That spends a key down to nothing before opening the
    /// next one, which is exactly "no key half-used and discarded".
    ///
    /// Probed keys rank above unprobed ones because we trust measured data.
    ///
    /// `hold` is the *effective* hold for this request (see [`Self::hold_for`]),
    /// not a constant — new-api's ratio mode scales the hold with prompt size.
    pub fn candidates_with_hold(
        &self,
        provider: &str,
        fp: &str,
        exclude: &[String],
        hold: f64,
    ) -> Vec<String> {
        let now = config::now_secs();
        let pool = self.pool(provider);
        let g = self.lock();
        let owned = Self::owned_elsewhere(&g, fp, now);

        // (allow_owned, require_margin) preference ladder
        let mut ladder: Vec<Vec<String>> = Vec::new();
        for (allow_owned, require_margin) in
            [(false, true), (false, false), (true, true), (true, false)]
        {
            let mut v: Vec<String> = pool
                .iter()
                .filter(|k| !exclude.iter().any(|e| e == *k))
                .filter(|k| allow_owned || !owned.contains(*k))
                .filter_map(|k| {
                    let st = g.keys.get(k)?;
                    if !st.usable(now, hold) {
                        return None;
                    }
                    if require_margin {
                        if let Some(b) = st.balance {
                            if b < hold * config::hold_safety_factor() {
                                return None;
                            }
                        }
                    }
                    // Avoid piling concurrent requests onto one key.
                    if st.inflight > 2 {
                        return None;
                    }
                    Some(k.clone())
                })
                .collect();

            v.sort_by(|a, b| {
                let ka = g.keys.get(a);
                let kb = g.keys.get(b);
                let (ba, bb) = (ka.and_then(|x| x.balance), kb.and_then(|x| x.balance));
                match (ba, bb) {
                    // probed first
                    (Some(_), None) => std::cmp::Ordering::Less,
                    (None, Some(_)) => std::cmp::Ordering::Greater,
                    // drain-first: lowest usable balance wins
                    (Some(x), Some(y)) => x.partial_cmp(&y).unwrap_or(std::cmp::Ordering::Equal),
                    // both unprobed: least recently used
                    (None, None) => ka
                        .map(|x| x.last_used)
                        .unwrap_or(0)
                        .cmp(&kb.map(|x| x.last_used).unwrap_or(0)),
                }
            });
            if !v.is_empty() {
                ladder.push(v);
            }
        }
        ladder.into_iter().next().unwrap_or_default()
    }

    /// Convenience wrapper using the provider's configured hold.
    pub fn candidates(&self, provider: &str, fp: &str, exclude: &[String]) -> Vec<String> {
        let hold = self.provider(provider).map(|p| p.hold).unwrap_or(0.8);
        self.candidates_with_hold(provider, fp, exclude, hold)
    }

    /// Any usable key regardless of balance — for free endpoints like /v1/models
    /// that spend no credits. Skips the hold/safety-factor check so the models
    /// endpoint works even when every key is drained.
    pub fn candidates_for_probe(&self, provider: &str, exclude: &[String]) -> Vec<String> {
        let pool = self.pool(provider);
        let g = self.lock();
        let now = config::now_secs();
        pool.iter()
            .filter(|k| !exclude.iter().any(|e| e == *k))
            .filter_map(|k| {
                let st = g.keys.get(k)?;
                if st.dead_until > now {
                    return None;
                }
                Some(k.clone())
            })
            .collect()
    }

    /// Which providers to try, best-first.
    ///
    /// # Scoring
    /// A provider is chosen by a single cost score, lower being better:
    ///
    /// ```text
    /// score = latency_penalty + error_penalty + model_penalty + breaker_penalty
    /// ```
    ///
    /// * **latency** — EWMA response time in seconds. Unmeasured providers get a
    ///   small optimistic value so they are sampled rather than starved.
    /// * **error** — recent EWMA error rate, weighted 4x latency. Errors are what
    ///   the user actually feels (a retry costs a whole round trip), so a &host
    ///   dropping requests loses to a slower &host that answers.
    /// * **model** — a provider that does not advertise the requested model is
    ///   pushed to the back, but not excluded: `/v1/models` can be stale, and a
    ///   provider may still serve a model it does not list.
    /// * **breaker** — while the circuit is open the provider is effectively last.
    ///
    /// `preferred` (an explicit `/tabi/...` route, or the session's existing
    /// provider) always wins, because session stickiness matters more than a
    /// marginal latency difference: switching &hosts mid-conversation means a new
    /// key, a new account wallet, and a lost prompt cache.
    pub fn provider_order(&self, preferred: Option<&str>) -> Vec<String> {
        self.provider_order_for(preferred, "")
    }

    /// Model-aware variant of [`Self::provider_order`].
    pub fn provider_order_for(&self, preferred: Option<&str>, model: &str) -> Vec<String> {
        let now = config::now_secs();
        // Policy weights come from settings so they are tunable without a rebuild.
        let (halflife, err_weight, streak_weight, model_pen_w, min_samples, biases) = {
            let s = self.settings.lock().unwrap_or_else(|e| e.into_inner());
            let b: std::collections::HashMap<String, f64> =
                s.providers.iter().map(|p| (p.id.clone(), p.bias)).collect();
            (
                s.routing.streak_halflife_secs,
                s.routing.error_weight,
                s.routing.streak_weight,
                s.routing.missing_model_penalty,
                s.routing.min_samples,
                b,
            )
        };
        let pools: Vec<(String, usize)> = self
            .providers
            .read().unwrap()
            .iter()
            .map(|p| (p.id.to_string(), self.pool(p.id).len()))
            .collect();
        let g = self.lock();

        let mut scored: Vec<(String, f64, bool, bool)> = pools
            .iter()
            .filter(|(_, n)| *n > 0)
            .map(|(id, _)| {
                let rt = g.providers.get(id);

                // TTFB in seconds — NOT full duration, which would count model
                // thinking time and make a fast provider serving a long reasoning
                // turn look slow. 1.5s for unmeasured: optimistic enough to get
                // sampled, not so low that it always wins.
                //
                // Below `min_samples` the reading is not trusted: blend toward the
                // neutral default so one unlucky measurement cannot demote a
                // provider that has barely been tried.
                let lat = match (rt.and_then(|r| r.ewma_ttfb_ms), rt.map(|r| r.ttfb_samples)) {
                    (Some(v), Some(n)) if n >= min_samples => v / 1000.0,
                    (Some(v), Some(n)) => {
                        let w = n as f64 / min_samples.max(1) as f64;
                        (v * w + 1500.0 * (1.0 - w)) / 1000.0
                    }
                    _ => 1.5,
                };

                // Recent error rate, decayed by age so a provider with no
                // traffic still recovers instead of being demoted forever.
                let err_pen = decayed_err(rt, now, halflife) * err_weight;

                // Consecutive failures compound fast — 3 in a row is a real
                // outage, not noise — but they DECAY. Without decay the penalty
                // is self-reinforcing: never chosen, so never succeeds, so the
                // streak never clears.
                let streak = decayed_streak(rt, now, halflife);
                let streak_pen = (streak * streak) * streak_weight;
                // Slow-but-alive counts at quarter weight: persistent slowness
                // should lose to a fast provider, but never look like death.
                let slow = decayed_slow(rt, now, halflife);
                let slow_pen = (slow * slow) * streak_weight * 0.25;

                 // Model availability. has_model is authoritative when any provider
                 // advertises the model: a provider whose known list lacks it is
                 // excluded outright below, not merely penalised. Unknown (never
                 // probed) still counts as capable — /v1/models can be stale.
                 // The declared models list (from settings) is unioned with the
                 // probed list, so a provider the user KNOWS serves a model is
                 // never penalised just because the probe came back empty.
                 let declared = self.provider(id).map(|p| p.models.clone()).unwrap_or_default();
                 let (model_pen, has_model) = if model.is_empty() {
                     (0.0, true)
                 } else if declared.iter().any(|m| m == model) {
                     (0.0, true)
                 } else {
                     match rt.map(|r| &r.models_seen) {
                         Some(list) if list.is_empty() => (0.0, true),
                         Some(list) if list.iter().any(|m| m == model) => (0.0, true),
                         Some(_) => (model_pen_w, false),
                         None => (0.0, true),
                     }
                 };

                // Circuit breaker.
                let breaker_pen = match rt {
                    Some(r) if r.open_until > now => 500.0,
                    _ => 0.0,
                };

                // No funded keys is the worst outcome: nothing can be served.
                let hold = self.provider(&id).map(|p| p.hold).unwrap_or(0.8);
                let has_funded = self
                    .pool(&id)
                    .iter()
                    .any(|k| g.keys.get(k).map(|s| s.usable(now, hold)).unwrap_or(false));
                let dry_pen = if has_funded { 0.0 } else { 1000.0 };

                // Manual per-provider nudge from settings.
                let bias = biases.get(id).copied().unwrap_or(0.0);

                (
                    id.clone(),
                    lat + err_pen + streak_pen + slow_pen + model_pen + breaker_pen + dry_pen
                        - bias,
                    has_funded,
                    has_model,
                )
            })
            .map(|(id, a, b, c)| (id.clone(), a, b, c)).collect();

        scored.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

        // Drop providers that do not serve this model, but ONLY when someone does.
        //
        // Previously a missing model was just a +50 penalty, so a justwoker-only
        // model still walked tabi and gorouter rungs every round — each a
        // guaranteed NoChannel/Client failure that cost a full head-budget wait.
        // Never return empty: if nobody advertises it, keep everyone and let the
        // normal error path report it (lists can be stale).
        if !model.is_empty() && scored.iter().any(|(_, _, _, m)| *m) {
            scored.retain(|(_, _, _, m)| *m);
        }

        // Drop provably dry providers, but ONLY while a funded one remains.
        //
        // The dry penalty above already sorts them last; the problem is that the
        // ladder still spends a whole rung discovering "no funded key available"
        // on every round. justwoker (0 funded keys, $0 balance) did exactly that
        // on all 12 attempts of a failed 1M request. `usable()` is optimistic for
        // unprobed keys, so a provider only lands here once every key it owns is
        // known-dead or known-broke — and the balance sweeper keeps re-probing
        // them, so this reverses itself the moment one gets funded again.
        //
        // Never return empty: if every provider is dry, keep them all and let the
        // normal "no funded key" path report it rather than claiming none exist.
        let any_funded = scored.iter().any(|(_, _, funded, _)| *funded);
        if any_funded {
            scored.retain(|(_, _, funded, _)| *funded);
        }

        let mut ids: Vec<String> = scored.into_iter().map(|(id, _, _, _)| id).collect();

        if let Some(pref) = preferred {
            if let Some(pos) = ids.iter().position(|x| x == pref) {
                let v = ids.remove(pos);
                ids.insert(0, v);
            }
        }
        ids
    }

    /// Provider scores, for the dashboard's routing explanation.
    ///
    /// Mirrors the weights in `provider_order_for` exactly: the dashboard must
    /// explain the routing decision the gateway actually makes, not a parallel
    /// opinion. TTFB (not total duration) drives the latency term, for the same
    /// reason — a thinking turn must not look like a slow provider.
    pub fn provider_scores(&self, model: &str) -> Vec<(String, f64)> {
        let order = self.provider_order_for(None, model);
        let now = config::now_secs();
        let (halflife, err_weight, streak_weight) = {
            let s = self.settings.lock().unwrap_or_else(|e| e.into_inner());
            (
                s.routing.streak_halflife_secs,
                s.routing.error_weight,
                s.routing.streak_weight,
            )
        };
        let g = self.lock();
        order
            .into_iter()
            .map(|id| {
                let rt = g.providers.get(&id);
                let lat = rt.and_then(|r| r.ewma_ttfb_ms).unwrap_or(1500.0) / 1000.0;
                let err = decayed_err(rt, now, halflife) * err_weight;
                let streak = decayed_streak(rt, now, halflife);
                let slow = decayed_slow(rt, now, halflife);
                let open = rt.map(|r| r.open_until > now).unwrap_or(false);
                let score = lat
                    + err
                    + (streak * streak) * streak_weight
                    + (slow * slow) * streak_weight * 0.25
                    + if open { 500.0 } else { 0.0 };
                (id.clone(), (score * 1000.0).round() / 1000.0)
            })
            .map(|(id, f)| (id.clone(), f)).collect()
    }

    // ── mutations ───────────────────────────────────────────────────────────

    pub fn mark_dead(&self, key: &str, ttl: u64, reason: impl Into<String>) {
        let mut g = self.lock();
        if let Some(k) = g.keys.get_mut(key) {
            k.dead_until = config::now_secs() + ttl;
            k.reason = reason.into();
        }
        drop(g);
        self.touch();
    }

    /// Learn a key's exact starting balance from a quota 403.
    ///
    /// The 403 states remaining balance; combined with measured spend that
    /// pins `initial` permanently, so this key is never mis-selected again.
    pub fn learn_from_quota(&self, key: &str, remaining: Option<f64>) {
        let mut g = self.lock();
        if let Some(k) = g.keys.get_mut(key) {
            if let Some(r) = remaining {
                if let Some(u) = k.usage_cents {
                    k.initial = r + u / 100.0;
                    k.initial_exact = true;
                }
                k.balance = Some(r);
            } else {
                k.balance = Some(0.0);
            }
        }
        drop(g);
        self.touch();
    }

    /// Record the hold a provider actually demanded for a model.
    ///
    /// From `relay/helper/price.go`, the hold is either flat (`usePrice` mode) or
    /// `(max(promptTokens,500) + maxTokens) * modelRatio * groupRatio`. Rather
    /// than modelling their pricing config, we take the number straight from the
    /// 403 and keep the largest seen — a conservative floor for "can this key
    /// serve this model".
    pub fn learn_hold(&self, provider: &str, model: &str, required: f64) {
        if required <= 0.0 || model.is_empty() {
            return;
        }
        let mut g = self.lock();
        let r = g.providers.entry(provider.to_string()).or_default();
        let cur = r.observed_hold.get(model).copied().unwrap_or(0.0);
        if required > cur {
            r.observed_hold.insert(model.to_string(), required);
            drop(g);
            self.touch();
        }
    }

    /// Effective hold for a provider/model: the largest value actually observed,
    /// falling back to the configured default when we have not seen a 403 yet.
    pub fn hold_for(&self, provider: &str, model: &str) -> f64 {
        let base = self.provider(provider).map(|p| p.hold).unwrap_or(0.8);
        if model.is_empty() {
            return base;
        }
        let g = self.lock();
        g.providers
            .get(provider)
            .and_then(|r| r.observed_hold.get(model).copied())
            .map(|observed| observed.max(base))
            .unwrap_or(base)
    }

    pub fn set_usage(&self, key: &str, usage_cents: f64) {
        let mut g = self.lock();

        // Billing-delta reconciliation.
        //
        // The OpenAI-compatible endpoint (`/v1/chat/completions`) returns token
        // counts but NO `usage.cost` — only `/v1/messages` does. Streaming
        // responses cannot be inspected at all without consuming the stream.
        // So response bodies alone cannot account for spend.
        //
        // The free billing endpoint IS ground truth: `total_usage` is cumulative
        // cents spent (verified against new-api `controller/billing.go`, where
        // `TotalUsage = quota/QuotaPerUnit*100`). Any increase since the last
        // probe is real money, so attribute it to the key and to whichever
        // session currently holds that key.
        let delta_dollars = match g.keys.get(key).and_then(|k| k.usage_cents) {
            Some(prev) if usage_cents > prev => (usage_cents - prev) / 100.0,
            _ => 0.0,
        };

        if let Some(k) = g.keys.get_mut(key) {
            k.usage_cents = Some(usage_cents);
            k.last_probe = config::now_secs();
            k.recompute();
            if delta_dollars > 0.0 {
                k.spent += delta_dollars;
            }
        }

        if delta_dollars > 0.0 {
            // Credit the most recently active session on this key. Without this,
            // per-session cost stays at zero for every non-Anthropic call.
            let owner = g
                .sessions
                .iter()
                .filter(|(_, s)| s.key == key)
                .max_by_key(|(_, s)| s.last_seen)
                .map(|(fp, _)| fp.clone());
            if let Some(fp) = owner {
                if let Some(s) = g.sessions.get_mut(&fp) {
                    s.cost += delta_dollars;
                }
            }

            // Roll it into lifetime + per-provider totals exactly once.
            let provider = g.keys.get(key).map(|k| k.provider.clone());
            g.totals.cost += delta_dollars;
            if let Some(p) = provider {
                g.providers.entry(p).or_default().cost += delta_dollars;
            }
        }

        drop(g);
        self.touch();
    }

    pub fn mark_probed(&self, key: &str) {
        let mut g = self.lock();
        if let Some(k) = g.keys.get_mut(key) {
            k.last_probe = config::now_secs();
        }
    }

    pub fn inflight(&self, key: &str, delta: i32) {
        let mut g = self.lock();
        if let Some(k) = g.keys.get_mut(key) {
            if delta > 0 {
                k.inflight = k.inflight.saturating_add(delta as u32);
                k.last_used = config::now_secs();
            } else {
                k.inflight = k.inflight.saturating_sub((-delta) as u32);
            }
        }
    }

    /// Record a successful attempt.
    ///
    /// `ttfb_ms` is the routing signal (time to response head); `total_ms` is
    /// informational. Keeping them apart is what stops a long thinking turn from
    /// being mistaken for a slow provider.
    pub fn note_latency_split(&self, provider: &str, ttfb_ms: u64, total_ms: u64) {
        let mut g = self.lock();
        let r = g.providers.entry(provider.to_string()).or_default();
        r.ewma_ttfb_ms = Some(match r.ewma_ttfb_ms {
            Some(prev) => prev * 0.7 + ttfb_ms as f64 * 0.3,
            None => ttfb_ms as f64,
        });
        r.ttfb_samples = r.ttfb_samples.saturating_add(1);
        r.ewma_ms = Some(match r.ewma_ms {
            Some(prev) => prev * 0.7 + total_ms as f64 * 0.3,
            None => total_ms as f64,
        });
        // A success is a 0.0 sample for the error rate, so recovery is quick.
        r.ewma_err = Some(match r.ewma_err {
            Some(prev) => prev * 0.8,
            None => 0.0,
        });
        r.consecutive_fails = 0;
        r.slow_streak = 0;
        r.open_until = 0;
        drop(g);
        self.touch();
    }

    /// Adaptive head budget for a streaming request to this provider.
    ///
    /// Two terms, because either alone is wrong:
    ///
    /// * the provider's measured TTFB EWMA, so the budget tracks how that &host
    ///   is behaving right now (requires at least 2 samples — a single unlucky
    ///   reading must not set policy); plus
    /// * a size term derived from `req_bytes`, because the EWMA is a single
    ///   scalar per provider and is therefore **blind to how big THIS request
    ///   is**. That blindness meant a 281KB request and a 2.7MB request were
    ///   handed the same 45s. The small one needed 11s and passed; the large one
    ///   needed ~59s and could never pass, on any key, on any provider. And
    ///   since only successes call `note_latency_split`, those timeouts were
    ///   excluded from the EWMA, so large requests could never teach the budget
    ///   to accommodate them either.
    ///
    /// Pass `req_bytes = 0` when the size is genuinely unknown; the size term
    /// then vanishes and this degrades to the old EWMA-only behaviour.
    pub fn head_budget_secs(&self, provider: &str, req_bytes: u64) -> u64 {
        let g = self.lock();
        let (ewma, samples) = match g.providers.get(provider) {
            Some(r) => (r.ewma_ttfb_ms, r.ttfb_samples),
            None => (None, 0),
        };
        drop(g);
        let base = match (ewma, samples) {
            (Some(v), n) if n >= 2 => v / 1000.0 * config::head_mult(),
            _ => config::head_default_secs() as f64,
        };
        let mb = req_bytes as f64 / (1024.0 * 1024.0);
        // Overhead applies only when there IS a body to buffer and clone; a
        // sizeless request pays nothing for a cost it never incurs.
        let overhead = if req_bytes > 0 {
            config::head_overhead_secs()
        } else {
            0.0
        };
        let size_term = (mb * config::head_secs_per_mb() + overhead) * config::head_size_safety();
        // A degraded local link inflates TTFB for reasons the provider is not
        // responsible for. Widening the budget here is what stops a weak radio
        // link from being recorded as provider slowness and burning healthy keys.
        let total = ((base + size_term) * self.link_slowdown()).round();
        // Saturating cast: NaN/negative env overrides must not wrap to a huge u64.
        let total = if total.is_finite() && total > 0.0 {
            total as u64
        } else {
            0
        };
        total.clamp(config::head_min_secs(), config::head_max_secs())
    }

    /// Budget multiplier for the current local Wi-Fi link.
    ///
    /// Separated from `head_budget_secs` so tests can reason about the size
    /// arithmetic without a live sampler, and so the link signal has exactly one
    /// entry point into routing. Returns 1.0 whenever the link is good or has
    /// never been sampled, which is what keeps this inert on any device where
    /// the sampler cannot run.
    pub fn link_slowdown(&self) -> f64 {
        crate::link::budget_multiplier()
    }

    pub fn note_latency(&self, provider: &str, ms: u64) {
        let mut g = self.lock();
        let r = g.providers.entry(provider.to_string()).or_default();
        r.ewma_ttfb_ms = Some(match r.ewma_ttfb_ms {
            Some(prev) => prev * 0.7 + ms as f64 * 0.3,
            None => ms as f64,
        });
        r.ttfb_samples = r.ttfb_samples.saturating_add(1);
        r.ewma_ms = Some(match r.ewma_ms {
            Some(prev) => prev * 0.7 + ms as f64 * 0.3,
            None => ms as f64,
        });
        // A success is a 0.0 sample for the error rate, so recovery is quick.
        r.ewma_err = Some(match r.ewma_err {
            Some(prev) => prev * 0.8,
            None => 0.0,
        });
        r.consecutive_fails = 0;
        r.slow_streak = 0;
        // Closing the breaker on success is what makes recovery automatic.
        r.open_until = 0;
    }

    /// A provider that was SLOW, not dead.
    pub fn note_provider_timeout(&self, provider: &str) {
        let mut g = self.lock();
        let r = g.providers.entry(provider.to_string()).or_default();
        r.errors += 1;
        r.last_fail = config::now_secs();
        r.slow_streak = r.slow_streak.saturating_add(1);
        r.ewma_err = Some(match r.ewma_err {
            Some(prev) => prev * 0.95 + 0.05,
            None => 0.05,
        });
        let trip = self.settings_trip() * 4;
        if r.slow_streak >= trip as u64 && r.consecutive_fails == 0 {
            let backoff = match r.slow_streak {
                0..=15 => 15,
                16..=31 => 45,
                _ => 120,
            };
            r.open_until = config::now_secs() + backoff;
        }
        drop(g);
        self.touch();
    }

    /// A timeout later proved premature (a subsequent round succeeded).
    pub fn note_wasted_timeout(&self, provider: &str) {
        let mut g = self.lock();
        if let Some(r) = g.providers.get_mut(provider) {
            r.timeouts_wasted = r.timeouts_wasted.saturating_add(1);
        }
        drop(g);
        self.touch();
    }

    pub fn effective_slow(&self, provider: &str, halflife_secs: u64) -> f64 {
        let now = config::now_secs();
        let g = self.lock();
        let Some(r) = g.providers.get(provider) else {
            return 0.0;
        };
        if r.slow_streak == 0 {
            return 0.0;
        }
        let age = now.saturating_sub(r.last_fail);
        if age <= halflife_secs {
            return r.slow_streak as f64;
        }
        let d = r.slow_streak as f64 * 0.5_f64.powf(age as f64 / halflife_secs.max(1) as f64);
        if d < 0.05 {
            0.0
        } else {
            d
        }
    }

    fn settings_trip(&self) -> u32 {
        self.settings
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .routing
            .breaker_trip
    }

    pub fn note_provider_fail(&self, provider: &str) {
        let mut g = self.lock();
        let r = g.providers.entry(provider.to_string()).or_default();
        r.errors += 1;
        r.last_fail = config::now_secs();
        r.consecutive_fails = r.consecutive_fails.saturating_add(1);
        // A failure is a 1.0 sample.
        r.ewma_err = Some(match r.ewma_err {
            Some(prev) => prev * 0.8 + 0.2,
            None => 0.2,
        });

        // Trip the breaker after 3 consecutive failures. This is the fix for
        // repeated `Bad Gateway`: rather than retrying a &host that is clearly
        // down on every single request, skip it for a while. Backoff grows with
        // the streak but is capped so recovery is never far away.
        if r.consecutive_fails >= 3 {
            let backoff = match r.consecutive_fails {
                3..=4 => 15,
                5..=8 => 45,
                _ => 120,
            };
            r.open_until = config::now_secs() + backoff;
        }
        drop(g);
        self.touch();
    }

    /// Is this provider's circuit breaker currently open?
    pub fn breaker_open(&self, provider: &str) -> bool {
        let now = config::now_secs();
        self.lock()
            .providers
            .get(provider)
            .map(|r| r.open_until > now)
            .unwrap_or(false)
    }

    /// The provider has no backend channel for this model.
    ///
    /// Not a key fault and not a transport fault: new-api answered correctly and
    /// said it has nothing to route to. Every sibling key returns the &identical
    /// answer, so this opens the breaker IMMEDIATELY rather than after a streak
    /// of three — the streak logic exists to tolerate flaky &hosts, and there is
    /// nothing flaky here to tolerate.
    ///
    /// Deliberately does NOT touch `consecutive_fails` or `ewma_err`: the &host
    /// is healthy and must not be scored as broken, or it would stay demoted in
    /// the routing order long after its channels came back.
    pub fn note_provider_no_channel(&self, provider: &str) {
        let mut g = self.lock();
        let r = g.providers.entry(provider.to_string()).or_default();
        r.errors += 1;
        r.last_fail = config::now_secs();
        r.open_until = config::now_secs() + config::no_channel_cooldown_secs();
        drop(g);
        self.touch();
    }

    pub fn push_uptime(&self, provider: &str, up: bool, ms: u32) {
        let mut g = self.lock();
        let r = g.providers.entry(provider.to_string()).or_default();
        r.uptime.push_back(UptimeSample {
            t: config::now_secs(),
            up,
            ms,
        });
        while r.uptime.len() > config::UPTIME_SAMPLES {
            r.uptime.pop_front();
        }
        drop(g);
        self.touch();
    }

    /// Record a completed request. This is the single place metrics are updated
    /// so the dashboard's numbers can never disagree with each other.
    #[allow(clippy::too_many_arguments)]
    pub fn record_request(
        &self,
        fp: &str,
        label: &str,
        provider: &str,
        key: &str,
        model: &str,
        ok: bool,
        latency_ms: u64,
        bytes_up: u64,
        bytes_down: u64,
        cost: f64,
        in_tokens: u64,
        out_tokens: u64,
    ) {
        let now = config::now_secs();
        let minute = now / 60;
        let mut g = self.lock();

        // lifetime
        g.totals.requests += 1;
        if !ok {
            g.totals.errors += 1;
        }
        g.totals.bytes_up += bytes_up;
        g.totals.bytes_down += bytes_down;
        g.totals.cost += cost;

        // Rolling history at three resolutions. All three are bumped here rather
        // than aggregated at query time: rolling up 500k minute rows on every
        // chart range change would be slow AND the minute rows are evicted long
        // before a month-wide question can be asked of them.
        let s = BucketSample {
            ok,
            bytes_up,
            bytes_down,
            cost,
            latency_ms,
        };
        // Three separate calls rather than a loop over `[&mut g.minutes, ..]`:
        // that would be three simultaneous mutable borrows of `g`.
        roll(&mut g.minutes, minute, config::HISTORY_MINUTES, s);
        roll(&mut g.hours, now / 3600, config::HISTORY_HOURS, s);
        roll(&mut g.days, now / 86400, config::HISTORY_DAYS, s);

        // per-provider
        let r = g.providers.entry(provider.to_string()).or_default();
        r.requests += 1;
        if !ok {
            r.errors += 1;
        }
        r.bytes_up += bytes_up;
        r.bytes_down += bytes_down;
        r.cost += cost;

        // per-model, combined across providers
        if !model.is_empty() {
            let m = g.models.entry(model.to_string()).or_default();
            m.requests += 1;
            if !ok {
                m.errors += 1;
            }
            m.cost += cost;
            m.in_tokens += in_tokens;
            m.out_tokens += out_tokens;
            m.latency_sum_ms += latency_ms;
            m.latency_n += 1;
            *m.by_provider.entry(provider.to_string()).or_insert(0) += 1;
        }

        // per-key
        if let Some(k) = g.keys.get_mut(key) {
            if ok {
                k.turns += 1;
            }
            k.last_used = now;
            // Only fold a response-reported cost in here. `set_usage()` handles
            // spend learned from the billing endpoint, and double-counting the
            // same dollars would inflate every total on the dashboard.
            if cost > 0.0 {
                k.spent += cost;
                if let Some(u) = k.usage_cents {
                    k.usage_cents = Some(u + cost * 100.0);
                    k.recompute();
                }
            }
        }

        // per-session
        if !fp.is_empty() {
            let s = g
                .sessions
                .entry(fp.to_string())
                .or_insert_with(|| SessionState {
                    created: now,
                    ..Default::default()
                });
            // A change of key mid-session is worth counting: it means the old key
            // ran dry or the provider failed over.
            if !s.key.is_empty() && s.key != key {
                s.key_switches += 1;
            }
            s.key = key.to_string();
            s.provider = provider.to_string();
            s.last_seen = now;
            s.turns += 1;
            s.cost += cost;
            s.in_tokens += in_tokens;
            s.out_tokens += out_tokens;
            s.bytes_up += bytes_up;
            s.bytes_down += bytes_down;
            if !ok {
                s.errors += 1;
            }
            if latency_ms > s.max_latency_ms {
                s.max_latency_ms = latency_ms;
            }
            if !label.is_empty() {
                s.label = label.to_string();
            }
            if !model.is_empty() {
                *s.models.entry(model.to_string()).or_insert(0) += 1;
            }
        }

        // Bound session growth so a long-lived gateway cannot leak memory.
        if g.sessions.len() > 400 {
            let mut by_age: Vec<(String, u64)> = g
                .sessions
                .iter()
                .map(|(k, v)| (k.clone(), v.last_seen))
                .collect();
            by_age.sort_by_key(|(_, t)| *t);
            for (k, _) in by_age.into_iter().take(100) {
                g.sessions.remove(&k);
            }
        }

        drop(g);
        self.touch();
    }

    // ── failure decay ───────────────────────────────────────────────────────

    /// Effective failure streak, decayed by age.
    ///
    /// The raw counter is self-reinforcing: a demoted provider is never chosen,
    /// so it never succeeds, so `consecutive_fails` never resets. Observed live —
    /// gorouter carried a 3-failure streak for 32 minutes while every free uptime
    /// probe passed. Decaying by age breaks that loop: full weight for the first
    /// `halflife` seconds, then halving each half-life.
    pub fn effective_streak(&self, provider: &str, halflife_secs: u64) -> f64 {
        let now = config::now_secs();
        let g = self.lock();
        let Some(r) = g.providers.get(provider) else {
            return 0.0;
        };
        if r.consecutive_fails == 0 {
            return 0.0;
        }
        let age = now.saturating_sub(r.last_fail);
        if age <= halflife_secs {
            return r.consecutive_fails as f64;
        }
        let halves = age as f64 / halflife_secs.max(1) as f64;
        let decayed = r.consecutive_fails as f64 * 0.5_f64.powf(halves);
        if decayed < 0.05 {
            0.0
        } else {
            decayed
        }
    }

    /// Error rate decayed by how long it has been since the last failure, so a
    /// provider with no traffic still recovers its score.
    pub fn effective_error_rate(&self, provider: &str, halflife_secs: u64) -> f64 {
        let now = config::now_secs();
        let g = self.lock();
        let Some(r) = g.providers.get(provider) else {
            return 0.0;
        };
        let base = r.ewma_err.unwrap_or(0.0);
        if base <= 0.0 {
            return 0.0;
        }
        let age = now.saturating_sub(r.last_fail);
        if age <= halflife_secs {
            return base;
        }
        let halves = age as f64 / halflife_secs.max(1) as f64;
        let decayed = base * 0.5_f64.powf(halves);
        if decayed < 0.01 {
            0.0
        } else {
            decayed
        }
    }

    /// A successful FREE uptime probe is a valid recovery signal.
    ///
    /// It costs nothing, runs every 2 minutes, and proves the &host is reachable —
    /// so using it to clear a streak is what lets a provider come back without
    /// needing to be selected first.
    /// A successful FREE probe proves the &host is REACHABLE. That earns another
    /// chance at being selected — it does not earn a clean record.
    ///
    /// This used to zero `consecutive_fails` and `slow_streak` and halve
    /// `ewma_err`. Measured consequence, from the 2026-09-04 21:36-21:56
    /// incident: `/v1/models` answered 200 in 541-1695ms median — *faster* than
    /// its own 1800ms baseline — every two minutes, while 62% of real attempts
    /// were failing. That endpoint is served from the relay's own model table
    /// and never touches a backend channel, so when the channels were exhausted
    /// the probe got quieter. It was not weakly correlated with serviceability;
    /// it was anti-correlated. And every two minutes it erased the breaker
    /// evidence that real 429s and 503s had just produced. The fingerprint was
    /// `ewma_err = 0.0` on a provider carrying 506 errors in 1388 attempts.
    ///
    /// Deleting the call entirely would resurrect the self-reinforcing
    /// demotion loop this function was written for, where a provider carried a
    /// stale failure streak for half an hour. But recovery is already handled
    /// correctly by time-decay in `effective_streak` and
    /// `effective_error_rate`. So only the hard skip is lifted: the breaker
    /// closes, and the accumulated evidence stays.
    pub fn probe_healed(&self, provider: &str) {
        let mut g = self.lock();
        let r = g.providers.entry(provider.to_string()).or_default();
        let had = r.consecutive_fails > 0 || r.slow_streak > 0 || r.ewma_err.unwrap_or(0.0) > 0.0;
        r.consecutive_fails = 0;
        r.slow_streak = 0;
        r.open_until = 0;
        if let Some(e) = r.ewma_err {
            r.ewma_err = Some(if e * 0.5 < 0.01 { 0.0 } else { e * 0.5 });
        }
        drop(g);
        if had {
            self.touch();
        }
    }

    // ── error log ───────────────────────────────────────────────────────────

    /// Record a failure with full context, and mirror it into the event feed.
    #[allow(clippy::too_many_arguments)]
    pub fn record_error(&self, rec: ErrorRecord) {
        let line = format!(
            "{} {} {} — {} ({})",
            rec.class,
            rec.provider,
            if rec.status > 0 {
                format!("HTTP {}", rec.status)
            } else {
                "transport".into()
            },
            rec.message.chars().take(120).collect::<String>(),
            rec.action
        );
        let mut g = self.lock();
        g.errors_log.push_back(rec);
        while g.errors_log.len() > 500 {
            g.errors_log.pop_front();
        }
        // Errors are also events: the Events tab was empty precisely because
        // per-request failures were counted but never narrated.
        g.events.push_back(Event {
            t: config::now_secs(),
            level: "error".into(),
            msg: line,
        });
        while g.events.len() > 200 {
            g.events.pop_front();
        }
        drop(g);
        self.touch();
    }

    pub fn recent_errors(&self, limit: usize) -> Vec<ErrorRecord> {
        let g = self.lock();
        g.errors_log.iter().rev().take(limit).cloned().collect()
    }

    // ── live request tracking ───────────────────────────────────────────────

    /// Register a request as in flight. Returns its &id for later updates.
    #[allow(clippy::too_many_arguments)]
    pub fn inflight_begin(
        &self,
        session: &str,
        label: &str,
        model: &str,
        client: &str,
        streaming: bool,
        bytes_up: u64,
    ) -> u64 {
        let id = self
            .next_inflight
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let rec = InFlight {
            id,
            session: session.to_string(),
            label: label.to_string(),
            provider: String::new(),
            host: String::new(),
            key: String::new(),
            model: model.to_string(),
            client: client.to_string(),
            started: config::now_secs(),
            round: 0,
            attempts: 0,
            streaming,
            bytes_up,
            phase: "selecting provider".into(),
        };
        self.inflight
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id.clone(), rec);
        self.touch();
        id
    }

    /// Update what an in-flight request is doing.
    pub fn inflight_phase(
        &self,
        id: u64,
        provider: &str,
        host: &str,
        key: &str,
        round: u32,
        phase: &str,
    ) {
        let mut g = self.inflight.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(r) = g.get_mut(&id) {
            if !provider.is_empty() {
                r.provider = provider.to_string();
            }
            if !host.is_empty() {
                r.host = host.to_string();
            }
            if !key.is_empty() {
                r.key = mask_key(key);
                r.attempts += 1;
            }
            r.round = round;
            r.phase = phase.to_string();
        }
        drop(g);
        self.touch();
    }

    pub fn inflight_end(&self, id: u64) {
        self.inflight
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id);
        self.touch();
    }

    pub fn inflight_list(&self) -> Vec<InFlight> {
        self.inflight
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect()
    }

    /// Sessions with at least one request in flight right now.
    pub fn working_sessions(&self) -> std::collections::HashSet<String> {
        self.inflight
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .map(|r| r.session.clone())
            .collect()
    }

    pub fn note_rotation(&self) {
        self.lock().totals.rotations += 1;
    }

    /// A client request entered the gateway.
    pub fn note_client_request(&self) {
        let mut g = self.lock();
        g.totals.client_requests += 1;
    }

    /// A client request finished. `saved` means it only succeeded because a key
    /// was rotated or a provider failed over — a save the user never saw.
    pub fn note_client_outcome(&self, provider: &str, ok: bool, saved: bool) {
        let mut g = self.lock();
        if ok {
            if saved {
                g.totals.client_saved += 1;
            }
        } else {
            g.totals.client_failures += 1;
        }
        if !provider.is_empty() {
            let r = g.providers.entry(provider.to_string()).or_default();
            if ok {
                r.client_requests += 1;
                if saved {
                    r.rescues += 1;
                }
            } else {
                r.client_failures += 1;
            }
        }
        drop(g);
        self.touch();
    }

    pub fn note_key_rotation(&self, provider: &str) {
        let mut g = self.lock();
        g.totals.rotations += 1;
        g.providers
            .entry(provider.to_string())
            .or_default()
            .key_rotations += 1;
    }

    pub fn note_failover_from(&self, provider: &str) {
        let mut g = self.lock();
        g.totals.failovers += 1;
        g.providers
            .entry(provider.to_string())
            .or_default()
            .failovers_out += 1;
    }
    pub fn note_failover(&self) {
        self.lock().totals.failovers += 1;
    }
    pub fn note_offline_hold(&self) {
        self.lock().totals.offline_holds += 1;
    }

    /// Record the model &ids a provider currently advertises (from the free
    /// `/v1/models` probe) so the dashboard can show a combined, live model list.
    pub fn set_models_seen(&self, provider: &str, models: Vec<String>) {
        if models.is_empty() {
            return;
        }
        let mut g = self.lock();
        let r = g.providers.entry(provider.to_string()).or_default();
        if r.models_seen != models {
            r.models_seen = models;
            drop(g);
            self.touch();
        }
    }

    /// (alive key count, total known funds) for a provider — used by logs and
    /// the dashboard summary.
    pub fn provider_summary(&self, provider: &str) -> (usize, f64) {
        let now = config::now_secs();
        let hold = self.provider(provider).map(|p| p.hold).unwrap_or(0.8);
        let pool = self.pool(provider);
        let g = self.lock();
        let mut alive = 0usize;
        let mut funds = 0.0f64;
        for k in &pool {
            if let Some(s) = g.keys.get(k) {
                if s.usable(now, hold) {
                    alive += 1;
                }
                funds += s.balance.unwrap_or(0.0);
            }
        }
        (alive, funds)
    }

    pub fn session(&self, fp: &str) -> Option<SessionState> {
        self.lock().sessions.get(fp).cloned()
    }

    /// Snapshot of sessions, for `session::resolve` to match against.
    pub fn sessions_snapshot(&self) -> HashMap<String, SessionState> {
        self.lock().sessions.clone()
    }

    /// Record how a session was &identified, folding in the message hashes used
    /// for compaction-tolerant matching.
    // Eight parameters because a session &identity genuinely has that many
    // independent facts, and every one is recorded verbatim. Grouping them into
    // a struct would move the argument list rather than shorten it.
    #[allow(clippy::too_many_arguments)]
    pub fn note_session_identity(
        &self,
        fp: &str,
        via: &str,
        compacted: bool,
        hashes: &[u64],
        client: &str,
        client_label: &str,
        api: &str,
    ) {
        let now = config::now_secs();
        let mut g = self.lock();
        let s = g
            .sessions
            .entry(fp.to_string())
            .or_insert_with(|| SessionState {
                created: now,
                ..Default::default()
            });
        s.via = via.to_string();
        // Only overwrite with a positive &identification: an "unknown" from one
        // request must not erase a good detection from an earlier one.
        if client != "unknown" || s.client.is_empty() {
            s.client = client.to_string();
            s.client_label = client_label.to_string();
        }
        if !api.is_empty() {
            s.api = api.to_string();
        }
        if compacted {
            s.compactions += 1;
        }
        crate::session::merge_hashes(&mut s.msg_hashes, hashes);
        if s.last_seen == 0 {
            s.last_seen = now;
        }
        drop(g);
        if compacted {
            self.event(
                "info",
                format!(
                    "session {} compacted — kept the same key",
                    // char-based: a header-supplied session &id could be multi-byte
                    fp.chars().take(10).collect::<String>()
                ),
            );
        }
    }

    // ── persistence ─────────────────────────────────────────────────────────

    /// Acquire an exclusive lock on the state file.
    ///
    /// Two instances sharing one state file silently corrupt each other: both
    /// load, both write, last-write-wins destroys the key pool. Observed in
    /// testing as `keys=0` in the API while startup had logged 859 keys.
    /// Binding the port only protects us when the port is the same, so the state
    /// file needs its own guard.
    ///
    /// Returns `Err(pid)` if another live process holds it.
    pub fn acquire_lock() -> Result<(), String> {
        let path = config::state_path();
        let lock = path.with_extension("lock");
        if let Some(dir) = lock.parent() {
            let _ = std::fs::create_dir_all(dir);
        }

        // A stale lock from a killed process must not block startup forever, so
        // verify the recorded pid is actually alive before refusing.
        if let Ok(text) = std::fs::read_to_string(&lock) {
            if let Ok(old) = text.trim().parse::<i32>() {
                if old != std::process::id() as i32 && pid_alive(old) {
                    return Err(format!(
                        "another keyforge (pid {old}) is already using {}",
                        path.display()
                    ));
                }
            }
        }

        std::fs::write(&lock, std::process::id().to_string())
            .map_err(|e| format!("cannot write lock {}: {e}", lock.display()))
    }

    pub fn release_lock() {
        let lock = config::state_path().with_extension("lock");
        // Only remove it if it is still ours — never clobber another instance.
        if let Ok(text) = std::fs::read_to_string(&lock) {
            if text.trim() == std::process::id().to_string() {
                let _ = std::fs::remove_file(&lock);
            }
        }
    }

    pub fn load(&self) {
        let path = config::state_path();
        let Ok(text) = std::fs::read_to_string(&path) else {
            return;
        };
        match serde_json::from_str::<Persisted>(&text) {
            Ok(mut p) if p.version == 3 => {
                // Reset volatile counters that cannot survive a restart.
                for k in p.keys.values_mut() {
                    k.inflight = 0;
                }
                let started = p.totals.started_at;
                *self.lock() = p;
                if started == 0 {
                    self.lock().totals.started_at = config::now_secs();
                }
            }
            Ok(_) => {
                // Older schema: keep nothing rather than mis-interpret fields.
                eprintln!("[state] older schema found — starting fresh");
                Self::preserve_unusable_state(&path, "schema");
            }
            Err(e) => {
                eprintln!("[state] unreadable ({e}) — starting fresh");
                Self::preserve_unusable_state(&path, "corrupt");
            }
        }
    }

    /// Move a state file we could not load aside, before anything overwrites it.
    ///
    /// Without this the evidence is destroyed within `STATE_FLUSH_SECS` (20s):
    /// `load()` starts fresh, the flusher marks state dirty, and `save()`
    /// renames a brand-new file over the unreadable one. The cause of the
    /// corruption then cannot be investigated at all — which also means an
    /// incident-capture bundle must be collected BEFORE a restart, never after.
    ///
    /// Best-effort by design: a failure here must never prevent startup, since
    /// refusing to boot over an unreadable state file would turn a recoverable
    /// annoyance into an outage.
    fn preserve_unusable_state(path: &std::path::Path, why: &str) {
        let aside = path.with_extension(format!("{why}-{}", config::now_secs()));
        match std::fs::rename(path, &aside) {
            Ok(()) => eprintln!("[state] previous file kept at {}", aside.display()),
            Err(e) => eprintln!("[state] could not preserve {}: {e}", path.display()),
        }
    }

    /// Atomic snapshot: write to a temp file then rename, so a crash mid-write
    /// can never leave a truncated state file.
    pub fn save(&self) {
        use std::sync::atomic::Ordering;
        if !self.dirty.swap(false, Ordering::Relaxed) {
            return; // nothing changed — skip the write entirely (battery)
        }
        let path = config::state_path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let snapshot = { self.lock().clone() };
        match serde_json::to_vec(&snapshot) {
            Ok(bytes) => {
                let tmp = path.with_extension("tmp");
                if std::fs::write(&tmp, &bytes).is_ok() {
                    let _ = std::fs::rename(&tmp, &path);
                }
            }
            Err(e) => eprintln!("[state] serialise failed: {e}"),
        }
    }
}

/// Age-decayed failure streak. Shared by scoring and the dashboard so both agree.
fn decayed_streak(rt: Option<&ProviderRuntime>, now: u64, halflife: u64) -> f64 {
    let Some(r) = rt else { return 0.0 };
    if r.consecutive_fails == 0 {
        return 0.0;
    }
    let age = now.saturating_sub(r.last_fail);
    if age <= halflife {
        return r.consecutive_fails as f64;
    }
    let d = r.consecutive_fails as f64 * 0.5_f64.powf(age as f64 / halflife.max(1) as f64);
    if d < 0.05 {
        0.0
    } else {
        d
    }
}

/// Age-decayed slow streak, mirroring `decayed_streak`.
fn decayed_slow(rt: Option<&ProviderRuntime>, now: u64, halflife: u64) -> f64 {
    let Some(r) = rt else { return 0.0 };
    if r.slow_streak == 0 {
        return 0.0;
    }
    let age = now.saturating_sub(r.last_fail);
    if age <= halflife {
        return r.slow_streak as f64;
    }
    let d = r.slow_streak as f64 * 0.5_f64.powf(age as f64 / halflife.max(1) as f64);
    if d < 0.05 {
        0.0
    } else {
        d
    }
}

/// Age-decayed error rate.
fn decayed_err(rt: Option<&ProviderRuntime>, now: u64, halflife: u64) -> f64 {
    let Some(r) = rt else { return 0.0 };
    let base = r.ewma_err.unwrap_or(0.0);
    if base <= 0.0 {
        return 0.0;
    }
    let age = now.saturating_sub(r.last_fail);
    if age <= halflife {
        return base;
    }
    let d = base * 0.5_f64.powf(age as f64 / halflife.max(1) as f64);
    if d < 0.01 {
        0.0
    } else {
        d
    }
}

/// Mask a key for display. Never let a full secret reach the dashboard or logs.
pub fn mask_key(k: &str) -> String {
    // Char-based, not byte-based: byte slicing panics on a multi-byte boundary,
    // and with `panic = "abort"` that takes the whole gateway down.
    let n = k.chars().count();
    let prefix = crate::config::key_prefix();
    if n < 14 {
        return format!("{}…", prefix);
    }
    let head: String = k.chars().take(10).collect();
    let tail: String = k.chars().skip(n - 4).collect();
    format!("{head}…{tail}")
}

/// Is this pid still running? Uses signal 0, which checks existence without
/// actually delivering a signal.
fn pid_alive(pid: i32) -> bool {
    // /proc is the most portable check available on Android/Termux without libc.
    std::path::Path::new(&format!("/proc/{pid}")).exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app_with(balances: &[(&str, Option<f64>)]) -> App {
        let app = App::new();
        {
            let mut g = app.lock();
            for (k, b) in balances {
                let mut st = KeyState::new("tabi", 120.0);
                st.balance = *b;
                if b.is_some() {
                    st.usage_cents = Some((120.0 - b.unwrap()) * 100.0);
                }
                g.keys.insert(k.to_string(), st);
            }
        }
        app.pools.lock().unwrap().insert(
            "tabi".into(),
            balances.iter().map(|(k, _)| k.to_string()).collect(),
        );
        app
    }

    #[test]
    fn drain_first_picks_lowest_usable_balance() {
        // 5.0 clears 0.80*2.5 = 2.0, so it should win over the richer keys.
        let app = app_with(&[
            ("sk-rich0000000000000000", Some(100.0)),
            ("sk-low00000000000000000", Some(5.0)),
            ("sk-mid00000000000000000", Some(40.0)),
        ]);
        let c = app.candidates("tabi", "fp1", &[]);
        assert_eq!(c.first().unwrap(), "sk-low00000000000000000");
    }

    #[test]
    fn skips_key_below_hold() {
        let app = app_with(&[
            ("sk-empty000000000000000", Some(0.10)), // < 0.80 hold
            ("sk-ok00000000000000000x", Some(50.0)),
        ]);
        let c = app.candidates("tabi", "fp1", &[]);
        assert!(!c.contains(&"sk-empty000000000000000".to_string()));
        assert_eq!(c.len(), 1);
    }

    #[test]
    fn probed_keys_rank_above_unprobed() {
        let app = app_with(&[
            ("sk-unprobed00000000000x", None),
            ("sk-probed0000000000000x", Some(90.0)),
        ]);
        let c = app.candidates("tabi", "fp1", &[]);
        assert_eq!(c.first().unwrap(), "sk-probed0000000000000x");
    }

    #[test]
    fn excluded_keys_are_not_returned() {
        let app = app_with(&[
            ("sk-a000000000000000000x", Some(10.0)),
            ("sk-b000000000000000000x", Some(20.0)),
        ]);
        let c = app.candidates("tabi", "fp1", &["sk-a000000000000000000x".to_string()]);
        assert_eq!(c, vec!["sk-b000000000000000000x".to_string()]);
    }

    #[test]
    fn dead_key_is_excluded_until_ttl_expires() {
        let app = app_with(&[("sk-dead00000000000000x0", Some(50.0))]);
        app.mark_dead("sk-dead00000000000000x0", 3600, "quota");
        assert!(app.candidates("tabi", "fp1", &[]).is_empty());
    }

    #[test]
    fn active_session_keeps_its_key_away_from_others() {
        let app = app_with(&[
            ("sk-owned00000000000000x", Some(5.0)),
            ("sk-free000000000000000x", Some(60.0)),
        ]);
        {
            let mut g = app.lock();
            g.sessions.insert(
                "other-fp".into(),
                SessionState {
                    key: "sk-owned00000000000000x".into(),
                    provider: "tabi".into(),
                    last_seen: config::now_secs(),
                    ..Default::default()
                },
            );
        }
        let c = app.candidates("tabi", "my-fp", &[]);
        // Even though the owned key has a lower balance (drain-first would
        // prefer it), isolation must win so two agents don't share a key.
        assert_eq!(c.first().unwrap(), "sk-free000000000000000x");
    }

    #[test]
    fn quota_403_pins_exact_initial_balance() {
        let app = app_with(&[("sk-learn0000000000000x0", None)]);
        app.set_usage("sk-learn0000000000000x0", 12640.0); // $126.40 spent
        app.learn_from_quota("sk-learn0000000000000x0", Some(0.495838));
        let g = app.lock();
        let k = g.keys.get("sk-learn0000000000000x0").unwrap();
        assert!(k.initial_exact);
        // 0.495838 + 126.40 = 126.895838, matching the observed $120-128 range
        assert!((k.initial - 126.895838).abs() < 1e-6, "got {}", k.initial);
    }

    #[test]
    fn billed_cost_reduces_balance_between_probes() {
        let app = app_with(&[("sk-spend0000000000000x0", Some(10.0))]);
        app.record_request(
            "fp",
            "",
            "tabi",
            "sk-spend0000000000000x0",
            "claude-opus-5",
            true,
            900,
            1000,
            2000,
            0.50,
            100,
            20,
        );
        let g = app.lock();
        let k = g.keys.get("sk-spend0000000000000x0").unwrap();
        assert!(
            (k.balance.unwrap() - 9.5).abs() < 1e-6,
            "got {:?}",
            k.balance
        );
        assert_eq!(k.turns, 1);
    }

    #[test]
    fn metrics_aggregate_across_providers_per_model() {
        let app = app_with(&[("sk-m000000000000000000x", Some(50.0))]);
        app.record_request(
            "f1",
            "",
            "tabi",
            "sk-m000000000000000000x",
            "claude-opus-5",
            true,
            100,
            1,
            1,
            0.1,
            5,
            5,
        );
        app.record_request(
            "f2",
            "",
            "gorouter",
            "sk-m000000000000000000x",
            "claude-opus-5",
            true,
            200,
            1,
            1,
            0.2,
            5,
            5,
        );
        let g = app.lock();
        let m = g.models.get("claude-opus-5").unwrap();
        assert_eq!(m.requests, 2);
        assert_eq!(m.by_provider.len(), 2);
        assert!((m.cost - 0.3).abs() < 1e-9);
    }

    #[test]
    fn offline_flag_transitions_emit_one_event_each() {
        let app = App::new();
        app.set_offline(true);
        app.set_offline(true); // duplicate must not add another event
        app.set_offline(false);
        let g = app.lock();
        assert_eq!(g.events.len(), 2);
    }

    // ── smart routing ───────────────────────────────────────────────────────

    /// Give every provider one funded key so `provider_order` has candidates.
    fn app_all_providers() -> App {
        let app = App::new();
        {
            let mut g = app.lock();
            for p in &*app.providers.read().unwrap() {
                let key = format!("sk-{}0000000000000000000", p.id);
                let mut st = KeyState::new(p.id, p.initial_guess);
                st.usage_cents = Some(0.0);
                st.recompute();
                g.keys.insert(key.clone(), st);
                app.pools
                    .lock()
                    .unwrap()
                    .insert(p.id.to_string(), vec![key]);
            }
        }
        app
    }

    #[test]
    fn faster_provider_is_preferred() {
        let app = app_all_providers();
        app.note_latency("tabi", 4000);
        app.note_latency("gorouter", 900);
        app.note_latency("justwoker", 2000);
        assert_eq!(app.provider_order(None).first().unwrap(), "gorouter");
    }

    #[test]
    fn error_rate_outweighs_a_small_latency_win() {
        // A &host that is slightly faster but frequently fails must lose: a retry
        // costs a whole round trip, which the user actually feels.
        let app = app_all_providers();
        app.note_latency("tabi", 1000);
        for _ in 0..6 {
            app.note_provider_fail("tabi");
        }
        app.note_latency("gorouter", 1400);
        let order = app.provider_order(None);
        assert_eq!(order.first().unwrap(), "gorouter", "order was {order:?}");
    }

    #[test]
    fn three_consecutive_failures_open_the_breaker() {
        let app = app_all_providers();
        assert!(!app.breaker_open("tabi"));
        app.note_provider_fail("tabi");
        app.note_provider_fail("tabi");
        assert!(!app.breaker_open("tabi"), "must tolerate transient blips");
        app.note_provider_fail("tabi");
        assert!(app.breaker_open("tabi"), "3 in a row is an outage");
    }

    #[test]
    fn success_closes_the_breaker_immediately() {
        let app = app_all_providers();
        for _ in 0..5 {
            app.note_provider_fail("tabi");
        }
        assert!(app.breaker_open("tabi"));
        app.note_latency("tabi", 800);
        assert!(!app.breaker_open("tabi"), "recovery must be automatic");
    }

    #[test]
    fn open_breaker_sinks_provider_to_the_back() {
        let app = app_all_providers();
        // tabi is by far the fastest, but broken.
        app.note_latency("tabi", 100);
        app.note_latency("gorouter", 3000);
        app.note_latency("justwoker", 3000);
        for _ in 0..3 {
            app.note_provider_fail("tabi");
        }
        let order = app.provider_order(None);
        assert_eq!(order.last().unwrap(), "tabi", "order was {order:?}");
    }

    #[test]
    fn provider_without_the_model_is_dropped_when_someone_has_it() {
        let app = app_all_providers();
        app.note_latency("tabi", 200); // fastest, but lacks the model
        // Set models_seen for ALL providers dynamically
        for p in &*app.providers.read().unwrap() {
            if p.id == "gorouter" {
                app.set_models_seen(&p.id, vec!["claude-opus-5".into()]);
            } else {
                app.set_models_seen(&p.id, vec!["claude-opus-4-8".into()]);
            }
        }

        let order = app.provider_order_for(None, "claude-opus-5");
        assert_eq!(order.first().unwrap(), "gorouter", "order was {order:?}");
        assert!(!order.contains(&"tabi".to_string()), "tabi lacks the model, order was {order:?}");
    }

    #[test]
    fn unknown_model_lists_still_try_everyone() {
        // If nobody advertises the model, lists are stale — keep the old
        // penalty behaviour and try everyone rather than returning empty.
        let app = app_all_providers();
        let n = app.providers.read().unwrap().len();
        for p in &*app.providers.read().unwrap() {
            app.set_models_seen(&p.id, vec!["a".into()]);
        }
        let order = app.provider_order_for(None, "brand-new-model");
        assert_eq!(order.len(), n, "order was {order:?}");
    }

    #[test]
    fn sticky_preferred_provider_lacking_model_does_not_win() {
        // Session stickiness must not override model support: a session pinned
        // to tabi asking for a justwoker-only model goes to justwoker.
        let app = app_all_providers();
        app.set_models_seen("tabi", vec!["claude-opus-4-8".into()]);
        app.set_models_seen("gorouter", vec!["claude-opus-4-8".into()]);
        app.set_models_seen("justwoker", vec!["just-new-model".into()]);
        let order = app.provider_order_for(Some("tabi"), "just-new-model");
        assert_eq!(order.first().unwrap(), "justwoker", "order was {order:?}");
        assert!(!order.contains(&"tabi".to_string()));
    }

    #[test]
    fn dry_provider_is_dropped_while_a_funded_one_remains() {
        // Previously a dry provider was merely sorted last, so the ladder still
        // spent a rung per round discovering "no funded key available" — on the
        // failed 1M request justwoker (0 funded, $0) did that 12 times. It is now
        // excluded outright, as long as somebody funded is left to serve.
        let app = app_all_providers();
        // Drain all providers' keys except the last one (which stays funded).
        let ids: Vec<String> = app.providers.read().unwrap().iter().map(|p| p.id.to_string()).collect();
        for id in &ids[..ids.len()-1] {
            for k in app.pool(id) {
                app.mark_dead(&k, 3600, "quota");
            }
        }
        let order = app.provider_order(None);
        assert_eq!(order.first().unwrap(), ids.last().unwrap(), "order was {order:?}");
        assert!(
            !order.contains(&ids[0]),
            "a provably dry provider must not cost a rung, order was {order:?}"
        );
    }

    #[test]
    fn all_providers_dry_still_returns_them_all() {
        // The exclusion must never empty the ladder: with nobody funded, the
        // router has to reach its normal "no funded key" reporting path instead
        // of the misleading "no providers configured" terminal error.
        let app = app_all_providers();
        let n = app.providers.read().unwrap().len();
        for p in &*app.providers.read().unwrap() {
            for k in app.pool(&p.id) {
                app.mark_dead(&k, 3600, "quota");
            }
        }
        let order = app.provider_order(None);
        assert_eq!(order.len(), n, "order was {order:?}");
    }

    #[test]
    fn a_refunded_provider_returns_to_the_ladder() {
        // `usable()` is optimistic for unprobed keys and the sweeper keeps
        // re-probing, so exclusion must be a live read of key state, not sticky.
        let app = app_all_providers();
        let key = app.pool("justwoker")[0].clone();
        app.mark_dead(&key, 3600, "quota");
        assert!(!app.provider_order(None).contains(&"justwoker".to_string()));
        app.mark_dead(&key, 0, "revived"); // cooldown elapsed
        assert!(
            app.provider_order(None).contains(&"justwoker".to_string()),
            "provider must come back once a key is usable again"
        );
    }

    #[test]
    fn pinned_dry_provider_does_not_resurrect_itself() {
        // `preferred` moves a provider to the front only if it is still in the
        // ladder; stickiness must not override "this provider cannot serve".
        let app = app_all_providers();
        for k in app.pool("justwoker") {
            app.mark_dead(&k, 3600, "quota");
        }
        let order = app.provider_order(Some("justwoker"));
        assert!(!order.is_empty());
        assert_ne!(order.first().unwrap(), "justwoker", "order was {order:?}");
    }

    #[test]
    fn pinned_provider_always_wins() {
        // Session stickiness beats a latency win: switching &hosts mid-conversation
        // means a new key and a new account wallet.
        let app = app_all_providers();
        app.note_latency("tabi", 5000);
        app.note_latency("gorouter", 100);
        assert_eq!(app.provider_order(Some("tabi")).first().unwrap(), "tabi");
    }

    #[test]
    fn billing_delta_attributes_spend_to_key_session_and_provider() {
        // `/v1/chat/completions` returns no `usage.cost`, so spend can only be
        // learned from the billing endpoint delta.
        let app = app_all_providers();
        let key = app.pool("justwoker")[0].clone();
        {
            let mut g = app.lock();
            g.sessions.insert(
                "fp-a".into(),
                SessionState {
                    key: key.clone(),
                    provider: "justwoker".into(),
                    last_seen: config::now_secs(),
                    ..Default::default()
                },
            );
        }
        app.set_usage(&key, 0.0);
        app.set_usage(&key, 250.0); // +$2.50 spent

        let g = app.lock();
        assert!(
            (g.totals.cost - 2.5).abs() < 1e-9,
            "totals {}",
            g.totals.cost
        );
        assert!((g.sessions["fp-a"].cost - 2.5).abs() < 1e-9);
        assert!((g.keys[&key].spent - 2.5).abs() < 1e-9);
        assert!((g.providers["justwoker"].cost - 2.5).abs() < 1e-9);
    }

    #[test]
    fn billing_delta_ignores_a_decrease() {
        // A top-up resets total_usage; that must not read as negative spend.
        let app = app_all_providers();
        let key = app.pool("tabi")[0].clone();
        app.set_usage(&key, 500.0);
        let before = app.lock().totals.cost;
        app.set_usage(&key, 100.0);
        assert!((app.lock().totals.cost - before).abs() < 1e-9);
    }

    #[test]
    fn response_cost_and_billing_delta_do_not_double_count() {
        let app = app_all_providers();
        let key = app.pool("tabi")[0].clone();
        app.set_usage(&key, 0.0);
        // Anthropic path reports cost directly.
        app.record_request(
            "fp",
            "",
            "tabi",
            &key,
            "claude-opus-5",
            true,
            900,
            10,
            20,
            1.0,
            5,
            5,
        );
        let after_response = app.lock().totals.cost;
        assert!((after_response - 1.0).abs() < 1e-9);
        // The next probe sees usage that already includes it; no second charge.
        app.set_usage(&key, 100.0);
        let after_probe = app.lock().totals.cost;
        assert!(
            (after_probe - 1.0).abs() < 1e-9,
            "double counted: {after_probe}"
        );
    }

    // ── failure decay (the self-reinforcing-penalty fix) ────────────────────

    #[test]
    fn a_fresh_streak_counts_at_full_weight() {
        let app = app_all_providers();
        for _ in 0..3 {
            app.note_provider_fail("tabi");
        }
        assert!((app.effective_streak("tabi", 120) - 3.0).abs() < 1e-9);
    }

    #[test]
    fn an_old_streak_decays_away() {
        // The exact live bug: gorouter held consecutive_fails=3 for 32 minutes
        // while every free uptime probe passed, so it was never selected and the
        // streak could never clear.
        let app = app_all_providers();
        for _ in 0..3 {
            app.note_provider_fail("gorouter");
        }
        {
            let mut g = app.lock();
            let r = g.providers.get_mut("gorouter").unwrap();
            r.last_fail = config::now_secs() - 1915; // as observed
        }
        let s = app.effective_streak("gorouter", 120);
        assert!(s < 0.05, "streak should have decayed to nothing, got {s}");
        let e = app.effective_error_rate("gorouter", 120);
        assert!(e < 0.01, "error rate should have decayed, got {e}");
    }

    #[test]
    fn decayed_provider_is_selectable_again() {
        let app = app_all_providers();
        app.note_latency("gorouter", 900);
        app.note_latency("tabi", 3000);
        app.note_latency("justwoker", 3000);
        for _ in 0..3 {
            app.note_provider_fail("gorouter");
        }
        // Immediately after failing, gorouter must NOT be first.
        assert_ne!(app.provider_order(None).first().unwrap(), "gorouter");
        // Age the failure out and clear the breaker as time would.
        {
            let mut g = app.lock();
            let r = g.providers.get_mut("gorouter").unwrap();
            r.last_fail = config::now_secs() - 1800;
            r.open_until = 0;
        }
        assert_eq!(
            app.provider_order(None).first().unwrap(),
            "gorouter",
            "a provider whose failures aged out must be usable again"
        );
    }

    #[test]
    fn free_probe_heals_the_score() {
        let app = app_all_providers();
        for _ in 0..4 {
            app.note_provider_fail("tabi");
        }
        assert!(app.breaker_open("tabi"));
        app.probe_healed("tabi");
        assert!(!app.breaker_open("tabi"));
        assert!((app.effective_streak("tabi", 120)).abs() < 1e-9);
    }

    #[test]
    fn provider_bias_from_settings_shifts_the_order() {
        let app = app_all_providers();
        app.note_latency("tabi", 2000);
        app.note_latency("gorouter", 1000);
        app.note_latency("justwoker", 3000);
        assert_eq!(app.provider_order(None).first().unwrap(), "gorouter");
        {
            let mut s = app.settings.lock().unwrap();
            s.providers
                .iter_mut()
                .find(|p| p.id == "tabi")
                .unwrap()
                .bias = 5.0;
        }
        assert_eq!(
            app.provider_order(None).first().unwrap(),
            "tabi",
            "a positive bias must be able to override latency"
        );
    }

    // ── error log ───────────────────────────────────────────────────────────

    fn err_rec(class: &str, provider: &str) -> ErrorRecord {
        ErrorRecord {
            t: config::now_secs(),
            class: class.into(),
            provider: provider.into(),
            host: "example.com".into(),
            key: "sk-abcdefghij…wxyz".into(),
            model: "claude-opus-5".into(),
            status: 403,
            message: "预扣费额度失败".into(),
            action: "rotated-key".into(),
            session: "fp1".into(),
            round: 1,
            latency_ms: 800,
            remaining: Some(0.08),
            required: Some(0.8),
            ..Default::default()
        }
    }

    #[test]
    fn recording_an_error_also_creates_an_event() {
        // The Events tab was empty while the dashboard reported a 25% error rate,
        // because failures were counted but never narrated.
        let app = App::new();
        app.record_error(err_rec("QUOTA", "tabi"));
        let g = app.lock();
        assert_eq!(g.errors_log.len(), 1);
        assert_eq!(g.events.len(), 1, "an error must surface in the event feed");
        assert_eq!(g.events[0].level, "error");
        assert!(g.events[0].msg.contains("QUOTA"));
    }

    #[test]
    fn error_log_is_a_bounded_ring_buffer() {
        let app = App::new();
        for i in 0..600 {
            let mut r = err_rec("WAF", "gorouter");
            r.round = i;
            app.record_error(r);
        }
        let g = app.lock();
        assert_eq!(g.errors_log.len(), 500, "must cap at 500");
        assert_eq!(g.events.len(), 200, "events cap independently");
        assert_eq!(g.errors_log.back().unwrap().round, 599, "keeps the newest");
    }

    #[test]
    fn recent_errors_returns_newest_first() {
        let app = App::new();
        let mut a = err_rec("AUTH", "tabi");
        a.round = 1;
        let mut b = err_rec("RATE", "tabi");
        b.round = 2;
        app.record_error(a);
        app.record_error(b);
        let out = app.recent_errors(10);
        assert_eq!(out[0].round, 2, "newest first");
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn error_record_never_holds_a_full_key() {
        let app = App::new();
        app.record_error(err_rec("QUOTA", "tabi"));
        let g = app.lock();
        let k = &g.errors_log[0].key;
        assert!(k.contains('…'), "key must be masked, got {k}");
    }

    // ── TTFB vs total duration (leaderboard correctness) ────────────────────

    #[test]
    fn routing_uses_ttfb_not_total_duration() {
        // The bug this fixes: a provider that answers instantly but whose MODEL
        // thinks for 55s was ranked as "slow", making the leaderboard meaningless.
        let app = app_all_providers();
        let min = { app.settings.lock().unwrap().routing.min_samples };
        for _ in 0..min {
            // tabi: fast to first byte, long total (thinking model)
            app.note_latency_split("tabi", 400, 55_000);
            // gorouter: slow to first byte, short total
            app.note_latency_split("gorouter", 9_000, 9_500);
            app.note_latency_split("justwoker", 9_000, 9_500);
        }
        assert_eq!(
            app.provider_order(None).first().unwrap(),
            "tabi",
            "must rank on TTFB, not total duration"
        );
        let g = app.lock();
        let r = g.providers.get("tabi").unwrap();
        assert!(r.ewma_ttfb_ms.unwrap() < 1000.0);
        assert!(
            r.ewma_ms.unwrap() > 40_000.0,
            "total duration still recorded"
        );
    }

    #[test]
    fn a_single_unlucky_sample_does_not_demote_a_provider() {
        // Below min_samples the reading is blended toward neutral, so one bad
        // measurement cannot bury a provider that has barely been tried.
        let app = app_all_providers();
        app.note_latency_split("tabi", 20_000, 20_000); // 1 sample, terrible
        let min = { app.settings.lock().unwrap().routing.min_samples };
        for _ in 0..(min + 2) {
            app.note_latency_split("gorouter", 1_400, 1_400);
        }
        let order = app.provider_order(None);
        assert_eq!(order.first().unwrap(), "gorouter");
        // justwoker is entirely unmeasured and must still beat a confidently-slow
        // provider, so exploration remains possible.
        assert!(
            order.iter().position(|x| x == "justwoker") < order.iter().position(|x| x == "tabi"),
            "unmeasured should outrank confidently-slow, got {order:?}"
        );
    }

    // ── client-level vs attempt-level accounting ────────────────────────────

    #[test]
    fn client_success_rate_is_not_polluted_by_retry_attempts() {
        // The leaderboard showed 0% success because one client request burning six
        // keys logged six requests AND six errors.
        let app = app_all_providers();
        let key = app.pool("gorouter")[0].clone();
        app.note_client_request();
        // five failed attempts, then one success — ONE client request
        for _ in 0..5 {
            app.record_request(
                "fp", "", "gorouter", &key, "m", false, 100, 10, 0, 0.0, 0, 0,
            );
        }
        app.record_request(
            "fp", "", "gorouter", &key, "m", true, 100, 10, 20, 0.01, 5, 5,
        );
        app.note_client_outcome("gorouter", true, true);

        let g = app.lock();
        let r = g.providers.get("gorouter").unwrap();
        assert_eq!(r.client_requests, 1, "one client request");
        assert_eq!(r.client_failures, 0, "the user saw no failure");
        assert_eq!(r.requests, 6, "six attempts, tracked separately");
        assert_eq!(g.totals.client_requests, 1);
        assert_eq!(g.totals.client_saved, 1, "counted as a save");
    }

    #[test]
    fn key_rotation_is_attributed_to_its_provider() {
        let app = app_all_providers();
        app.note_key_rotation("tabi");
        app.note_key_rotation("tabi");
        app.note_failover_from("tabi");
        let g = app.lock();
        assert_eq!(g.providers.get("tabi").unwrap().key_rotations, 2);
        assert_eq!(g.providers.get("tabi").unwrap().failovers_out, 1);
        assert_eq!(g.totals.rotations, 2);
        assert_eq!(
            g.totals.failovers, 1,
            "failover count must no longer be stuck at 0"
        );
    }

    #[test]
    fn a_rescue_credits_the_provider_that_finished_the_job() {
        let app = app_all_providers();
        app.note_client_request();
        app.note_failover_from("tabi");
        app.note_client_outcome("gorouter", true, true);
        let g = app.lock();
        assert_eq!(g.providers.get("gorouter").unwrap().rescues, 1);
        assert_eq!(g.providers.get("tabi").unwrap().failovers_out, 1);
    }

    #[test]
    fn terminal_failure_counts_once_against_the_client() {
        let app = app_all_providers();
        app.note_client_request();
        app.note_client_outcome("tabi", false, false);
        let g = app.lock();
        assert_eq!(g.totals.client_failures, 1);
        assert_eq!(g.totals.client_saved, 0);
    }

    // ── size-aware head budget (the 1M-context outage) ───────────────────────

    #[test]
    fn head_budget_grows_with_request_size() {
        // The bug: one scalar EWMA per provider, so a 281KB request and a 2.7MB
        // request got the &identical budget. Measured direct to gorouter with a
        // real key:
        //   281KB -> TTFB 11.9s (passed on 45s)
        //   2.7MB -> TTFB 95.3s, then 99.4s on repeat (guaranteed to fail on 45s)
        // The budget must therefore be a function of THIS request's size.
        let app = app_all_providers();
        for _ in 0..4 {
            app.note_latency_split("tabi", 14_400, 60_000); // live EWMA was 14.4s
        }
        let small = app.head_budget_secs("tabi", 281_305);
        let large = app.head_budget_secs("tabi", 2_702_917);
        assert!(
            large > small,
            "big body must get a bigger budget ({small}s vs {large}s)"
        );
    }

    #[test]
    fn one_megabyte_body_clears_the_measured_requirement() {
        // The whole point. 2.70MB was measured at 95.3s and 99.4s TTFB direct,
        // plus ~13s of gateway buffer-and-forward overhead (paired A/B on a
        // 952KB body: direct mean 42.9s vs gateway mean 55.8s), so the real
        // requirement through this gateway is ~110s with a tail near 140s.
        //
        // An earlier revision asserted only `>= 60`, which passed against a
        // budget that could never have served the request: the old 130s ceiling
        // clamped it and it completed solely because failover to a second
        // provider bought extra wall-clock. The floor here is the measured need,
        // not the old extrapolation.
        let app = app_all_providers();
        for _ in 0..4 {
            app.note_latency_split("tabi", 14_400, 60_000);
        }
        let b = app.head_budget_secs("tabi", 2_702_917);
        assert!(
            b >= 110,
            "1M context must clear its measured ~110s need through the gateway, got {b}s"
        );
        assert!(
            b <= config::head_max_secs(),
            "must not exceed the edge ceiling, got {b}s"
        );
    }

    #[test]
    fn small_request_still_fails_over_fast() {
        // The size term must not inflate ordinary turns into slow failovers:
        // a tight budget on a small body is a feature, not collateral damage.
        let app = app_all_providers();
        for _ in 0..4 {
            app.note_latency_split("tabi", 2_000, 8_000);
        }
        assert_eq!(
            app.head_budget_secs("tabi", 4_096),
            config::head_min_secs(),
            "a 4KB body on a fast provider stays at the floor"
        );
    }

    #[test]
    fn head_budget_is_monotonic_in_size() {
        // Guards against a sign/units slip in the size term.
        let app = app_all_providers();
        for _ in 0..4 {
            app.note_latency_split("gorouter", 17_600, 60_000);
        }
        let mut prev = 0u64;
        for bytes in [0u64, 100_000, 500_000, 1_000_000, 2_000_000, 3_000_000] {
            let b = app.head_budget_secs("gorouter", bytes);
            assert!(
                b >= prev,
                "budget must not shrink as body grows ({bytes} -> {b}s)"
            );
            prev = b;
        }
    }

    #[test]
    fn absurd_body_is_capped_at_the_edge_ceiling() {
        // 50MB must not produce a 1000s budget: past the Cloudflare edge the
        // extra wait buys nothing and just burns the request deadline.
        let app = app_all_providers();
        for _ in 0..4 {
            app.note_latency_split("tabi", 14_400, 60_000);
        }
        assert_eq!(
            app.head_budget_secs("tabi", 50 * 1024 * 1024),
            config::head_max_secs()
        );
    }

    #[test]
    fn unmeasured_provider_still_gets_size_room() {
        // A provider with no samples uses the default as its base term, but the
        // size term must still apply — otherwise the very first 1M request to a
        // fresh provider repeats the original bug.
        let app = app_all_providers();
        let cold_small = app.head_budget_secs("justwoker", 0);
        let cold_large = app.head_budget_secs("justwoker", 2_702_917);
        assert_eq!(cold_small, config::head_default_secs());
        assert!(cold_large > cold_small, "{cold_small}s vs {cold_large}s");
    }

    #[test]
    fn head_budget_adapts_to_measured_ttfb() {
        // TTFB here ranged from ~2s (small context) to ~80s (800k tokens).
        // A fixed timeout is wrong for one end or the other, so the budget
        // must track the provider's own measurements.
        let app = app_all_providers();
        assert_eq!(
            app.head_budget_secs("tabi", 0),
            config::head_default_secs(),
            "unmeasured provider gets the generous default"
        );
        // One sample is not policy: still default.
        app.note_latency_split("tabi", 2000, 2000);
        assert_eq!(app.head_budget_secs("tabi", 0), config::head_default_secs());
        // Two samples: 3x EWMA of ~2s = ~6s, clamped up to the floor.
        app.note_latency_split("tabi", 2000, 2000);
        assert_eq!(
            app.head_budget_secs("tabi", 0),
            config::head_min_secs(),
            "fast provider fails over fast"
        );
    }

    #[test]
    fn head_budget_gives_slow_provider_room_but_never_past_the_edge() {
        let app = app_all_providers();
        // Simulate the live 63-80s TTFB measurements.
        for _ in 0..6 {
            app.note_latency_split("gorouter", 70_000, 300_000);
        }
        let b = app.head_budget_secs("gorouter", 0);
        // 3x of ~70s = ~210s, capped at the edge ceiling. Asserted against
        // `head_max_secs()` rather than a literal so raising the ceiling (130 ->
        // 170 when 1M measurements landed) does not silently invalidate it.
        assert_eq!(
            b,
            config::head_max_secs(),
            "slow provider gets room up to the edge ceiling, got {b}"
        );
    }

    #[test]
    fn head_budget_tracks_recovery_downward() {
        // EWMA must pull the budget back down when the provider gets fast
        // again, otherwise one bad hour inflates every future budget.
        let app = app_all_providers();
        for _ in 0..6 {
            app.note_latency_split("tabi", 60_000, 200_000);
        }
        let slow = app.head_budget_secs("tabi", 0);
        for _ in 0..10 {
            app.note_latency_split("tabi", 3_000, 10_000);
        }
        let fast = app.head_budget_secs("tabi", 0);
        assert!(
            fast < slow,
            "budget must shrink as TTFB recovers ({slow} -> {fast})"
        );
    }

    #[test]
    fn observed_hold_overrides_configured_default() {
        let app = app_all_providers();
        assert!((app.hold_for("tabi", "claude-opus-5") - 0.80).abs() < 1e-9);
        app.learn_hold("tabi", "claude-opus-5", 2.4);
        assert!((app.hold_for("tabi", "claude-opus-5") - 2.4).abs() < 1e-9);
        // Never below the configured floor.
        app.learn_hold("tabi", "claude-opus-5", 0.1);
        assert!((app.hold_for("tabi", "claude-opus-5") - 2.4).abs() < 1e-9);
    }
}
