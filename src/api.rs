//! Dashboard JSON API + Server-Sent Events.
//!
//! Battery discipline (target <=5%/hour on a Pad Go):
//!   * SSE **push on change**, never poll. The loop watches a revision counter
//!     and only serialises when state actually moved.
//!   * A heartbeat every 20s keeps the connection alive without sending data.
//!   * The browser closes the stream when the tab is hidden (see the JS), so a
//!     backgrounded dashboard costs literally nothing.
//!
//! Keys are ALWAYS masked in every payload. The dashboard shows balances and
//! spend, never full secrets.

use crate::config;
use crate::router::{full, Body};
use crate::state::App;
use crate::upstream;
use hyper::header::{HeaderName, HeaderValue};
use hyper::{Response, StatusCode};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

pub fn mask(k: &str) -> String {
    // Char-based: `POST /api/keys/verify` accepts arbitrary JSON strings, so a
    // multi-byte key would panic here — fatal under `panic = "abort"`.
    let n = k.chars().count();
    if n < 14 {
        return "sk-…".into();
    }
    let head: String = k.chars().take(10).collect();
    let tail: String = k.chars().skip(n - 4).collect();
    format!("{head}…{tail}")
}

fn json_response(status: StatusCode, v: Value) -> Response<Body> {
    let bytes = serde_json::to_vec(&v).unwrap_or_else(|_| b"{}".to_vec());
    let mut r = Response::new(full(bytes));
    *r.status_mut() = status;
    r.headers_mut().insert(
        HeaderName::from_static("content-type"),
        HeaderValue::from_static("application/json"),
    );
    // Never cache live data.
    r.headers_mut().insert(
        HeaderName::from_static("cache-control"),
        HeaderValue::from_static("no-store"),
    );
    r
}

pub fn ok(v: Value) -> Response<Body> {
    json_response(StatusCode::OK, v)
}

pub fn bad(msg: &str) -> Response<Body> {
    json_response(
        StatusCode::BAD_REQUEST,
        json!({ "ok": false, "error": msg }),
    )
}

/// Everything the dashboard needs, in one snapshot.
///
/// Deliberately one payload rather than several endpoints: a single SSE frame
/// keeps every number on screen mutually consistent, and one wake-up is cheaper
/// than five.
pub fn snapshot(app: &Arc<App>) -> Value {
    let now = config::now_secs();
    let g = app.lock();

    // ── providers ────────────────────────────────────────────────────────────
    let mut providers = Vec::new();
    for p in &*app.providers.read().unwrap() {
        let pool = app.pool(p.id);
        let mut alive = 0usize;
        let mut cooling = 0usize;
        let mut probed = 0usize;
        let mut funds = 0.0f64;
        let mut spent = 0.0f64;
        for k in &pool {
            let Some(s) = g.keys.get(k) else { continue };
            if s.usage_cents.is_some() {
                probed += 1;
            }
            if s.dead_until > now {
                cooling += 1;
            } else if s.usable(now, p.hold) {
                alive += 1;
            }
            funds += s.balance.unwrap_or(0.0);
            spent += s.spent;
        }
        let rt = g.providers.get(p.id);
        // Only the tail the chart actually draws. The full 720-sample series is
        // still available from `/api/uptime`. Sending all of it here cost 88KB of
        // a 161KB snapshot — re-serialised and re-parsed on every state change,
        // which is felt as UI lag rather than as bandwidth.
        let uptime: Vec<Value> = rt
            .map(|r| {
                let skip = r
                    .uptime
                    .len()
                    .saturating_sub(config::SNAPSHOT_UPTIME_SAMPLES);
                r.uptime
                    .iter()
                    .skip(skip)
                    .map(|s| json!({ "t": s.t, "up": s.up, "ms": s.ms }))
                    .collect()
            })
            .unwrap_or_default();
        // Percentage is computed over the FULL series, not the truncated tail:
        // "99.4% up" must mean the whole retained window.
        let uptime_total = rt.map(|r| r.uptime.len()).unwrap_or(0);
        let up_pct = rt
            .map(|r| {
                if r.uptime.is_empty() {
                    100.0
                } else {
                    let up = r.uptime.iter().filter(|s| s.up).count() as f64;
                    up / r.uptime.len() as f64 * 100.0
                }
            })
            .unwrap_or(100.0);

        providers.push(json!({
            "id": p.id,
            "label": p.label,
            "host": p.host,
            "hold": p.hold,
            "keys": pool.len(),
            "alive": alive,
            "cooling": cooling,
            "probed": probed,
            "funds": (funds * 100.0).round() / 100.0,
            "spent": (spent * 1e6).round() / 1e6,
            "requests": rt.map(|r| r.requests).unwrap_or(0),
            "errors": rt.map(|r| r.errors).unwrap_or(0),
            "bytesUp": rt.map(|r| r.bytes_up).unwrap_or(0),
            "bytesDown": rt.map(|r| r.bytes_down).unwrap_or(0),
            "cost": rt.map(|r| (r.cost * 1e6).round() / 1e6).unwrap_or(0.0),
            "ewmaMs": rt.and_then(|r| r.ewma_ms).map(|v| v.round()),
            "ttfbMs": rt.and_then(|r| r.ewma_ttfb_ms).map(|v| v.round()),
            "clientRequests": rt.map(|r| r.client_requests).unwrap_or(0),
            "clientFailures": rt.map(|r| r.client_failures).unwrap_or(0),
            "keyRotations": rt.map(|r| r.key_rotations).unwrap_or(0),
            "consecutiveFails": rt.map(|r| r.consecutive_fails).unwrap_or(0),
            "uptimePct": (up_pct * 10.0).round() / 10.0,
            "uptime": uptime,
            "uptimeSamples": uptime_total,
            "models": rt.map(|r| r.models_seen.clone()).unwrap_or_default(),
        }));
    }

    // ── combined model list across providers ─────────────────────────────────
    let mut models: Vec<Value> = g
        .models
        .iter()
        .map(|(id, m)| {
            json!({
                "id": &id,
                "requests": m.requests,
                "errors": m.errors,
                "cost": (m.cost * 1e6).round() / 1e6,
                "inTokens": m.in_tokens,
                "outTokens": m.out_tokens,
                "avgMs": m.latency_sum_ms.checked_div(m.latency_n).unwrap_or(0) as i64,
                "byProvider": m.by_provider,
            })
        })
        .collect();
    models.sort_by(|a, b| {
        b["requests"]
            .as_u64()
            .unwrap_or(0)
            .cmp(&a["requests"].as_u64().unwrap_or(0))
    });

    // Models advertised but not yet used, so the list is complete rather than
    // only showing what happens to have traffic.
    let used: std::collections::HashSet<String> = g.models.keys().cloned().collect();
    let mut advertised: Vec<Value> = Vec::new();
    for p in &*app.providers.read().unwrap() {
        if let Some(rt) = g.providers.get(p.id) {
            for m in &rt.models_seen {
                if !used.contains(m) {
                    advertised.push(json!({ "id": m, "provider": p.id }));
                }
            }
        }
    }

    // ── sessions (live, with cost attribution) ───────────────────────────────
    // Computed first: the session rows below need it to distinguish "working
    // right now" from "active in the last 45 minutes".
    let working = app.working_sessions();
    let mut sessions: Vec<Value> = g
        .sessions
        .iter()
        .map(|(fp, s)| {
            json!({
                "fp": fp,
                "label": s.label,
                "provider": s.provider,
                "key": mask(&s.key),
                "turns": s.turns,
                "cost": (s.cost * 1e6).round() / 1e6,
                "inTokens": s.in_tokens,
                "outTokens": s.out_tokens,
                "bytesUp": s.bytes_up,
                "bytesDown": s.bytes_down,
                "created": s.created,
                "lastSeen": s.last_seen,
                "active": now.saturating_sub(s.last_seen) < config::SESSION_ACTIVE_SECS,
                // "working" means a request is in flight RIGHT NOW, which is
                // different from "active" (seen within the session window).
                "working": working.contains(fp),
                "models": s.models,
                // Which tool this came from (Claude Code / OpenCode / curl / …)
                // and which API surface it used. Empty until first &identified.
                "client": if s.client.is_empty() { "unknown" } else { &s.client },
                "clientLabel": if s.client_label.is_empty() { "unknown" } else { &s.client_label },
                "api": s.api,
                // How the session was &identified, and whether the client has
                // compacted its history (which would otherwise fork the session).
                "via": s.via,
                "compactions": s.compactions,
                "keySwitches": s.key_switches,
                "errors": s.errors,
                "maxLatencyMs": s.max_latency_ms,
            })
        })
        .collect();
    sessions.sort_by(|a, b| {
        b["lastSeen"]
            .as_u64()
            .unwrap_or(0)
            .cmp(&a["lastSeen"].as_u64().unwrap_or(0))
    });
    let active = sessions
        .iter()
        .filter(|s| s["active"] == json!(true))
        .count();
    // Count first, THEN truncate: "3 active" must count every session, even the
    // ones trimmed out of this payload. The Sessions tab pages via /api/session.
    let sessions_total = sessions.len();
    sessions.truncate(config::SNAPSHOT_SESSIONS);

    // ── per-minute series for the charts ─────────────────────────────────────
    let minutes: Vec<Value> = g
        .minutes
        .iter()
        .skip(g.minutes.len().saturating_sub(config::SNAPSHOT_MINUTES))
        .map(|b| {
            json!({
                "t": b.t * 60,
                "requests": b.requests,
                "errors": b.errors,
                "bytesUp": b.bytes_up,
                "bytesDown": b.bytes_down,
                "cost": (b.cost * 1e6).round() / 1e6,
                "avgMs": b.latency_sum_ms.checked_div(b.latency_n).unwrap_or(0) as i64,
            })
        })
        .collect();

    // ── live in-flight requests ──────────────────────────────────────────────
    let inflight: Vec<Value> = app
        .inflight_list()
        .into_iter()
        .map(|f| {
            json!({
                "id": f.id,
                "session": f.session,
                "label": f.label,
                "provider": f.provider,
                "host": f.host,
                "key": f.key,
                "model": f.model,
                "client": f.client,
                "started": f.started,
                "elapsedSecs": now.saturating_sub(f.started),
                "round": f.round,
                "attempts": f.attempts,
                "streaming": f.streaming,
                "bytesUp": f.bytes_up,
                "phase": f.phase,
            })
        })
        .collect();

    // ── errors, full detail, for the Errors tab ──────────────────────────────
    //
    // The training features (reqBytes/budgetSecs/ttfbMs/streaming/proxy) are
    // serialised too, not just stored on disk. They were the ONE correlation
    // that explained the 1M-context outage — request size against the budget in
    // force — and because the API dropped them, the dashboard could not show it
    // and the diagnosis had to be done by reading the raw state file.
    let errors_log: Vec<Value> = g
        .errors_log
        .iter()
        .rev()
        .take(config::SNAPSHOT_ERRORS)
        .map(|e| {
            json!({
                "t": e.t,
                "class": e.class,
                "provider": e.provider,
                "host": e.host,
                "key": e.key,
                "model": e.model,
                "status": e.status,
                "message": e.message,
                "action": e.action,
                "session": e.session,
                "round": e.round,
                "latencyMs": e.latency_ms,
                "remaining": e.remaining,
                "required": e.required,
                "reqBytes": e.req_bytes,
                "budgetSecs": e.budget_secs,
                "ttfbMs": e.ttfb_ms,
                "streaming": e.streaming,
                "headTimeout": e.head_timeout,
                "proxy": e.proxy,
            })
        })
        .collect();

    // Error counts by class, so the Errors tab can lead with a breakdown rather
    // than only a flat list.
    let mut by_class: std::collections::BTreeMap<String, u64> = Default::default();
    for e in g.errors_log.iter() {
        *by_class.entry(e.class.clone()).or_insert(0) += 1;
    }

    let events: Vec<Value> = g
        .events
        .iter()
        .rev()
        .take(40)
        .map(|e| json!({ "t": e.t, "level": e.level, "msg": e.msg }))
        .collect();

    let t = &g.totals;
    json!({
        "now": now,
        "rev": app.rev(),
        "offline": app.is_offline(),
        "uptimeSecs": now.saturating_sub(t.started_at),
        "totals": {
            // Attempt-level (internal).
            "requests": t.requests,
            "errors": t.errors,
            // Client-level (what the user experienced).
            "clientRequests": t.client_requests,
            "clientFailures": t.client_failures,
            "clientSaved": t.client_saved,
            "bytesUp": t.bytes_up,
            "bytesDown": t.bytes_down,
            "bytesTotal": t.bytes_up + t.bytes_down,
            "cost": (t.cost * 1e6).round() / 1e6,
            "rotations": t.rotations,
            "failovers": t.failovers,
            "offlineHolds": t.offline_holds,
        },
        "providers": providers,
        "models": models,
        "advertised": advertised,
        "sessions": sessions,
        "activeSessions": active,
        "workingSessions": working.len(),
        "inflight": inflight,
        "errorsLog": errors_log,
        "errorsByClass": by_class,
        "minutes": minutes,
        "sessionsTotal": sessions_total,
        "events": events,
    })
}

/// Per-key detail for the Keys page. Masked, paginated, sorted by balance.
pub fn keys_page(app: &Arc<App>, provider: &str, limit: usize) -> Value {
    let now = config::now_secs();
    let hold = app.provider(provider).map(|p| p.hold).unwrap_or(0.8);
    let pool = app.pool(provider);
    let g = app.lock();

    let mut rows: Vec<Value> = pool
        .iter()
        .filter_map(|k| {
            let s = g.keys.get(k)?;
            Some(json!({
                "key": mask(k),
                "balance": s.balance.map(|b| (b * 100.0).round() / 100.0),
                "usageCents": s.usage_cents,
                "exact": s.initial_exact,
                "turns": s.turns,
                "spent": (s.spent * 1e6).round() / 1e6,
                "cooling": s.dead_until > now,
                "coolingFor": s.dead_until.saturating_sub(now),
                "reason": s.reason,
                "usable": s.usable(now, hold),
                "lastUsed": s.last_used,
            }))
        })
        .collect();

    rows.sort_by(|a, b| {
        let x = a["balance"].as_f64().unwrap_or(-1.0);
        let y = b["balance"].as_f64().unwrap_or(-1.0);
        y.partial_cmp(&x).unwrap_or(std::cmp::Ordering::Equal)
    });
    let total = rows.len();
    rows.truncate(limit);

    json!({ "provider": provider, "total": total, "rows": rows })
}

/// Verify a pasted key against every provider and adopt it if it has funds.
///
/// Uses only FREE endpoints:
///   1. `/v1/models`                     — which provider owns this key?
///   2. `/v1/dashboard/billing/usage`    — how much has it spent?
///
/// The balance test is relative to that provider's own hold, so a key is only
/// adopted if it can actually serve at least a few requests.
pub async fn verify_and_add(app: &Arc<App>, key: &str) -> Value {
    let key = key.trim();
    if !key.starts_with("sk-") || key.len() < 20 {
        return json!({ "ok": false, "error": "not a valid sk- key" });
    }
    if app.lock().keys.contains_key(key) {
        return json!({ "ok": false, "error": "key already in the pool", "duplicate": true });
    }

    let mut tried = Vec::new();

    let providers = app.providers.read().unwrap().clone();
    for p in providers {
        // Step 1: does this key authenticate here?
        let models = match upstream::probe_models(p.host, key).await {
            upstream::Attempt::Ok(r) if (200..300).contains(&r.status) => {
                upstream::parse_model_ids(&r.body)
            }
            upstream::Attempt::Ok(r) => {
                tried.push(json!({ "provider": p.id, "result": format!("HTTP {}", r.status) }));
                continue;
            }
            upstream::Attempt::Err { message, .. } => {
                tried.push(json!({ "provider": p.id, "result": message.chars().take(80).collect::<String>() }));
                continue;
            }
        };

        // Step 2: measure spend, derive balance.
        let (usage_cents, balance) = match upstream::probe_usage(p.host, key).await {
            upstream::Attempt::Ok(r) if r.status == 200 => {
                match upstream::parse_usage_cents(&r.body) {
                    Some(c) => (Some(c), Some((p.initial_guess - c / 100.0).max(0.0))),
                    None => (None, None),
                }
            }
            _ => (None, None),
        };

        // Reasonable = can serve at least ~3 requests at this provider's hold.
        let threshold = p.hold * 3.0;
        let reasonable = balance.map(|b| b >= threshold).unwrap_or(true);

        if !reasonable {
            tried.push(json!({
                "provider": p.id,
                "result": format!("balance ${:.2} below ${:.2} minimum", balance.unwrap_or(0.0), threshold)
            }));
            return json!({
                "ok": false,
                "provider": p.id,
                "balance": balance,
                "error": format!(
                    "key belongs to {} but only has ${:.2} left (needs ${:.2}+). Not added.",
                    p.label, balance.unwrap_or(0.0), threshold
                ),
                "tried": tried,
            });
        }

        let added = app.append_key(p.id, key).unwrap_or(false);
        if added {
            if let Some(c) = usage_cents {
                app.set_usage(key, c);
            }
            app.event(
                "info",
                format!(
                    "key added to {} ({}) balance ~${:.2}",
                    p.label,
                    mask(key),
                    balance.unwrap_or(0.0)
                ),
            );
        }
        return json!({
            "ok": added,
            "provider": p.id,
            "providerLabel": p.label,
            "balance": balance.map(|b| (b * 100.0).round() / 100.0),
            "usageCents": usage_cents,
            "models": models,
            "message": if added {
                format!("added to {} — balance ~${:.2}", p.label, balance.unwrap_or(0.0))
            } else {
                "already present".to_string()
            },
        });
    }

    json!({
        "ok": false,
        "error": "key was not accepted by any configured provider",
        "tried": tried,
    })
}

/// Provider leaderboard.
///
/// Ranks providers on the same signals routing uses, so the dashboard explains
/// the gateway's actual behaviour rather than a parallel opinion. Score is
/// lower-is-better; the table shows each component so a surprising rank can be
/// understood at a glance.
pub fn leaderboard(app: &Arc<App>) -> Value {
    let now = config::now_secs();
    let (halflife, err_w, streak_w) = {
        let s = app.settings.lock().unwrap_or_else(|e| e.into_inner());
        (
            s.routing.streak_halflife_secs,
            s.routing.error_weight,
            s.routing.streak_weight,
        )
    };

    let mut rows: Vec<Value> = Vec::new();
    for p in &*app.providers.read().unwrap() {
        let (alive, funds) = app.provider_summary(p.id);
        let streak = app.effective_streak(p.id, halflife);
        let err = app.effective_error_rate(p.id, halflife);
        let g = app.lock();
        let rt = g.providers.get(p.id);
        // TTFB drives routing; full duration is shown separately because it
        // includes model thinking time and is not a provider quality signal.
        let ttfb_ms = rt.and_then(|r| r.ewma_ttfb_ms).unwrap_or(0.0);
        let total_ms = rt.and_then(|r| r.ewma_ms).unwrap_or(0.0);
        let samples = rt.map(|r| r.ttfb_samples).unwrap_or(0);
        let reqs = rt.map(|r| r.requests).unwrap_or(0);
        let errs = rt.map(|r| r.errors).unwrap_or(0);
        // CLIENT-level figures: what a user actually experienced. Attempt-level
        // counts made the success rate read 0% because one request that burns six
        // keys logs six requests AND six errors.
        let creqs = rt.map(|r| r.client_requests).unwrap_or(0);
        let cfails = rt.map(|r| r.client_failures).unwrap_or(0);
        let rotations = rt.map(|r| r.key_rotations).unwrap_or(0);
        let failovers = rt.map(|r| r.failovers_out).unwrap_or(0);
        let rescues = rt.map(|r| r.rescues).unwrap_or(0);
        let cost = rt.map(|r| r.cost).unwrap_or(0.0);
        let uptime_pct = rt
            .map(|r| {
                if r.uptime.is_empty() {
                    100.0
                } else {
                    r.uptime.iter().filter(|s| s.up).count() as f64 / r.uptime.len() as f64 * 100.0
                }
            })
            .unwrap_or(100.0);
        let breaker = rt.map(|r| r.open_until > now).unwrap_or(false);
        drop(g);

        let lat_pen = ttfb_ms / 1000.0;
        let err_pen = err * err_w;
        let streak_pen = streak * streak * streak_w;
        let dry_pen = if alive == 0 { 1000.0 } else { 0.0 };
        let breaker_pen = if breaker { 500.0 } else { 0.0 };
        let score = lat_pen + err_pen + streak_pen + dry_pen + breaker_pen;

        // Cost per successful request, the number that actually matters when
        // comparing a flat-hold provider against a per-token one.
        // Cost per CLIENT request — the number that matters when comparing a
        // flat-hold provider against a per-token one.
        let cost_per_req = if creqs > 0 { cost / creqs as f64 } else { 0.0 };

        rows.push(json!({
            "id": p.id,
            "label": p.label,
            "host": p.host,
            "score": (score * 1000.0).round() / 1000.0,
            "components": {
                "latency": (lat_pen * 1000.0).round() / 1000.0,
                "errorRate": (err_pen * 1000.0).round() / 1000.0,
                "streak": (streak_pen * 1000.0).round() / 1000.0,
                "dry": dry_pen,
                "breaker": breaker_pen,
            },
            "ttfbMs": ttfb_ms.round(),
            "latencyMs": total_ms.round(),
            "ttfbSamples": samples,
            "clientRequests": creqs,
            "clientFailures": cfails,
            "keyRotations": rotations,
            "failoversOut": failovers,
            "rescues": rescues,
            "rawErrorRate": (err * 1000.0).round() / 1000.0,
            "rawStreak": (streak * 100.0).round() / 100.0,
            "uptimePct": (uptime_pct * 10.0).round() / 10.0,
            "aliveKeys": alive,
            "funds": (funds * 100.0).round() / 100.0,
            "requests": reqs,
            "errors": errs,
            // Client-level success: of the requests users actually made, how many
            // got an answer. Attempt-level rates are reported separately.
            "successRate": if creqs + cfails > 0 {
                (creqs as f64 / (creqs + cfails) as f64 * 1000.0).round() / 10.0
            } else { 100.0 },
            "attemptSuccessRate": if reqs > 0 {
                (reqs.saturating_sub(errs) as f64 / reqs as f64 * 1000.0).round() / 10.0
            } else { 100.0 },
            "cost": (cost * 1e6).round() / 1e6,
            "costPerRequest": (cost_per_req * 1e6).round() / 1e6,
            "breakerOpen": breaker,
            "hold": p.hold,
        }));
    }

    rows.sort_by(|a, b| {
        a["score"]
            .as_f64()
            .unwrap_or(0.0)
            .partial_cmp(&b["score"].as_f64().unwrap_or(0.0))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    for (i, r) in rows.iter_mut().enumerate() {
        r["rank"] = json!(i + 1);
    }

    json!({ "rows": rows, "weights": { "errorWeight": err_w, "streakWeight": streak_w, "streakHalflifeSecs": halflife } })
}

/// Full detail for one session, for the session drill-down view.
pub fn session_detail(app: &Arc<App>, fp: &str) -> Value {
    let now = config::now_secs();
    let working = app.working_sessions();
    let g = app.lock();
    let Some(s) = g.sessions.get(fp) else {
        return json!({ "error": "no such session" });
    };

    // Errors that happened inside this session.
    let errs: Vec<Value> = g
        .errors_log
        .iter()
        .rev()
        .filter(|e| e.session == fp)
        .take(50)
        .map(|e| {
            json!({
                "t": e.t, "class": e.class, "provider": e.provider, "host": e.host,
                "key": e.key, "status": e.status, "message": e.message,
                "action": e.action, "round": e.round, "latencyMs": e.latency_ms,
            })
        })
        .collect();

    let dur = s.last_seen.saturating_sub(s.created).max(1);
    json!({
        "fp": fp,
        "label": s.label,
        "client": s.client,
        "clientLabel": s.client_label,
        "api": s.api,
        "via": s.via,
        "provider": s.provider,
        "key": s.key,
        "created": s.created,
        "lastSeen": s.last_seen,
        "ageSecs": now.saturating_sub(s.created),
        "idleSecs": now.saturating_sub(s.last_seen),
        "active": now.saturating_sub(s.last_seen) < config::SESSION_ACTIVE_SECS,
        "working": working.contains(fp),
        "turns": s.turns,
        "errors": s.errors,
        "cost": (s.cost * 1e6).round() / 1e6,
        "costPerTurn": if s.turns > 0 { (s.cost / s.turns as f64 * 1e6).round() / 1e6 } else { 0.0 },
        "inTokens": s.in_tokens,
        "outTokens": s.out_tokens,
        "tokensPerTurn": (s.in_tokens + s.out_tokens).checked_div(s.turns).unwrap_or(0),
        "bytesUp": s.bytes_up,
        "bytesDown": s.bytes_down,
        "bytesTotal": s.bytes_up + s.bytes_down,
        "maxLatencyMs": s.max_latency_ms,
        "compactions": s.compactions,
        "keySwitches": s.key_switches,
        "models": s.models,
        "trackedHashes": s.msg_hashes.len(),
        "turnsPerMinute": (s.turns as f64 / (dur as f64 / 60.0) * 100.0).round() / 100.0,
        "errorList": errs,
    })
}

// ── Server-Sent Events ──────────────────────────────────────────────────────

/// Full history at a requested resolution, for the chart's range selector.
///
/// Kept OUT of the SSE snapshot on purpose. The snapshot is re-serialised on
/// every state change, so anything in it is paid for continuously; a 30-day
/// series is looked at deliberately and rarely, so it is pulled on demand.
///
/// `points` caps what is returned by bucketing N source buckets into one. That
/// matters because a 2-year daily query is 730 rows but a 6-hour minute query
/// asked for at 1-minute detail on a 375px screen is 360 rows for ~180 usable
/// pixels — drawing them all costs time and shows nothing extra.
pub fn history(app: &Arc<App>, res: config::HistoryRes, points: usize) -> Value {
    let g = app.lock();
    let series = match res {
        config::HistoryRes::Minute => &g.minutes,
        config::HistoryRes::Hour => &g.hours,
        config::HistoryRes::Day => &g.days,
    };
    let total = series.len();
    let points = points.clamp(1, 2000);
    // Group size chosen so the output never exceeds `points`.
    let group = total.div_ceil(points).max(1);

    let mut rows: Vec<Value> = Vec::with_capacity(total / group + 1);
    let mut chunk: Vec<&crate::state::MinuteBucket> = Vec::with_capacity(group);
    for b in series.iter() {
        chunk.push(b);
        if chunk.len() == group {
            rows.push(merge_buckets(&chunk, res));
            chunk.clear();
        }
    }
    // Trailing partial group still gets drawn — the current minute/hour/day is
    // the most interesting point on the chart.
    if !chunk.is_empty() {
        rows.push(merge_buckets(&chunk, res));
    }

    json!({
        "res": res.label(),
        "bucketSecs": res.secs(),
        "group": group,
        "total": total,
        "rows": rows,
    })
}

/// Merge N adjacent buckets into one chart point.
///
/// Latency is a weighted mean via the retained sum/count — averaging the
/// per-bucket averages would weight a 1-request minute the same as a
/// 500-request minute.
fn merge_buckets(chunk: &[&crate::state::MinuteBucket], res: config::HistoryRes) -> Value {
    let mut requests = 0u64;
    let mut errors = 0u64;
    let mut bytes_up = 0u64;
    let mut bytes_down = 0u64;
    let mut cost = 0.0f64;
    let mut lat_sum = 0u64;
    let mut lat_n = 0u64;
    for b in chunk {
        requests += b.requests;
        errors += b.errors;
        bytes_up += b.bytes_up;
        bytes_down += b.bytes_down;
        cost += b.cost;
        lat_sum += b.latency_sum_ms;
        lat_n += b.latency_n;
    }
    json!({
        // Timestamp of the FIRST bucket in the group, so the x-axis stays
        // monotonic and a partial trailing group is not drawn in the future.
        "t": chunk[0].t * res.secs(),
        "span": chunk.len() as u64 * res.secs(),
        "requests": requests,
        "errors": errors,
        "bytesUp": bytes_up,
        "bytesDown": bytes_down,
        "cost": (cost * 1e6).round() / 1e6,
        "avgMs": lat_sum.checked_div(lat_n).unwrap_or(0),
    })
}

/// Full uptime series per provider — the part the snapshot deliberately trims.
pub fn uptime_full(app: &Arc<App>, provider: &str) -> Value {
    let g = app.lock();
    let mut out: Vec<Value> = Vec::new();
    for p in app.providers.read().unwrap().iter() {
        if !provider.is_empty() && p.id != provider {
            continue;
        }
        let Some(r) = g.providers.get(p.id) else {
            continue;
        };
        let samples: Vec<Value> = r
            .uptime
            .iter()
            .map(|s| json!({ "t": s.t, "up": s.up, "ms": s.ms }))
            .collect();
        let down = r.uptime.iter().filter(|s| !s.up).count();
        out.push(json!({
            "id": p.id,
            "label": p.label,
            "host": p.host,
            "samples": samples,
            "total": r.uptime.len(),
            "down": down,
        }));
    }
    json!({ "providers": out, "probeSecs": config::UPTIME_PROBE_SECS })
}

/// SSE stream that pushes ONLY when state changes.
///
/// A naive dashboard polls every second and keeps the CPU awake. This instead
/// watches `App::rev()` — bumped by any real mutation — and serialises only on
/// change, with a comment heartbeat otherwise. Idle cost is ~0.
pub fn sse(app: Arc<App>) -> Response<Body> {
    use http_body_util::StreamBody;
    use hyper::body::Frame;

    let (tx, rx) =
        tokio::sync::mpsc::channel::<Result<Frame<bytes::Bytes>, std::convert::Infallible>>(8);

    tokio::spawn(async move {
        // Initial full snapshot so the page paints immediately.
        let first = format!("event: snapshot\ndata: {}\n\n", snapshot(&app));
        if tx
            .send(Ok(Frame::data(bytes::Bytes::from(first))))
            .await
            .is_err()
        {
            return;
        }

        let mut last_rev = app.rev();
        let mut since_beat = 0u32;

        loop {
            // 700ms cadence is a good balance: changes feel instant, and the
            // wake-up itself is trivial because nothing is serialised unless the
            // revision moved.
            tokio::time::sleep(Duration::from_millis(700)).await;

            let rev = app.rev();
            if rev != last_rev {
                last_rev = rev;
                since_beat = 0;
                let payload = format!("event: snapshot\ndata: {}\n\n", snapshot(&app));
                if tx
                    .send(Ok(Frame::data(bytes::Bytes::from(payload))))
                    .await
                    .is_err()
                {
                    return; // client closed the tab
                }
            } else {
                since_beat += 1;
                // ~20s heartbeat: keeps proxies from closing an &idle stream.
                if since_beat >= 28 {
                    since_beat = 0;
                    if tx
                        .send(Ok(Frame::data(bytes::Bytes::from(":\n\n".to_string()))))
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
            }
        }
    });

    let stream = tokio_stream_shim::ReceiverStream::new(rx);
    let body = StreamBody::new(stream);
    let mut r = Response::new(http_body_util::BodyExt::boxed(body));
    r.headers_mut().insert(
        HeaderName::from_static("content-type"),
        HeaderValue::from_static("text/event-stream"),
    );
    r.headers_mut().insert(
        HeaderName::from_static("cache-control"),
        HeaderValue::from_static("no-store"),
    );
    r.headers_mut().insert(
        HeaderName::from_static("x-accel-buffering"),
        HeaderValue::from_static("no"),
    );
    r
}

/// Minimal `Stream` adapter over an mpsc receiver.
///
/// Hand-rolled to avoid adding `tokio-stream` for one type — fewer deps means a
/// smaller binary and less to audit.
mod tokio_stream_shim {
    use std::pin::Pin;
    use std::task::{Context, Poll};

    pub struct ReceiverStream<T> {
        inner: tokio::sync::mpsc::Receiver<T>,
    }

    impl<T> ReceiverStream<T> {
        pub fn new(inner: tokio::sync::mpsc::Receiver<T>) -> Self {
            Self { inner }
        }
    }

    impl<T> futures_core::Stream for ReceiverStream<T> {
        type Item = T;
        fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<T>> {
            self.inner.poll_recv(cx)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::MinuteBucket;

    fn bucket(t: u64, requests: u64, errors: u64, lat_sum: u64, lat_n: u64) -> MinuteBucket {
        MinuteBucket {
            t,
            requests,
            errors,
            bytes_up: requests * 10,
            bytes_down: requests * 20,
            cost: requests as f64 * 0.5,
            latency_sum_ms: lat_sum,
            latency_n: lat_n,
        }
    }

    #[test]
    fn merge_weights_latency_by_request_count() {
        // Averaging the per-bucket averages would weight a 1-request minute the
        // same as a 500-request one. A 1x1000ms minute merged with a 999x1ms
        // minute must land near 2ms, not near 500ms.
        let a = bucket(0, 1, 0, 1000, 1);
        let b = bucket(1, 999, 0, 999, 999);
        let refs = vec![&a, &b];
        let v = merge_buckets(&refs, config::HistoryRes::Minute);
        let avg = v["avgMs"].as_u64().unwrap();
        assert_eq!(
            avg, 1,
            "weighted mean of 1000ms over 999+1 samples, got {avg}ms"
        );
        assert!(
            avg < 10,
            "must not be dragged to the naive mean, got {avg}ms"
        );
    }

    #[test]
    fn merge_sums_counters_and_timestamps_from_the_first_bucket() {
        // The x-axis must stay monotonic, so a merged point is stamped with the
        // START of its group — never the end, which would plot in the future.
        let a = bucket(100, 2, 1, 20, 2);
        let b = bucket(101, 3, 0, 30, 3);
        let refs = vec![&a, &b];
        let v = merge_buckets(&refs, config::HistoryRes::Minute);
        assert_eq!(v["t"].as_u64().unwrap(), 100 * 60);
        assert_eq!(v["span"].as_u64().unwrap(), 120);
        assert_eq!(v["requests"].as_u64().unwrap(), 5);
        assert_eq!(v["errors"].as_u64().unwrap(), 1);
    }

    #[test]
    fn hour_and_day_resolutions_scale_the_timestamp() {
        let a = bucket(9, 1, 0, 10, 1);
        let refs = vec![&a];
        assert_eq!(
            merge_buckets(&refs, config::HistoryRes::Hour)["t"]
                .as_u64()
                .unwrap(),
            9 * 3600
        );
        assert_eq!(
            merge_buckets(&refs, config::HistoryRes::Day)["t"]
                .as_u64()
                .unwrap(),
            9 * 86400
        );
    }

    #[test]
    fn empty_latency_reports_zero_not_a_divide_by_zero() {
        // A bucket can exist with requests but no recorded latency (e.g. only
        // transport failures). Dividing by latency_n would panic.
        let a = bucket(0, 3, 3, 0, 0);
        let refs = vec![&a];
        assert_eq!(
            merge_buckets(&refs, config::HistoryRes::Minute)["avgMs"]
                .as_u64()
                .unwrap(),
            0
        );
    }

    #[test]
    fn history_resolution_parses_forgivingly() {
        // A wrong x-scale beats an empty chart, so unknown values fall back to
        // the finest resolution rather than erroring.
        assert_eq!(config::HistoryRes::parse("hour"), config::HistoryRes::Hour);
        assert_eq!(config::HistoryRes::parse("d"), config::HistoryRes::Day);
        assert_eq!(
            config::HistoryRes::parse("nonsense"),
            config::HistoryRes::Minute
        );
        assert_eq!(config::HistoryRes::parse(""), config::HistoryRes::Minute);
    }

    #[test]
    fn bucket_seconds_match_their_resolution() {
        assert_eq!(config::HistoryRes::Minute.secs(), 60);
        assert_eq!(config::HistoryRes::Hour.secs(), 3600);
        assert_eq!(config::HistoryRes::Day.secs(), 86400);
    }

    #[test]
    fn snapshot_caps_are_smaller_than_what_is_retained() {
        // The whole point of the snapshot caps is that they trim. If a cap ever
        // exceeded its retention the trim would silently become a no-op and the
        // 161KB-per-push regression would return unnoticed.
        // Compile-time, not runtime: both sides are consts, so an inconsistent
        // edit must fail the build rather than wait for the test to be run.
        const _: () = assert!(config::SNAPSHOT_UPTIME_SAMPLES < config::UPTIME_SAMPLES);
        const _: () = assert!(config::SNAPSHOT_MINUTES < config::HISTORY_MINUTES);
    }

    #[test]
    fn masking_never_leaks_a_key_and_survives_multibyte() {
        // /api/keys/verify accepts arbitrary JSON strings, so a multi-byte input
        // must not panic — under `panic = "abort"` that would kill the gateway.
        let masked = mask("sk-abcdefghijklmnopqrstuvwxyz1234");
        assert!(masked.contains('…'));
        assert!(!masked.contains("mnopqrst"));
        assert_eq!(mask("日本語"), "sk-…");
    }
}
