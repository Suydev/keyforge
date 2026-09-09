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
                            // Unambiguously OUR network (DNS/route). Record
                            // nothing and abandon the round.
                            offline_signal = true;
                            app.set_offline(true);
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
            let all_failed = probed > 0 && failed == probed;
            if offline_signal || all_failed {
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
                let (t_alive, t_funds) = app.provider_summary("tabi");
                let (g_alive, g_funds) = app.provider_summary("gorouter");
                eprintln!(
                    "[sweep] done — tabi {t_alive} alive ${t_funds:.2} | gorouter {g_alive} alive ${g_funds:.2}"
                );
            }
            tokio::time::sleep(Duration::from_secs(config::sweep_interval_secs())).await;
        }
    });
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
                            app.set_offline(true);
                            break; // stop the sweep; no network
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

/// While offline, checks for the network coming back so held requests resume
/// immediately — no fixed timer, no client-visible retry counter.
pub fn spawn_net_watchdog(app: Arc<App>) {
    tokio::spawn(async move {
        let mut fail_streak = 0u32;
        loop {
            if app.is_offline() {
                // Try to come back online: probe any provider's /v1/models.
                let prov = app.providers.read().unwrap().first().cloned();
                if let Some(p) = prov {
                    if let Some(k) = app.pool(p.id).first() {
                        if let upstream::Attempt::Ok(_) = upstream::probe_models(&p.host, k).await {
                            fail_streak = 0;
                            app.set_offline(false);
                        }
                    }
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
            } else {
                // Check liveness less often. A single failure does NOT flip us
                // offline — WiFi with no internet would otherwise hold every
                // request forever. Require 3 consecutive failures first.
                let prov = app.providers.read().unwrap().first().cloned();
                let ok = if let Some(p) = prov {
                    if let Some(k) = app.pool(p.id).first() {
                        matches!(upstream::probe_models(&p.host, k).await, upstream::Attempt::Ok(_))
                    } else {
                        true
                    }
                } else {
                    true
                };
                if ok {
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
                            app.set_offline(true);
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
                            .find(|k| {
                                g.keys.get(*k).map(|s| s.dead_until <= now).unwrap_or(false)
                            })
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
                                if let Some(cfg) =
                                    s.providers.iter_mut().find(|c| c.id == p.id)
                                {
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

    // Skip anything already in the pool, then take a bounded batch. Candidates
    // arrive sorted best-first, so the batch is the most promising slice.
    let known: std::collections::HashSet<String> = pool
        .snapshot()
        .into_iter()
        .map(|(e, _)| e.addr())
        .collect();
    let batch: Vec<crate::proxy::ProxyEndpoint> = candidates
        .into_iter()
        .filter(|e| !known.contains(&e.addr()))
        .take(config::proxy_vet_batch())
        .collect();
    if batch.is_empty() {
        return 0;
    }

    // A provider + key to vet against. Any funded key works; this is a free
    // endpoint, so it costs nothing.
    let Some((host, key)) = vet_target(app) else {
        app.event("warn", "proxy: no key available to vet proxies with".to_string());
        return 0;
    };

    app.event(
        "info",
        format!(
            "proxy: vetting {} candidates from {}",
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
            let host = host.clone();
            let key = key.clone();
            set.push(tokio::spawn(async move {
                let r = upstream::vet_proxy(&ep, &host, &key).await;
                (ep, r)
            }));
        }
        for h in set {
            checked += 1;
            if let Ok((ep, Ok(_ms))) = h.await {
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
    let (t_alive, t_funds) = app.provider_summary("tabi");
    let (g_alive, g_funds) = app.provider_summary("gorouter");
    app.event(
        "info",
        format!("refresh done — tabi {t_alive} alive ${t_funds:.2} | gorouter {g_alive} alive ${g_funds:.2}"),
    );
}
