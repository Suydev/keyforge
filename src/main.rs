//! KeyForge — multi-provider AI gateway with a live dashboard.
//!
//! # What it does
//! Owns a pool of ~1700 API keys across providers, and presents them to clients
//! as a single endpoint that never runs out of credit and never surfaces a
//! retryable error. OpenCode and Claude Code both point at 127.0.0.1 and never
//! need restarting when a key dies.
//!
//! # Surfaces
//! ```text
//! POST /v1/messages            Anthropic Messages API (Claude Code)
//! POST /v1/chat/completions    OpenAI-compatible (OpenCode)
//! GET  /v1/models              model list
//!      /tabi/v1/...            force KeyForge Token
//!      /gorouter/v1/...        force GoRouter
//!      /auto/v1/...            explicit auto-route (same as bare /v1)
//!
//! GET  /                       dashboard
//! GET  /api/stream             SSE, pushes only on change
//! GET  /api/snapshot           one-shot JSON of everything
//! GET  /api/keys?provider=…    per-key balances (masked)
//! POST /api/keys/verify        verify a pasted key, adopt if funded
//! POST /api/refresh            trigger a free balance sweep
//! GET  /api/health             liveness
//! ```
//!
//! Anything not matching the above returns a LOCAL 404. Critically, unknown
//! paths are never forwarded upstream — the old Node proxy did that, which meant
//! a stray browser hit on `/` got signed with a real API key and relayed to the
//! provider.

// ── Lint policy ──────────────────────────────────────────────────────────────
// `dead_code` is allowed at the crate level, deliberately, and only this lint.
//
// The codebase carries a documented backlog of wired-but-not-yet-called
// machinery: multi-host failover (`Settings::hosts`), settings hot-reload
// (`settings_mtime`), a millisecond clock for finer-grained proxy rotation
// (`now_ms`), and failure-analysis plumbing. Each is reachable, tested, and
// scheduled — see HANDOFF notes in the repository history.
//
// The alternative was ~24 individual `#[allow(dead_code)]` attributes scattered
// across six modules, which is noise that obscures rather than documents. Every
// other lint, including all of clippy's, remains denied in CI via
// `-D warnings`, so this does not weaken the check for anything that is
// genuinely wrong.
#![allow(dead_code)]

mod api;
mod classify;
mod config;
mod helpers;
mod link;
mod modelsync;
mod proxy;
mod router;
mod session;
mod settings;
mod state;
mod upstream;

use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use router::{full, Body};
use state::App;
use std::net::SocketAddr;
use std::sync::Arc;

/// Embedded at compile time so the binary is self-contained — no asset paths to
/// get wrong, and the dashboard cannot break because a file moved.
const INDEX_HTML: &str = include_str!("../web/index.html");
const APP_CSS: &str = include_str!("../web/app.css");
const APP_JS: &str = include_str!("../web/app.js");
const CHARTS_JS: &str = include_str!("../web/charts.js");

fn asset(body: &'static str, content_type: &'static str) -> Response<Body> {
    let mut r = Response::new(full(body));
    r.headers_mut().insert(
        hyper::header::CONTENT_TYPE,
        hyper::header::HeaderValue::from_static(content_type),
    );
    // `no-store`, not `no-cache`.
    //
    // These are compiled in, so they change exactly when the binary changes —
    // but there is no ETag or Last-Modified for a browser to revalidate against,
    // and `no-cache` without a validator still lets an ES module linger in the
    // module map across reloads. That produced a genuinely confusing bug: a
    // fixed dashboard still throwing an error from a line that no longer
    // existed in the served file.
    //
    // `no-store` forbids keeping a copy at all, so a reload is always the real
    // asset. Over loopback the re-download is ~75KB and costs nothing measurable.
    r.headers_mut().insert(
        hyper::header::CACHE_CONTROL,
        hyper::header::HeaderValue::from_static("no-store, must-revalidate"),
    );
    r
}

fn not_found(path: &str) -> Response<Body> {
    let body = serde_json::json!({
        "error": {
            "type": "not_found",
            "message": format!("keyforge: no route for {path}"),
            "hint": "API paths must start with /v1/ (optionally prefixed /tabi/ or /gorouter/). Dashboard is at /",
        }
    });
    let mut r = Response::new(full(serde_json::to_vec_pretty(&body).unwrap_or_default()));
    *r.status_mut() = StatusCode::NOT_FOUND;
    r.headers_mut().insert(
        hyper::header::CONTENT_TYPE,
        hyper::header::HeaderValue::from_static("application/json"),
    );
    r
}

/// Is this a path we should proxy to a provider?
///
/// Deliberately strict: only `/v1/...`, optionally behind a known provider
/// prefix. Everything else is served locally or 404s. This is what stops the
/// gateway being an open relay.
fn is_api_path(path: &str, app: &App) -> bool {
    if path.starts_with("/v1/") {
        return true;
    }
    if path.starts_with("/auto/v1/") {
        return true;
    }
    app.providers.read().unwrap()
        .iter()
        .any(|p| path.starts_with(&format!("/{}/v1/", p.id)))
}

async fn route(app: Arc<App>, req: Request<hyper::body::Incoming>) -> Response<Body> {
    let path = req.uri().path().to_string();
    let method = req.method().clone();

    // ── static dashboard ────────────────────────────────────────────────────
    //
    // HEAD is served exactly like GET, minus the body. Matching only GET made
    // `HEAD /app.js` fall through the asset table into the API router and answer
    // 404, which is wrong per RFC 9110 (HEAD must mirror GET's headers) and
    // breaks any liveness check or debugger that probes with HEAD.
    let is_get = method == Method::GET;
    let is_head = method == Method::HEAD;
    if is_get || is_head {
        let hit = match path.as_str() {
            "/" | "/index.html" => Some((INDEX_HTML, "text/html; charset=utf-8")),
            "/app.css" => Some((APP_CSS, "text/css; charset=utf-8")),
            "/app.js" => Some((APP_JS, "text/javascript; charset=utf-8")),
            "/charts.js" => Some((CHARTS_JS, "text/javascript; charset=utf-8")),
            _ => None,
        };
        if let Some((body, ctype)) = hit {
            let mut r = asset(body, ctype);
            if is_head {
                // Keep content-type and content-length, drop the body.
                let len = body.len();
                *r.body_mut() = full("");
                r.headers_mut().insert(
                    hyper::header::CONTENT_LENGTH,
                    hyper::header::HeaderValue::from_str(&len.to_string())
                        .unwrap_or(hyper::header::HeaderValue::from_static("0")),
                );
            }
            return r;
        }
        // Browsers request this unprompted; answer locally so it never becomes
        // an upstream request.
        if path == "/favicon.ico" {
            let mut r = Response::new(full(""));
            *r.status_mut() = StatusCode::NO_CONTENT;
            return r;
        }
    }

    // ── dashboard API ───────────────────────────────────────────────────────
    if path == "/api/stream" && method == Method::GET {
        return api::sse(app);
    }
    if path == "/api/snapshot" && method == Method::GET {
        return api::ok(api::snapshot(&app));
    }
    if path == "/api/health" && method == Method::GET {
        return api::ok(serde_json::json!({
            "ok": true,
            "offline": app.is_offline(),
            "rev": app.rev(),
        }));
    }
    if path == "/api/keys" && method == Method::GET {
        let q = req.uri().query().unwrap_or("");
        let provider = query_param(q, "provider").unwrap_or_else(|| {
            app.providers.read().unwrap()
                .first()
                .map(|p| p.id.to_string())
                .unwrap_or_default()
        });
        let limit = query_param(q, "limit")
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(200)
            .min(2000);
        return api::ok(api::keys_page(&app, &provider, limit));
    }
    if path == "/api/keys/verify" && method == Method::POST {
        let body = match collect_body(req).await {
            Ok(b) => b,
            Err(e) => return api::bad(&e),
        };
        let key = serde_json::from_slice::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| v.get("key").and_then(|k| k.as_str()).map(str::to_string));
        let Some(key) = key else {
            return api::bad("expected JSON body: {\"key\":\"sk-…\"}");
        };
        return api::ok(api::verify_and_add(&app, &key).await);
    }
    if path == "/api/leaderboard" && method == Method::GET {
        return api::ok(api::leaderboard(&app));
    }
    if path == "/api/session" && method == Method::GET {
        let fp = query_param(req.uri().query().unwrap_or(""), "fp").unwrap_or_default();
        if fp.is_empty() {
            return api::bad("expected ?fp=<session fingerprint>");
        }
        return api::ok(api::session_detail(&app, &fp));
    }
    // Full history at a chosen resolution. `points` lets a small screen ask for
    // fewer buckets instead of drawing 700 rows into 180 pixels.
    if path == "/api/history" && method == Method::GET {
        let q = req.uri().query().unwrap_or("");
        let res = config::HistoryRes::parse(&query_param(q, "res").unwrap_or_default());
        let points = query_param(q, "points")
            .and_then(|v| v.parse().ok())
            .unwrap_or(240usize);
        return api::ok(api::history(&app, res, points));
    }
    // Full uptime series — the snapshot only carries the visible tail.
    if path == "/api/uptime" && method == Method::GET {
        let provider = query_param(req.uri().query().unwrap_or(""), "provider").unwrap_or_default();
        return api::ok(api::uptime_full(&app, &provider));
    }
    if path == "/api/errors" && method == Method::GET {
        let q = req.uri().query().unwrap_or("");
        let limit = query_param(q, "limit")
            .and_then(|v| v.parse().ok())
            .unwrap_or(120usize)
            .min(500);
        let class = query_param(q, "class").unwrap_or_default();
        let rows: Vec<serde_json::Value> = app
            .recent_errors(500)
            .into_iter()
            .filter(|e| class.is_empty() || e.class.eq_ignore_ascii_case(&class))
            .take(limit)
            .map(|e| {
                serde_json::json!({
                    "t": e.t, "class": e.class, "provider": e.provider, "host": e.host,
                    "key": e.key, "model": e.model, "status": e.status, "message": e.message,
                    "action": e.action, "session": e.session, "round": e.round,
                    "latencyMs": e.latency_ms, "remaining": e.remaining, "required": e.required,
                    // These four are the diagnostic payload and were being
                    // dropped here while the SSE snapshot carried them, so both
                    // documented troubleshooting recipes returned nothing:
                    //   reqBytes + budgetSecs  explains most timeout clusters —
                    //     a large body against a small budget means the budget
                    //     was the problem, not the provider
                    //   proxy                  attributes a failure to an
                    //     egress, which is how you notice traffic has silently
                    //     collapsed onto one IP
                    //   ttfbMs / headTimeout / streaming  separate "slow" from
                    //     "dead" without re-reading the message text
                    "reqBytes": e.req_bytes, "budgetSecs": e.budget_secs,
                    "ttfbMs": e.ttfb_ms, "headTimeout": e.head_timeout,
                    "streaming": e.streaming, "proxy": e.proxy,
                })
            })
            .collect();
        return api::ok(serde_json::json!({ "rows": rows }));
    }
    if path == "/api/inflight" && method == Method::GET {
        let rows: Vec<serde_json::Value> = app
            .inflight_list()
            .into_iter()
            .map(|f| {
                serde_json::json!({
                    "id": f.id, "session": f.session, "label": f.label, "provider": f.provider,
                    "host": f.host, "key": f.key, "model": f.model, "client": f.client,
                    "elapsedSecs": config::now_secs().saturating_sub(f.started),
                    "round": f.round, "attempts": f.attempts, "streaming": f.streaming,
                    "phase": f.phase,
                })
            })
            .collect();
        return api::ok(serde_json::json!({ "rows": rows, "count": rows.len() }));
    }
    if path == "/api/proxies" && method == Method::GET {
        let (rot, direct, cooling) = upstream::proxy_pool().stats();
        let rows: Vec<serde_json::Value> = upstream::proxy_pool()
            .snapshot()
            .into_iter()
            .map(|(e, h)| {
                serde_json::json!({
                    "addr": e.addr(),
                    "host": e.host,
                    "port": e.port,
                    "auth": e.user.is_some(),
                    "ok": h.ok,
                    "fail": h.fail,
                    "cooling": h.cool_until > config::now_secs(),
                    "coolingFor": h.cool_until.saturating_sub(config::now_secs()),
                    "lastError": h.last_error,
                    "ewmaMs": h.ewma_ms.map(|v| v.round()),
                    "lastUsed": h.last_used,
                })
            })
            .collect();
        return api::ok(serde_json::json!({
            "enabled": upstream::proxy_pool().is_enabled(),
            "count": rows.len(),
            "cooling": cooling,
            "rotations": rot,
            "directFallbacks": direct,
            "rows": rows,
        }));
    }
    if path == "/api/proxies/toggle" && method == Method::POST {
        let on = !upstream::proxy_pool().is_enabled();
        upstream::proxy_pool().set_enabled(on);
        app.event(
            "info",
            format!(
                "outbound proxies {}",
                if on { "enabled" } else { "disabled" }
            ),
        );
        return api::ok(serde_json::json!({ "ok": true, "enabled": on }));
    }
    // Run a maintenance cycle now: evict dead, refill from candidates, persist.
    if path == "/api/proxies/maintain" && method == Method::POST {
        let a = app.clone();
        tokio::spawn(async move {
            crate::helpers::proxy_maint_cycle(&a).await;
        });
        return api::ok(serde_json::json!({
            "ok": true,
            "message": "proxy maintenance started — watch the events feed"
        }));
    }
    if path == "/api/proxies/reload" && method == Method::POST {
        let r = upstream::proxy_pool().load_file_detailed(&config::proxies_path());
        // A refused file is a warning, not an "ok": the button appeared to work
        // while emptying the pool, which is precisely how this went unnoticed.
        app.event(
            if r.kept_existing { "warn" } else { "info" },
            format!("proxy reload: {}", r.note),
        );
        return api::ok(serde_json::json!({
            "ok": !r.kept_existing,
            "count": r.loaded,
            "skipped": r.skipped,
            "keptExisting": r.kept_existing,
            "message": r.note,
        }));
    }
    if path == "/api/settings" && method == Method::GET {
        let s = app.settings.lock().unwrap_or_else(|e| e.into_inner());
        return api::ok(serde_json::to_value(&*s).unwrap_or_default());
    }
    if path == "/api/settings" && method == Method::POST {
        let body = match collect_body(req).await {
            Ok(b) => b,
            Err(e) => return api::bad(&e),
        };
        let new_settings: crate::settings::Settings = match serde_json::from_slice(&body) {
            Ok(s) => s,
            Err(e) => return api::bad(&format!("invalid settings JSON: {e}")),
        };
        match new_settings.save() {
            Ok(()) => {
                let mut s = app.settings.lock().unwrap_or_else(|e| e.into_inner());
                *s = new_settings;
                app.event("info", "settings updated via dashboard".to_string());
                return api::ok(serde_json::json!({ "ok": true, "message": "settings saved" }));
            }
            Err(e) => return api::bad(&format!("failed to save settings: {e}")),
        }
    }
    if path == "/api/models/sync" && method == Method::POST {
        let a = app.clone();
        tokio::spawn(async move {
            let (added, removed) = crate::modelsync::run_once(&a).await;
            eprintln!("[modelsync] manual: +{} -{}", added.len(), removed.len());
        });
        return api::ok(serde_json::json!({ "ok": true, "message": "model sync started" }));
    }
    if path == "/api/keys/probe" && method == Method::POST {
        let a = app.clone();
        tokio::spawn(async move {
            crate::helpers::probe_all_unprobed(a, "manual").await;
        });
        return api::ok(serde_json::json!({ "ok": true, "message": "probing unmeasured keys" }));
    }
    if path == "/api/refresh" && method == Method::POST {
        let a = app.clone();
        tokio::spawn(async move {
            helpers::sweep_now(a).await;
        });
        return api::ok(serde_json::json!({ "ok": true, "message": "free balance sweep started" }));
    }

    // ── proxy ───────────────────────────────────────────────────────────────
    if is_api_path(&path, &app) {
        return router::handle_proxy(app, req).await;
    }

    not_found(&path)
}

fn query_param(query: &str, name: &str) -> Option<String> {
    for pair in query.split('&') {
        let mut it = pair.splitn(2, '=');
        if it.next() == Some(name) {
            return it.next().map(percent_decode);
        }
    }
    None
}

/// Minimal percent-decoding — enough for provider ids and integers.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                if let Ok(b) = u8::from_str_radix(hex, 16) {
                    out.push(b);
                    i += 3;
                    continue;
                }
                out.push(bytes[i]);
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

async fn collect_body(req: Request<hyper::body::Incoming>) -> Result<bytes::Bytes, String> {
    use http_body_util::BodyExt;
    req.into_body()
        .collect()
        .await
        .map(|c| c.to_bytes())
        .map_err(|e| format!("could not read body: {e}"))
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    // 2 worker threads: this is an I/O-bound proxy on a tablet. More threads
    // would only add scheduler churn and wake-ups.
    //
    // Lock the state file BEFORE loading it. Two instances on the same state
    // file silently overwrite each other's key pool.
    if let Err(e) = App::acquire_lock() {
        eprintln!("keyforge: {e}");
        eprintln!("  stop the other instance, or set KEYFORGE_STATE to a different path.");
        std::process::exit(1);
    }

    let app = Arc::new(App::new());
    app.load();
    let report = app.load_key_files();
    app.save();

    // Outbound proxy pool. Enabled by default when proxies.txt has entries;
    // KEYFORGE_NO_PROXY=1 forces direct so a proxy outage can never block work.
    let proxy_file = config::proxies_path();
    let mut n_proxies = upstream::proxy_pool().load_file(&proxy_file);
    // Cold start: no vetted file yet, so seed straight from the candidate list.
    // Unvetted entries are fine here — the maintainer evicts whatever fails, and
    // an unvetted pool still beats going direct into a Cloudflare block.
    if n_proxies == 0 {
        for path in config::proxy_candidates_paths() {
            if let Ok(bytes) = std::fs::read(&path) {
                let cands = proxy::parse_proxyscrape_json(&bytes);
                if !cands.is_empty() {
                    let take = cands.into_iter().take(config::proxy_vet_batch()).collect();
                    n_proxies = upstream::proxy_pool().add_endpoints(take);
                    println!("  proxies    seeded {n_proxies} unvetted from {}", path.display());
                    break;
                }
            }
        }
    }
    let want_proxy = std::env::var("KEYFORGE_NO_PROXY").ok().as_deref() != Some("1");
    upstream::proxy_pool().set_enabled(want_proxy && n_proxies > 0);

    let port = config::port();
    let addr = SocketAddr::from((config::BIND_ADDR, port));

    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("keyforge: cannot bind {addr}: {e}");
            eprintln!("  is another gateway or the old node proxy already running?");
            eprintln!("  try: pkill -f keyforge-proxy   (or set KEYFORGE_PORT)");
            App::release_lock();
            std::process::exit(1);
        }
    };

    println!("keyforge listening on http://{addr}");
    for (id, total, added) in &report {
        let (alive, funds) = app.provider_summary(id);
        println!("  {id:<9} {total:>4} keys  {alive:>4} usable  ${funds:.2} known  (+{added} new)");
    }
    if upstream::proxy_pool().is_enabled() {
        println!("  egress     via {n_proxies} rotating proxies (LRU, direct fallback)");
    } else {
        println!("  egress     direct (no proxies loaded or disabled)");
    }
    println!("  dashboard  http://127.0.0.1:{port}/");
    println!("  anthropic  http://127.0.0.1:{port}/v1/messages");
    println!("  openai     http://127.0.0.1:{port}/v1/chat/completions");

    helpers::spawn_all(app.clone());

    // Save state on Ctrl-C so nothing learned is lost.
    {
        let a = app.clone();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                eprintln!("\nkeyforge: saving state and exiting");
                a.dirty.store(true, std::sync::atomic::Ordering::Relaxed);
                a.save();
                App::release_lock();
                std::process::exit(0);
            }
        });
    }

    // ── SIGTERM ──────────────────────────────────────────────────────────────
    // `tabi stop` and `tabi restart` send SIGTERM via pkill, and
    // `tokio::signal::ctrl_c()` above is SIGINT only. So until this existed,
    // every scripted stop skipped `save()` and `release_lock()` entirely:
    // up to STATE_FLUSH_SECS (20s) of learned holds, balances and TTFB EWMAs
    // discarded per restart, plus a leaked PID lockfile that the next start had
    // to reap. The launcher's own comment already claimed "state is saved on
    // SIGTERM, so prefer it" — this is what makes that true.
    //
    // Prerequisite for any automated restart: a watchdog that restarts the
    // gateway without this would quietly destroy learning on every recovery.
    {
        let a = app.clone();
        tokio::spawn(async move {
            #[cfg(unix)]
            let shutdown = async {
                let mut term = match tokio::signal::unix::signal(
                    tokio::signal::unix::SignalKind::terminate(),
                ) {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("keyforge: cannot install SIGTERM handler: {e}");
                        return;
                    }
                };
                term.recv().await;
            };
            #[cfg(not(unix))]
            let shutdown = async {
                let _ = tokio::signal::ctrl_c().await;
            };
            shutdown.await;
            eprintln!("keyforge: shutdown signal — saving state and exiting");
            a.dirty.store(true, std::sync::atomic::Ordering::Relaxed);
            a.save();
            App::release_lock();
            std::process::exit(0);
        });
    }

    loop {
        let (stream, _peer) = match listener.accept().await {
            Ok(x) => x,
            Err(e) => {
                eprintln!("[accept] {e}");
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                continue;
            }
        };
        let io = TokioIo::new(stream);
        let app = app.clone();

        tokio::spawn(async move {
            let svc = service_fn(move |req| {
                let app = app.clone();
                async move { Ok::<_, std::convert::Infallible>(route(app, req).await) }
            });

            if let Err(e) = http1::Builder::new()
                // SSE and long streams must not be killed by a keep-alive timer.
                .keep_alive(true)
                .serve_connection(io, svc)
                .await
            {
                let msg = e.to_string();
                // Client disconnects are normal (closing a dashboard tab).
                if !msg.contains("connection reset")
                    && !msg.contains("broken pipe")
                    && !msg.contains("closed")
                {
                    eprintln!("[conn] {msg}");
                }
            }
        });
    }
}
