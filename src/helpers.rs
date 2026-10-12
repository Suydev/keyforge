//! Background helpers. All of them are deliberately cheap — the battery target
//! is <=5%/hour on a OnePlus Pad Go, so nothing here polls aggressively and
//! nothing here spends credits.
//!
//! * `uptime_prober`  — provider availability via FREE `/v1/models`
//! * `balance_sweeper` — key balances via FREE `/v1/dashboard/billing/usage`
//! * `key_sync`        — pulls new keys from the GitHub repo every 4h
//! * `state_flusher`   — coalesced atomic snapshots of state
//! * `net_watchdog`    — flips the offline flag so requests wait instead of failing

use crate::config;
use crate::state::App;
use crate::upstream;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

/// Provider uptime WITHOUT spending credits.
///
/// `GET /v1/models` is free on every new-api deployment (verified on keyforge-token,
/// gorouter, justwoker and seekai) and proves two things at once: the &host is
/// reachable, and the key authenticates. A "hi" completion would prove the same
/// while burning the pre-deduction hold on every probe.
///
/// # Distinguishing "provider down" from "we have no internet"
/// This matters, because drawing a red bar for our own dead wifi would be a lie
/// about the provider. The rules:
///   * a transport failure classified `Offline` (DNS/no route) records NOTHING
///     and instead flips the global offline flag;
///   * a single failure is not enough — `UPTIME_FAIL_THRESHOLD` consecutive
///     failures are required before a sample is written as down, with a fast
///     retry in between, so one dropped packet cannot paint an outage;
///   * a reachable-but-unhappy response (401/403) still counts as UP, because
///     the &host answered.
pub fn spawn_uptime_prober(app: Arc<App>) {
    tokio::spawn(async move {
        // Small initial delay so startup isn't a thundering herd.
        tokio::time::sleep(Duration::from_secs(3)).await;

        // provider &id -> consecutive failed probes not yet recorded
        let mut pending_fails: HashMap<String, u32> = HashMap::new();
        // Set when any provider failed this round, so we retry sooner.
        let mut retry_soon;

        loop {
            retry_soon = false;

            // If the device has no network at all, do not probe: it would just
            // manufacture failures for every provider. Wait for the watchdog.
            if app.is_offline() {
                tokio::time::sleep(Duration::from_secs(10)).await;
                continue;
            }

            // Samples staged for this round rather than written immediately.
            //
            // A red bar must mean "the provider was down", never "my wifi was
            // off". A single provider's error is not enough to tell those apart,
            // so nothing is recorded until the whole round is in: if EVERY
            // provider failed, that is corroboration of a local network fault and
            // the round is discarded.
            let mut staged: Vec<(String, bool, u32)> = Vec::new();
            let mut probed = 0usize;
            let mut failed = 0usize;
            let mut offline_signal = false;

            let providers = app.providers.read().unwrap().clone();
            for p in providers {
                let pool = app.pool(p.id);
                let key = {
                    let now = config::now_secs();
                    let g = app.lock();
                    pool.iter()
                        .find(|k| g.keys.get(*k).map(|s| s.dead_until <= now).unwrap_or(false))
                        .cloned()
                        .or_else(|| pool.first().cloned())
                };
                let Some(key) = key else { continue };
                probed += 1;

                match upstream::probe_models(p.host, &key).await {
                    upstream::Attempt::Ok(r) if (200..300).contains(&r.status) => {
                        let models = upstream::parse_model_ids(&r.body);
                        staged.push((p.id.to_string(), true, r.latency_ms as u32));
                        app.set_models_seen(p.id, models);
                        // A single success proves the uplink is fine.
                        app.set_offline(false);
                        // A free successful probe is a valid recovery signal: it
                        // proves the &host is reachable, so clear any stale failure
                        // streak. Without this the penalty is self-reinforcing.
                        let heals = {
                            let s = app.settings.lock().unwrap_or_else(|e| e.into_inner());
                            s.routing.probe_heals_score
                        };
                        if heals {
                            app.probe_healed(p.id);
                        }
                        // Recovery: announce it if we had been counting failures.
                        if pending_fails.remove(p.id).unwrap_or(0) > 0 {
                            app.event("info", format!("{} is reachable again", p.label));
                        }
                    }
                    upstream::Attempt::Ok(r) => {
                        // The &host answered, so it is UP even if it disliked the
                        // key. Recording this as downtime would be wrong.
                        let up = r.status == 401 || r.status == 403 || r.status < 500;
                        if up {
                            staged.push((p.id.to_string(), true, r.latency_ms as u32));
                            pending_fails.remove(p.id);
                        } else {
                            failed += 1;
                            let n = pending_fails.entry(p.id.to_string()).or_insert(0);
                            *n += 1;
                            if *n >= config::uptime_fail_threshold() {
                                staged.push((p.id.to_string(), false, r.latency_ms as u32));
                                app.event(
                                    "error",
                                    format!(
                                        "{} is DOWN — HTTP {} on {} consecutive probes",
                                        p.label, r.status, n
                                    ),
                                );
                                *n = 0;
                            } else {
                                retry_soon = true;
                            }
                        }
                    }
                    upstream::Attempt::Err { message, .. } => {
                        let c = crate::classify::classify_transport(&message);
                        failed += 1;
                        if c.class == crate::classify::ErrClass::Offline {
                            // This provider could not be reached. That is a
                            // statement about the provider, not about the
                            // phone's link, so it is recorded as this provider
                            // being down and never flips the global flag. Only
                            // `spawn_net_watchdog`, which probes connectivity
                            // targets that owe us nothing, may claim we are
                            // offline — a dead domain here used to hold every
                            // request on a device with working internet.
                            offline_signal = true;
                            pending_fails.clear();
                            staged.clear();
                            break;
                        }
                        let n = pending_fails.entry(p.id.to_string()).or_insert(0);
                        *n += 1;
                        if *n >= config::uptime_fail_threshold() {
                            staged.push((p.id.to_string(), false, 0));
                            app.event(
                                "error",
                                format!(
                                    "{} is DOWN — {} ({} consecutive probes)",
                                    p.label, c.detail, n
                                ),
                            );
                            *n = 0;
                        } else {
                            retry_soon = true;
                        }
                    }
                }
                // Stagger providers so the radio isn't held open continuously.
                tokio::time::sleep(Duration::from_millis(400)).await;
            }

            // Corroboration gate. Every provider failing at once is far more
            // likely to be one dead uplink than three simultaneous outages, so
            // treat it as offline and record NOTHING — otherwise switching wifi
            // off paints red bars across every provider.
            // Every provider failing at once is strong evidence of a dead
            // uplink, but it is still provider-shaped evidence. Confirm it
            // against the link before saying so: this is where a single dead
            // provider stopped being a provider problem and became a total
            // outage for every request.
            let all_failed = probed > 0 && failed == probed;
            if (offline_signal || all_failed) && !device_online().await {
                if !app.is_offline() {
                    app.set_offline(true);
                }
                pending_fails.clear();
                staged.clear();
            } else {
                // Genuine per-provider result: commit the samples.
                for (id, up, ms) in staged.drain(..) {
                    app.push_uptime(&id, up, ms);
                }
                if failed == 0 && probed > 0 {
                    app.set_offline(false);
                }
            }

            let wait = if retry_soon {
                config::uptime_retry_secs()
            } else {
                config::uptime_probe_secs()
            };
            tokio::time::sleep(Duration::from_secs(wait)).await;
        }
    });
}

/// Keeps every key's balance fresh using only free billing probes.
///
/// Why this matters: knowing the balance *before* sending means we almost never
/// hit a quota 403 in the hot path. Requests go out on keys already measured to
/// have funds.
pub fn spawn_balance_sweeper(app: Arc<App>) {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(8)).await;
        loop {
            if app.is_offline() {
                tokio::time::sleep(Duration::from_secs(30)).await;
                continue;
            }
            let batch = collect_stale(&app);
            if !batch.is_empty() {
                eprintln!("[sweep] probing {} keys (free)", batch.len());
                sweep_batch(&app, batch).await;
                eprintln!("[sweep] done — {}", sweep_summary(&app));
            }
            tokio::time::sleep(Duration::from_secs(config::sweep_interval_secs())).await;
        }
    });
}

/// One `id N alive $X` clause per CONFIGURED provider.
///
/// These used to be two hardcoded `provider_summary` calls. `providers.json` is
/// user-editable, so that reported whatever two ids happened to be baked in at
/// compile time — on any other provider set both clauses read "0 alive $0.00",
/// which is a lie about the pools the sweeper had just refilled. Iterating the
/// live list cannot go stale.
fn sweep_summary(app: &App) -> String {
    app.providers
        .read()
        .unwrap()
        .iter()
        .map(|p| {
            let (alive, funds) = app.provider_summary(p.id);
            format!("{} {alive} alive ${funds:.2}", p.id)
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

/// Keys whose balance we do not know, or have not checked recently.
/// Unprobed keys come first so the pool becomes accurate fastest.
fn collect_stale(app: &Arc<App>) -> Vec<String> {
    let now = config::now_secs();
    let mut unprobed = Vec::new();
    let mut stale = Vec::new();
    {
        let g = app.lock();
        for p in &*app.providers.read().unwrap() {
            for key in app.pool(p.id) {
                let Some(k) = g.keys.get(&key) else { continue };
                if k.dead_until > now {
                    continue; // cooling down; no point measuring
                }
                match k.usage_cents {
                    None => unprobed.push(key),
                    Some(_) if now.saturating_sub(k.last_probe) > config::usage_stale_secs() => {
                        stale.push(key)
                    }
                    _ => {}
                }
            }
        }
    }
    unprobed.extend(stale);
    unprobed.truncate(config::sweep_batch());
    unprobed
}

/// Probe a batch with bounded concurrency and pacing.
///
/// Concurrency is low (4) and paced (120ms) on purpose: it keeps CPU and radio
/// duty-cycle down, and avoids looking like a scraper to Cloudflare.
async fn sweep_batch(app: &Arc<App>, keys: Vec<String>) {
    let keys = Arc::new(tokio::sync::Mutex::new(keys.into_iter()));
    let mut workers = Vec::new();

    for _ in 0..config::sweep_concurrency() {
        let app = app.clone();
        let keys = keys.clone();
        workers.push(tokio::spawn(async move {
            loop {
                let key = { keys.lock().await.next() };
                let Some(key) = key else { break };

                let host = {
                    let g = app.lock();
                    let Some(k) = g.keys.get(&key) else { continue };
                    app.provider(&k.provider).map(|p| p.host.to_string())
                };
                let Some(host) = host else { continue };

                match upstream::probe_usage(&host, &key).await {
                    upstream::Attempt::Ok(r) if r.status == 200 => {
                        if let Some(cents) = upstream::parse_usage_cents(&r.body) {
                            app.set_usage(&key, cents);
                        } else {
                            app.mark_probed(&key);
                        }
                    }
                    upstream::Attempt::Ok(r) if r.status == 401 => {
                        app.mark_dead(&key, config::cooldown_auth_secs(), "probe: invalid key");
                    }
                    upstream::Attempt::Ok(_) => app.mark_probed(&key),
                    upstream::Attempt::Err { message, .. } => {
                        if crate::classify::classify_transport(&message).class
                            == crate::classify::ErrClass::Offline
                        {
                            // Unreachable provider, not unreachable network.
                            // Keep sweeping; only the watchdog decides the flag.
                            app.mark_probed(&key);
                            break; // this provider is down; stop probing it
                        }
                        app.mark_probed(&key);
                    }
                }
                tokio::time::sleep(Duration::from_millis(config::sweep_pacing_ms())).await;
            }
        }));
    }
    for w in workers {
        let _ = w.await;
    }
}

/// Pulls new keys from the GitHub repo every 4 hours.
///
/// Runs `git pull` in the key repo, then re-reads the key files. Any key not
/// already known is registered and will be balance-probed by the sweeper. Keys
/// are never removed — a key missing upstream might still have funds.
pub fn spawn_key_sync(app: Arc<App>) {
    tokio::spawn(async move {
        // First run after a short delay; startup already loaded the files.
        tokio::time::sleep(Duration::from_secs(60)).await;
        loop {
            if !app.is_offline() {
                match git_pull().await {
                    Ok(changed) => {
                        let report = app.load_key_files();
                        let added: usize = report.iter().map(|(_, _, a)| *a).sum();
                        if added > 0 {
                            let detail = report
                                .iter()
                                .filter(|(_, _, a)| *a > 0)
                                .map(|(id, total, a)| format!("{id} +{a} (now {total})"))
                                .collect::<Vec<_>>()
                                .join(", ");
                            eprintln!("[keysync] {detail}");
                            app.event("info", format!("key sync: {detail}"));
                        } else if changed {
                            eprintln!("[keysync] repo updated, no new keys");
                        }
                    }
                    Err(e) => eprintln!("[keysync] skipped: {e}"),
                }
            }
            tokio::time::sleep(Duration::from_secs(config::keysync_interval_secs())).await;
        }
    });
}

async fn git_pull() -> Result<bool, String> {
    let repo = config::home().join("git_gorouter_keyforge-token");
    if !repo.join(".git").exists() {
        return Err("not a git repo".into());
    }
    let out = tokio::process::Command::new("git")
        .arg("-C")
        .arg(&repo)
        .arg("pull")
        .arg("--ff-only")
        .arg("--quiet")
        .output()
        .await
        .map_err(|e| format!("git failed to launch: {e}"))?;

    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(!out.stdout.is_empty())
}

/// Coalesced state snapshots. `save()` is a no-op unless something changed, so
/// an &idle gateway performs zero disk writes.
pub fn spawn_state_flusher(app: Arc<App>) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(config::state_flush_secs())).await;
            app.save();
        }
    });
}

/// Probe every provider until one answers. Returns `true` on the first success.
///
/// # Why this exists instead of probing `providers.first()`
/// Probing only the first provider — and only that provider's first key — made
/// the offline flag a property of ONE provider rather than of the network. If
/// that provider was Cloudflare-blocked, out of keys, or simply drained its
/// pool, the flag never cleared and every request was held forever even though
/// the device had working internet and healthy providers sitting right behind
/// it. Any provider answering is sufficient evidence the network is up; no
/// single provider is authoritative.
///
/// An empty provider list or an empty key pool counts as "cannot judge", which
/// callers treat as healthy. Refusing to go offline on absent evidence is the
/// safe direction: the cost of a wrong `false` is a failed request the client
/// can retry, while a wrong `true` silently holds traffic indefinitely.
pub async fn probe_any_provider(app: &App) -> bool {
    let providers = app.providers.read().unwrap().clone();
    if providers.is_empty() {
        return true;
    }
    let mut any_had_key = false;
    for p in providers {
        // Rotates through the pool so a provider whose FIRST key is dead but
        // whose other keys are funded still proves liveness.
        for k in app.pool(p.id).into_iter().take(3) {
            any_had_key = true;
            if let upstream::Attempt::Ok(_) = upstream::probe_models(p.host, &k).await {
                return true;
            }
        }
    }
    // Nothing to probe at all: no keys loaded. Not evidence of a dead network.
    !any_had_key
}

/// Whether the DEVICE has lost connectivity.
///
/// # The distinction this exists to enforce
/// "Offline" is a claim about the phone's link, not about any provider. A
/// provider with a dead DNS record, a retired domain, or a bad cert produces the
/// same `classify_transport` verdict as a phone in a tunnel — and inferring
/// "offline" from it holds every request, on a device whose internet is working
/// perfectly. That is the single most expensive bug in the gateway: it does not
/// fail loudly, it silently stops serving traffic.
///
/// So liveness is judged against the network, not against the providers. A probe
/// target is only allowed to declare the device offline when **every** target
/// independently fails to resolve or connect — one bad hostname is a bad
/// hostname, not a dead link.
///
/// Failures are counted per-host so a permanently broken provider cannot
/// outvote a working one: a host that has never once answered is retried on a
/// slower schedule, and is never counted toward "everything failed" until it has
/// actually been tried recently.
static NET_FAILS: std::sync::Mutex<Option<std::collections::HashMap<String, u32>>> =
    std::sync::Mutex::new(None);

const NET_PROBE_TARGETS: &[&str] = &[
    // Deliberately infrastructure, never a configured provider: these must not
    // be affected by a key being empty, a provider being disabled, or a domain
    // being retired. Any one of them answering proves the link works.
    "1.1.1.1", "8.8.8.8",
];

/// Record a transport failure against the device, not against a provider.
///
/// Returns `true` when enough *independent* hosts have failed to justify
/// declaring the link down. A caller reporting a provider-level failure passes
/// `None`, which never counts — a single unreachable provider is not evidence.
pub fn note_network_failure(host: Option<&str>) -> bool {
    let host = match host {
        Some(h) => h,
        // Provider-level failure. Not our network's fault by definition.
        None => return false,
    };
    let mut g = NET_FAILS.lock().unwrap_or_else(|e| e.into_inner());
    let m = g.get_or_insert_with(Default::default);
    let e = m.entry(host.to_string()).or_insert(0);
    *e = e.saturating_add(1);
    // Every target must fail NET_FAILS_BEFORE_OFFLINE times before we claim the
    // link is down. One blip on one host is not a disconnection, and a brief
    // radio stall during a wake-from-sleep is routine on a phone — treating that
    // as "offline" holds every request for as long as the stall lasts, which is
    // the exact failure this whole path exists to prevent.
    NET_PROBE_TARGETS
        .iter()
        .all(|t| m.get(*t).copied().unwrap_or(0) >= NET_FAILS_BEFORE_OFFLINE)
}

pub fn note_network_success() {
    let mut g = NET_FAILS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(m) = g.as_mut() {
        m.clear();
    }
}

/// Consecutive failed rounds against EVERY target before the link is called down.
///
/// Three rounds at the watchdog's 20s cadence is a full minute of the phone
/// being unable to open a single TCP connection — well past a radio stall, and
/// unambiguous evidence that the caller is not going to get an answer anyway.
const NET_FAILS_BEFORE_OFFLINE: u32 = 3;

/// Poll connectivity targets that owe us nothing: no key, no provider, no config.
///
/// Used by the watchdog to decide the offline flag from the link itself.
///
/// A TCP connect rather than a lookup: these targets are bare IPs precisely so
/// no resolver is involved, and `lookup_host` on an IP literal is not reliable
/// across platforms — it failed here while the device was plainly online. If we
/// cannot tell the link is down, we must not claim that it is.
pub async fn device_online() -> bool {
    use std::time::Duration as D;
    for host in NET_PROBE_TARGETS {
        // Two attempts per round: a connect that fails fast and one that is
        // given a little longer, so a single slow handshake cannot be mistaken
        // for a dead link.
        for attempt in 0..2 {
            let budget = D::from_secs(if attempt == 0 { 3 } else { 6 });
            let ok = tokio::time::timeout(budget, tokio::net::TcpStream::connect((*host, 443)))
                .await
                .map(|r| r.is_ok())
                .unwrap_or(false);
            if ok {
                note_network_success();
                return true;
            }
        }
        note_network_failure(Some(host));
    }
    false
}

/// While offline, checks for the network coming back so held requests resume
/// immediately — no fixed timer, no client-visible retry counter.
///
/// The flag describes the DEVICE's link, so the watchdog tests the link. It used
/// to ask the providers instead, which meant one provider with a retired domain
/// kept re-declaring the phone offline and the banner never went away — the
/// gateway was fully capable of serving traffic the entire time.
pub fn spawn_net_watchdog(app: Arc<App>) {
    tokio::spawn(async move {
        let mut fail_streak = 0u32;
        loop {
            if app.is_offline() {
                if device_online().await {
                    fail_streak = 0;
                    app.set_offline(false);
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
            } else {
                // Check liveness less often. A single failure does NOT flip us
                // offline — WiFi with no internet would otherwise hold every
                // request forever. Require 3 consecutive failures first.
                if device_online().await {
                    fail_streak = 0;
                } else {
                    fail_streak += 1;
                    if fail_streak >= 3 {
                        app.set_offline(true);
                        fail_streak = 0;
                    }
                }
                tokio::time::sleep(Duration::from_secs(20)).await;
            }
        }
    });
}

/// Probe every not-yet-measured key as fast as is polite.
///
/// Called at boot and whenever keys are added, so "no key left unprobed" holds
/// instead of waiting for the slow periodic sweep to reach them. Uses only the
/// FREE billing endpoint, so this costs nothing but time.
pub async fn probe_all_unprobed(app: Arc<App>, reason: &str) {
    let todo: Vec<String> = {
        let g = app.lock();
        let mut v = Vec::new();
        for p in &*app.providers.read().unwrap() {
            for key in app.pool(p.id) {
                if g.keys
                    .get(&key)
                    .map(|k| k.usage_cents.is_none())
                    .unwrap_or(false)
                {
                    v.push(key);
                }
            }
        }
        v
    };
    if todo.is_empty() {
        return;
    }
    app.event(
        "info",
        format!("probing {} unmeasured keys ({reason})", todo.len()),
    );
    eprintln!("[probe] {} unmeasured keys ({reason})", todo.len());

    // Higher concurrency than the periodic sweep: this is a one-off catch-up and
    // we want the pool accurate quickly. Still paced so Cloudflare stays calm.
    let total = todo.len();
    let iter = Arc::new(tokio::sync::Mutex::new(todo.into_iter()));
    let done = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut workers = Vec::new();

    for _ in 0..8 {
        let app = app.clone();
        let iter = iter.clone();
        let done = done.clone();
        workers.push(tokio::spawn(async move {
            loop {
                if app.is_offline() {
                    break;
                }
                let key = { iter.lock().await.next() };
                let Some(key) = key else { break };
                let host = {
                    let g = app.lock();
                    g.keys
                        .get(&key)
                        .and_then(|k| app.provider(&k.provider))
                        .map(|p| p.host)
                };
                let Some(host) = host else { continue };
                match upstream::probe_usage(host, &key).await {
                    upstream::Attempt::Ok(r) if r.status == 200 => {
                        if let Some(c) = upstream::parse_usage_cents(&r.body) {
                            app.set_usage(&key, c);
                        } else {
                            app.mark_probed(&key);
                        }
                    }
                    upstream::Attempt::Ok(r) if r.status == 401 => {
                        app.mark_dead(&key, config::cooldown_auth_secs(), "probe: invalid key");
                    }
                    upstream::Attempt::Ok(_) => app.mark_probed(&key),
                    upstream::Attempt::Err { message, .. } => {
                        if crate::classify::classify_transport(&message).class
                            == crate::classify::ErrClass::Offline
                        {
                            app.mark_probed(&key);
                            break;
                        }
                        app.mark_probed(&key);
                    }
                }
                let n = done.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                if n.is_multiple_of(200) {
                    eprintln!("[probe] {n}/{total}");
                }
                tokio::time::sleep(Duration::from_millis(70)).await;
            }
        }));
    }
    for w in workers {
        let _ = w.await;
    }
    let n = done.load(std::sync::atomic::Ordering::Relaxed);
    app.event("info", format!("probed {n} keys ({reason})"));
    eprintln!("[probe] finished {n}/{total} ({reason})");
    app.save();
}

/// Keep the proxy pool alive without supervision.
///
/// # Why this has to exist
/// Free proxies decay within hours. Of 100 candidates vetted from one
/// ProxyScrape download, 53 passed a real provider request; a day later far
/// fewer do. A static `proxies.txt` therefore degrades from "working" to "every
/// request falls back to direct" silently — and direct is exactly what gets a
/// Cloudflare HTML 403 from this device.
///
/// # What a cycle does
/// 1. Evict endpoints that failed `EVICT_AFTER_FAILS` times in a row.
/// 2. If healthy count is below the floor, vet fresh candidates and add the ones
///    that pass.
/// 3. Persist the pool, best-first, so a restart starts from the vetted set.
///
/// Vetting costs one FREE `/v1/models` request per candidate, with small
/// concurrency, so a cycle is cheap enough to run unattended.
pub fn spawn_proxy_maintainer(app: Arc<App>) {
    tokio::spawn(async move {
        // Let boot settle before the first cycle.
        tokio::time::sleep(Duration::from_secs(45)).await;
        loop {
            if !app.is_offline() {
                proxy_maint_cycle(&app).await;
            }
            tokio::time::sleep(Duration::from_secs(config::proxy_maint_secs())).await;
        }
    });
}

/// Pull a fresh public proxy list and normalise it into the ProxyScrape JSON
/// the pool already ingests, writing it to the primary candidates path.
///
/// The default source is proxifly/free-proxy-list's raw http list — a bare
/// array of `{"proxy":"https://ip:port","ip":...,"port":...,"https":true,...}`
/// records, which `parse_proxyscrape_json` does NOT understand on its own
/// (it expects `{"proxies":[{ip,port,protocol,ssl,alive,uptime,timeout}]}`).
/// So the normaliser maps each record into that shape: `protocol:"http"`,
/// `ssl` from the `https` flag (proxifly's http list is CONNECT-capable for
/// HTTPS origins — the ssl field is what the parser's gate reads), and the
/// `proxy` URL as the authoritative host:port when ip/port fields are
/// missing.
///
/// Returns the number of usable candidates in the downloaded file, or `Err`.
pub async fn download_candidates(app: &Arc<App>) -> Result<usize, String> {
    let url = config::proxy_download_url();
    let raw = upstream::direct_get(&url, Duration::from_secs(30)).await?;

    let src: serde_json::Value =
        serde_json::from_slice(&raw).map_err(|e| format!("bad JSON from {url}: {e}"))?;
    // Accept a bare array or a `{"proxies":[...]}` wrap.
    let arr = match src.get("proxies").and_then(|p| p.as_array()) {
        Some(a) => a,
        None => match src.as_array() {
            Some(a) => a,
            None => return Err("download was neither a proxy array nor wrapped".into()),
        },
    };

    let mut out: Vec<serde_json::Value> = Vec::new();
    for r in arr {
        let ip = r.get("ip").and_then(|x| x.as_str()).unwrap_or("");
        let port = r
            .get("port")
            .and_then(|x| x.as_u64())
            .or_else(|| {
                r.get("port")
                    .and_then(|x| x.as_str())
                    .and_then(|s| s.parse().ok())
            })
            .unwrap_or(0);
        // The `proxy` field ("https://host:port") wins when ip/port are missing.
        let mut host = ip.to_string();
        let mut port = port;
        if host.is_empty() || port == 0 {
            if let Some(p) = r.get("proxy").and_then(|x| x.as_str()) {
                let bare = p
                    .trim_start_matches("https://")
                    .trim_start_matches("http://")
                    .trim_start_matches("socks5://")
                    .trim_start_matches("socks4://");
                match bare.rsplit_once(':') {
                    Some((h, p2)) => {
                        host = h.to_string();
                        port = p2.parse().unwrap_or(0);
                    }
                    None => {
                        host = bare.to_string();
                        port = 0;
                    }
                }
            }
        }
        if host.is_empty() || !(1..=65535).contains(&port) {
            continue;
        }
        let https = r.get("https").and_then(|x| x.as_bool()).unwrap_or(false);
        out.push(serde_json::json!({
            "ip": host,
            "port": port,
            "protocol": "http",
            "ssl": https || r.get("protocol").and_then(|x| x.as_str()) == Some("https"),
            "alive": null,
        }));
    }
    if out.is_empty() {
        return Err("download contained no usable entries".into());
    }

    // Persist next to the other candidate paths so the cold-start seeder and
    // the refill path both find it.
    let dest = config::proxy_candidates_paths()
        .first()
        .cloned()
        .ok_or_else(|| "no candidate path configured".to_string())?;
    if let Some(dir) = dest.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
    }
    let tmp = dest.with_extension("json.tmp");
    let payload = serde_json::to_vec(&serde_json::json!({ "proxies": out }))
        .map_err(|e| format!("encode: {e}"))?;
    std::fs::write(&tmp, payload).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &dest).map_err(|e| e.to_string())?;

    app.event(
        "info",
        format!(
            "proxy: downloaded {} candidates from {url} -> {}",
            out.len(),
            dest.display()
        ),
    );
    Ok(out.len())
}

/// Refresh the candidate list before a maintainer cycle, so the refill step
/// always works from a current list rather than one that rotted overnight.
/// Silently skipped when offline or when the download fails (the existing
/// candidate file, if any, still feeds the refill).
async fn refresh_candidates(app: &Arc<App>) {
    if app.is_offline() {
        return;
    }
    match download_candidates(app).await {
        Ok(n) => {
            eprintln!("[keyforge] proxy: {n} fresh candidates downloaded");
        }
        Err(e) => {
            app.event("warn", format!("proxy: candidate download failed: {e}"));
        }
    }
}

/// Fast model list sync. Probes `/v1/models` for every provider every few
/// seconds and unions the result into each provider's declared `models` list,
/// so the dashboard (and routing) always knows what each provider actually
/// serves — no hand-editing, no hardcoding.
pub fn spawn_fast_model_sync(app: Arc<App>) {
    tokio::spawn(async move {
        let interval = std::env::var("KEYFORGE_MODEL_SYNC_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(5);
        loop {
            if !app.is_offline() {
                let providers = app.providers.read().unwrap().clone();
                for p in providers {
                    let pool = app.pool(p.id);
                    let key = {
                        let now = config::now_secs();
                        let g = app.lock();
                        pool.iter()
                            .find(|k| g.keys.get(*k).map(|s| s.dead_until <= now).unwrap_or(false))
                            .cloned()
                            .or_else(|| pool.first().cloned())
                    };
                    let Some(key) = key else { continue };
                    match upstream::probe_models(p.host, &key).await {
                        upstream::Attempt::Ok(r) if (200..300).contains(&r.status) => {
                            let ids = upstream::parse_model_ids(&r.body);
                            if !ids.is_empty() {
                                // Update state (for routing)
                                app.set_models_seen(p.id, ids.clone());
                                // Union into settings (memory)
                                let mut s = app.settings.lock().unwrap();
                                if let Some(cfg) = s.providers.iter_mut().find(|c| c.id == p.id) {
                                    for m in ids {
                                        if !cfg.models.contains(&m) {
                                            cfg.models.push(m);
                                        }
                                    }
                                }
                                drop(s);
                                app.touch();
                            }
                        }
                        _ => {}
                    }
                    tokio::time::sleep(Duration::from_millis(300)).await;
                }
            }
            tokio::time::sleep(Duration::from_secs(interval)).await;
        }
    });
}
pub fn spawn_settings_watcher(app: Arc<App>) {
    tokio::spawn(async move {
        let mut last_mtime = crate::settings::Settings::mtime();
        loop {
            tokio::time::sleep(Duration::from_secs(5)).await;
            let mtime = crate::settings::Settings::mtime();
            if mtime > last_mtime {
                last_mtime = mtime;
                let old_len = app.providers.read().unwrap().len();
                app.reload_providers();
                let new_len = app.providers.read().unwrap().len();
                app.event(
                    "info",
                    format!("settings reloaded — {old_len} → {new_len} providers"),
                );
            }
        }
    });
}

/// One maintenance pass. Public so the dashboard can trigger it on demand.
pub async fn proxy_maint_cycle(app: &Arc<App>) {
    let pool = upstream::proxy_pool();
    if !pool.is_enabled() {
        return;
    }

    // ── 1. shed the dead ────────────────────────────────────────────────────
    let evicted = pool.evict_dead();
    if !evicted.is_empty() {
        app.event(
            "info",
            format!(
                "proxy: dropped {} dead endpoint{} ({} healthy left)",
                evicted.len(),
                if evicted.len() == 1 { "" } else { "s" },
                pool.healthy_count()
            ),
        );
    }

    // ── 2. refill if thin ───────────────────────────────────────────────────
    // Refresh the public candidate list first, so the refill below works from
    // a current list rather than one that rotted overnight. The download is
    // direct (no proxy pool) and best-effort: it is skipped when offline and a
    // failure falls back to whatever candidate file is already on disk.
    refresh_candidates(app).await;
    if pool.needs_refill() {
        let added = refill_proxies(app).await;
        if added == 0 && pool.healthy_count() == 0 {
            // Say so once, loudly: every request is now going out from this
            // device's own IP, which is the condition that produces WAF blocks.
            app.event(
                "warn",
                "proxy: pool is empty and no candidates passed — egress is DIRECT".to_string(),
            );
        }
    }

    // ── 3. persist ──────────────────────────────────────────────────────────
    if let Err(e) = pool.save_file(&config::proxies_path()) {
        app.event("warn", format!("proxy: could not save pool ({e})"));
    }
}

/// Vet candidates from the JSON list and add whatever works.
///
/// Returns how many endpoints were added.
async fn refill_proxies(app: &Arc<App>) -> usize {
    let pool = upstream::proxy_pool();

    // Load candidates from the first path that exists.
    let mut candidates = Vec::new();
    let mut source = String::new();
    for path in config::proxy_candidates_paths() {
        if let Ok(bytes) = std::fs::read(&path) {
            candidates = crate::proxy::parse_proxyscrape_json(&bytes);
            if !candidates.is_empty() {
                source = path.display().to_string();
                break;
            }
        }
    }
    if candidates.is_empty() {
        app.event(
            "warn",
            "proxy: pool is thin but no candidate list was found — see docs for the ProxyScrape download".to_string(),
        );
        return 0;
    }

    // ── 1. ingest the WHOLE list, not a slice ──────────────────────────────
    //
    // A public list carries thousands of entries and the great majority are dead
    // on arrival. Ingesting only `vet_batch` of them per cycle caps the pool at
    // a few dozen IPs no matter how big the source is, which is not what "trust
    // the whole list" means. So every unseen candidate joins the pool as
    // UNVETTED: `pick()` will not route through it (the vetted gate holds), but
    // it is now a candidate for the verification pass below, and successive
    // cycles verify more of it without ever re-downloading.
    let known: std::collections::HashSet<String> =
        pool.snapshot().into_iter().map(|(e, _)| e.addr()).collect();
    let fresh: Vec<crate::proxy::ProxyEndpoint> = candidates
        .into_iter()
        .filter(|e| !known.contains(&e.addr()))
        .collect();
    if fresh.is_empty() {
        return 0;
    }
    let ingested = pool.add_endpoints(fresh);

    // ── 2. verify a bounded slice of the un-vetted backlog ──────────────────
    //
    // Verification costs a real request each (up to `zen_ping_timeout_secs`), so
    // only `vet_batch` are checked per cycle — but they are drawn from the whole
    // pool now, not just the head of the download, so a cycle always makes
    // progress on proving more of the backlog rather than re-reading a prefix.
    let batch: Vec<crate::proxy::ProxyEndpoint> = pool
        .unvetted(config::proxy_vet_batch())
        .into_iter()
        .map(|(e, _)| e)
        .collect();
    if batch.is_empty() {
        return ingested;
    }

    // Vet method: a real zen "pong" (default when a zen provider is configured)
    // or the cheap `/v1/models` probe. Only a PONG accepts a proxy — a free IP
    // that tunnels but cannot carry a model call would just park requests on a
    // burned address, which is exactly what the pool exists to avoid.
    let method = config::proxy_vet_method();
    let models_target = if method == "ping" {
        None
    } else {
        // A provider + key to vet against. Any funded key works; this is a free
        // endpoint, so it costs nothing.
        vet_target(app)
    };
    if method != "ping" && models_target.is_none() {
        app.event(
            "warn",
            "proxy: no key available to vet proxies with".to_string(),
        );
        return 0;
    }

    app.event(
        "info",
        format!(
            "proxy: vetting {} candidates from {} ({method})",
            batch.len(),
            source
        ),
    );

    // Bounded concurrency: a burst of parallel connections from one device to one
    // provider is the pattern proxies exist to avoid.
    let mut good: Vec<crate::proxy::ProxyEndpoint> = Vec::new();
    let mut checked = 0usize;
    for chunk in batch.chunks(config::proxy_vet_concurrency()) {
        let mut set = Vec::new();
        for ep in chunk {
            let ep = ep.clone();
            let target = models_target.clone();
            set.push(tokio::spawn(async move {
                if let Some((host, key)) = target {
                    let r = upstream::vet_proxy(&ep, &host, &key).await;
                    (ep, r)
                } else {
                    let r = upstream::vet_zen_ping(&ep).await;
                    (ep, r)
                }
            }));
        }
        for h in set {
            checked += 1;
            if let Ok((ep, Ok(ms))) = h.await {
                // Record the vetted latency, which was previously discarded. It is
                // the only measurement of this proxy we will ever have before it
                // serves its first real request, and `pick()` orders on it.
                pool.note_vetted(&ep.addr(), ms);
                good.push(ep);
            }
        }
        // Stop as soon as the pool is comfortable again rather than vetting the
        // whole batch for its own sake.
        if pool.healthy_count() + good.len() >= config::proxy_vet_batch().min(20) {
            break;
        }
    }

    let added = pool.add_endpoints(good);
    app.event(
        if added > 0 { "info" } else { "warn" },
        format!(
            "proxy: {added} of {checked} candidates passed — {} healthy",
            pool.healthy_count()
        ),
    );
    added
}

/// Pick a (&host, key) pair to vet against: the first provider with a usable key.
fn vet_target(app: &Arc<App>) -> Option<(String, String)> {
    for p in app.providers.read().unwrap().iter() {
        let pool = app.pool(p.id);
        let g = app.lock();
        for k in pool.iter() {
            if let Some(st) = g.keys.get(k) {
                if st.usable(config::now_secs(), p.hold) {
                    return Some((p.host.to_string(), k.clone()));
                }
            }
        }
    }
    None
}

pub fn spawn_all(app: Arc<App>) {
    // Catch-up probe first so the pool is accurate within minutes of boot,
    // rather than after many slow sweep cycles.
    {
        let a = app.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(2)).await;
            probe_all_unprobed(a, "startup").await;
        });
    }
    spawn_uptime_prober(app.clone());
    spawn_balance_sweeper(app.clone());
    spawn_key_sync(app.clone());
    spawn_state_flusher(app.clone());
    spawn_net_watchdog(app.clone());
    spawn_settings_watcher(app.clone());
    spawn_fast_model_sync(app.clone());
    // Local Wi-Fi quality. Runs on its own OS thread, not a tokio task: it
    // blocks on process spawns for seconds at a time and would otherwise hold a
    // runtime worker &hostage.
    crate::link::spawn(app.clone());
    spawn_proxy_maintainer(app.clone());
    crate::modelsync::spawn(app);
}

/// Run one balance sweep immediately (used by the dashboard refresh button).
pub async fn sweep_now(app: Arc<App>) {
    let batch = collect_stale(&app);
    if batch.is_empty() {
        app.event("info", "refresh: all balances already current");
        return;
    }
    app.event(
        "info",
        format!("refresh: probing {} keys (free)", batch.len()),
    );
    sweep_batch(&app, batch).await;
    app.event("info", format!("refresh done — {}", sweep_summary(&app)));
}
