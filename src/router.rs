//! The router: one request in, one good response out — no matter what.
//!
//! # The contract
//!
//! `<none> [retrying in 1m 51s attempt #1]` is the failure mode we are
//! engineering against. That message is OpenCode's own backoff, and it appears
//! when the gateway does any of:
//!   * refuses the connection (not running),
//!   * returns a *retryable* status (429 / 5xx),
//!   * drops the connection mid-response,
//!   * returns a body OpenCode cannot parse (hence `<none>` — no message found).
//!
//! So this router guarantees:
//!   1. **Never emit a retryable status.** Only 2xx, or a terminal 400 whose body
//!      is valid in BOTH Anthropic and OpenAI error shapes so the client always
//!      finds a message. No `<none>`, no client-side backoff.
//!   2. **Retry internally, patiently.** Transient trouble (429 storms, Cloudflare
//!      502, provider 5xx) is retried across keys and providers for as long as it
//!      takes. Offline waits indefinitely and resumes the instant DNS returns.
//!   3. **Stream, never buffer.** Only the response *head* is awaited before
//!      committing. A successful body is piped straight through, so a long
//!      thinking stream cannot be truncated or delayed.
//!   4. **Never burn a healthy key.** Only an explicit quota/auth signal retires
//!      a key; WAF and 5xx leave it untouched.

use crate::classify::{self, ErrClass};
use crate::config::{self, ApiFlavor};
use crate::state::App;
use crate::upstream::{self, HeadAttempt};
use bytes::Bytes;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::header::{HeaderMap, HeaderName, HeaderValue};
use hyper::{Request, Response, StatusCode};
use std::sync::Arc;
use std::time::Duration;

pub type Body = BoxBody<Bytes, std::convert::Infallible>;

pub fn full(b: impl Into<Bytes>) -> Body {
    Full::new(b.into()).boxed()
}

mod pass {
    use bytes::Bytes;
    use hyper::body::{Body, Frame, Incoming, SizeHint};
    use std::pin::Pin;
    use std::task::{Context, Poll};

    /// Wraps an upstream body and swallows read errors by ending the stream.
    ///
    /// Our response body type is `Infallible` because headers are already
    /// committed by the time we stream — there is no way to surface a late
    /// error, so truncating is the only honest option. This is also what makes
    /// SSE passthrough zero-copy: frames go straight out as they arrive.
    pub struct PassBody {
        pub inner: Incoming,
        pub done: bool,
    }

    impl Body for PassBody {
        type Data = Bytes;
        type Error = std::convert::Infallible;

        fn poll_frame(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
        ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
            if self.done {
                return Poll::Ready(None);
            }
            match Pin::new(&mut self.inner).poll_frame(cx) {
                Poll::Ready(Some(Ok(f))) => Poll::Ready(Some(Ok(f))),
                Poll::Ready(Some(Err(_))) => {
                    self.done = true;
                    Poll::Ready(None)
                }
                Poll::Ready(None) => Poll::Ready(None),
                Poll::Pending => Poll::Pending,
            }
        }

        fn size_hint(&self) -> SizeHint {
            self.inner.size_hint()
        }
    }
}

fn pipe(body: Incoming) -> Body {
    pass::PassBody {
        inner: body,
        done: false,
    }
    .boxed()
}

// ── request shape ───────────────────────────────────────────────────────────

struct Route {
    /// Explicitly pinned provider, if the caller used `/tabi/...` or `/gorouter/...`.
    pinned: Option<String>,
    /// Path to send upstream (always begins `/v1/...`).
    path: String,
    flavor: ApiFlavor,
}

fn parse_route(uri_path: &str, providers: &[config::Provider]) -> Route {
    let mut pinned = None;
    let mut path = uri_path.to_string();

    for p in providers {
        let prefix = format!("/{}/", p.id);
        if path.starts_with(&prefix) {
            pinned = Some(p.id.to_string());
            path = path[p.id.len() + 1..].to_string();
            break;
        }
    }
    if path.starts_with("/auto/") {
        path = path["/auto".len()..].to_string();
    }

    let flavor = if path.starts_with("/v1/messages") {
        ApiFlavor::Anthropic
    } else if path.starts_with("/v1/chat/completions") || path.starts_with("/v1/completions") {
        ApiFlavor::OpenAi
    } else {
        ApiFlavor::Passthrough
    };

    Route {
        pinned,
        path,
        flavor,
    }
}

/// Identify the conversation this request belongs to.
///
/// Every agent here uses the same model AND the same system prompt, so the
/// system block cannot distinguish sessions — the FIRST USER MESSAGE can. It is
/// unique per conversation and stable as the conversation grows, which is what
/// makes a resumed session land back on its original key.
///
/// An explicit `x-session-id` header always wins if a client sends one.
fn fingerprint(headers: &HeaderMap, body: &[u8]) -> (String, String) {
    for h in ["x-session-id", "x-opencode-session", "x-conversation-id"] {
        if let Some(v) = headers.get(h).and_then(|v| v.to_str().ok()) {
            if !v.is_empty() {
                return (format!("sid:{v}"), v.to_string());
            }
        }
    }

    let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) else {
        return ("nosession".into(), String::new());
    };

    let mut text = String::new();
    if let Some(msgs) = v.get("messages").and_then(|m| m.as_array()) {
        for m in msgs {
            if m.get("role").and_then(|r| r.as_str()) == Some("user") {
                match m.get("content") {
                    Some(serde_json::Value::String(s)) => text = s.clone(),
                    Some(serde_json::Value::Array(parts)) => {
                        for p in parts {
                            if let Some(t) = p.get("text").and_then(|t| t.as_str()) {
                                text.push_str(t);
                            }
                        }
                    }
                    _ => {}
                }
                break;
            }
        }
    }
    if text.is_empty() {
        return ("nosession".into(), String::new());
    }
    let cut: String = text.chars().take(4000).collect();
    (short_hash(cut.as_bytes()), String::new())
}

/// FNV-1a 64 — plenty for grouping requests, and avoids a hashing dependency.
fn short_hash(data: &[u8]) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in data {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

fn model_of(body: &[u8]) -> String {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("model").and_then(|m| m.as_str()).map(str::to_string))
        .unwrap_or_default()
}

fn is_stream(body: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("stream").and_then(|s| s.as_bool()))
        .unwrap_or(false)
}

/// Pull `usage` out of a non-streaming reply so spend and tokens are exact.
fn parse_usage(body: &[u8]) -> (f64, u64, u64) {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) else {
        return (0.0, 0, 0);
    };
    let u = v.get("usage");
    let cost = u
        .and_then(|u| u.get("cost"))
        .and_then(|c| c.as_f64())
        .unwrap_or(0.0);
    let get = |names: &[&str]| -> u64 {
        for n in names {
            if let Some(x) = u.and_then(|u| u.get(*n)).and_then(|x| x.as_u64()) {
                return x;
            }
        }
        0
    };
    (
        cost,
        get(&["input_tokens", "prompt_tokens"]),
        get(&["output_tokens", "completion_tokens"]),
    )
}

// ── error surface ───────────────────────────────────────────────────────────

/// Build a terminal error that ANY client can parse.
///
/// Contains Anthropic's `{type:"error", error:{type,message}}` and OpenAI's
/// `{error:{message,type,code}}` shapes at once, plus a top-level `message`.
/// That redundancy is why the client can never render `<none>`.
///
/// Status is deliberately 400: non-retryable, so the client shows the message
/// immediately instead of starting its own backoff.
fn terminal_error(msg: &str, detail: Vec<String>) -> Response<Body> {
    let payload = serde_json::json!({
        "type": "error",
        "message": msg,
        "error": {
            "type": "tabi_gateway_error",
            "code": "gateway_exhausted",
            "message": msg,
            "param": serde_json::Value::Null,
            "detail": detail,
        }
    });
    let bytes = serde_json::to_vec_pretty(&payload).unwrap_or_else(|_| b"{}".to_vec());
    let mut r = Response::new(full(bytes));
    *r.status_mut() = StatusCode::BAD_REQUEST;
    r.headers_mut().insert(
        HeaderName::from_static("content-type"),
        HeaderValue::from_static("application/json"),
    );
    r
}

/// Headers we must not copy from the upstream response.
const DROP_RESP: &[&str] = &[
    "transfer-encoding",
    "content-encoding",
    "connection",
    "keep-alive",
    "content-length",
];

fn copy_response_headers(src: &HeaderMap, dst: &mut HeaderMap) {
    for (n, v) in src.iter() {
        if DROP_RESP.contains(&n.as_str().to_ascii_lowercase().as_str()) {
            continue;
        }
        dst.insert(n.clone(), v.clone());
    }
}

// ── the main handler ────────────────────────────────────────────────────────

/// Record a failure with full context for the Errors tab.
///
/// Every failure path funnels through here so the dashboard can always answer
/// "what went wrong, on which key, and what did the gateway do about it".
#[allow(clippy::too_many_arguments)]
fn log_error(
    app: &Arc<App>,
    class: &str,
    provider: &str,
    host: &str,
    key: &str,
    model: &str,
    status: u16,
    message: &str,
    action: &str,
    session: &str,
    round: u32,
    latency_ms: u64,
    remaining: Option<f64>,
    required: Option<f64>,
    extra: &AttemptMeta,
) {
    let msg: String = message.split_whitespace().collect::<Vec<_>>().join(" ");
    app.record_error(crate::state::ErrorRecord {
        t: config::now_secs(),
        class: class.to_string(),
        provider: provider.to_string(),
        host: host.to_string(),
        key: mask(key),
        model: model.to_string(),
        status,
        message: msg.chars().take(400).collect(),
        action: action.to_string(),
        session: session.to_string(),
        round,
        latency_ms,
        remaining,
        required,
        req_bytes: extra.req_bytes,
        streaming: extra.streaming,
        ttfb_ms: extra.ttfb_ms,
        budget_secs: extra.budget_secs,
        proxy: extra.proxy.clone(),
        head_timeout: extra.head_timeout,
    });
}

/// Per-attempt training metadata: everything the 200-failure analysis needs to
/// tell "slow wifi upload" apart from "slow provider" apart from "dead key".
#[derive(Clone, Default)]
struct AttemptMeta {
    req_bytes: u64,
    streaming: bool,
    ttfb_ms: Option<u64>,
    budget_secs: u64,
    proxy: String,
    head_timeout: bool,
}

/// If the request's `model` is a key in `map`, replace it with the mapped value.
/// Returns the (possibly modified) body bytes. On any parse error the original
/// bytes are returned untouched — a malformed body will fail upstream anyway,
/// and we would rather not be the reason a request never fires.
fn rewrite_model(body: &Bytes, map: &std::collections::HashMap<String, String>) -> Bytes {
    if map.is_empty() {
        return body.clone();
    }
    let Ok(mut v) = serde_json::from_slice::<serde_json::Value>(&body) else {
        return body.clone();
    };
    let Some(obj) = v.as_object_mut() else {
        return body.clone();
    };
    let Some(model) = obj.get("model").and_then(|m| m.as_str()) else {
        return body.clone();
    };
    let Some(mapped) = map.get(model) else {
        return body.clone();
    };
    obj.insert("model".to_string(), serde_json::Value::String(mapped.clone()));
    serde_json::to_vec(&v).map(Bytes::from).unwrap_or_else(|_| body.clone())
}

pub async fn handle_proxy(app: Arc<App>, req: Request<Incoming>) -> Response<Body> {
    let (parts, body) = req.into_parts();
    let route = parse_route(parts.uri.path(), &*app.providers.read().unwrap());

    // Buffer the request body once. Every retry replays these exact bytes, so a
    // retry can never send a partial or empty body.
    //
    // The read has its own generous timeout, SEPARATE from the provider budget:
    // a big context on slow wifi can legitimately take a while to arrive, and
    // killing the read early would misattribute the client's slow upload to the
    // provider. If declared content-length exceeds the cap we refuse immediately
    // instead of buffering megabytes into RAM.
    if let Some(cl) = parts
        .headers
        .get(hyper::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<usize>().ok())
    {
        if cl > config::MAX_BODY_BYTES {
            app.record_error(crate::state::ErrorRecord {
                t: config::now_secs(),
                class: "CLIENT".into(),
                provider: String::new(),
                host: String::new(),
                key: String::new(),
                model: model_of(&[]),
                status: 413,
                message: format!(
                    "request body {cl} bytes exceeds {max} byte cap",
                    max = config::MAX_BODY_BYTES
                ),
                action: "rejected-before-send".into(),
                session: String::new(),
                round: 0,
                latency_ms: 0,
                remaining: None,
                required: None,
                ..Default::default()
            });
            return terminal_error(
                &format!(
                    "request body too large ({cl} bytes, cap is {} bytes)",
                    config::MAX_BODY_BYTES
                ),
                vec![],
            );
        }
    }
    let body_bytes = match tokio::time::timeout(
        Duration::from_secs(config::CLIENT_BODY_TIMEOUT_SECS),
        body.collect(),
    )
    .await
    {
        Ok(Ok(c)) => c.to_bytes(),
        Ok(Err(e)) => {
            // The client connection broke mid-upload. Nothing reached any
            // provider, so no key burned, no failover, no retry — there is
            // nothing to retry WITH yet. Terminal, immediately.
            return terminal_error(
                &format!("request upload incomplete (client connection broke): {e}"),
                vec![],
            );
        }
        Err(_) => {
            return terminal_error(
                &format!(
                    "request upload timed out after {}s — the client never finished sending. \
                     Check the connection; no provider was contacted.",
                    config::CLIENT_BODY_TIMEOUT_SECS
                ),
                vec![],
            );
        }
    };
    // Truncation check: declared length but fewer bytes means a cut connection.
    // Sending that to a provider would bill a malformed request.
    if let Some(cl) = parts
        .headers
        .get(hyper::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<usize>().ok())
    {
        if body_bytes.len() < cl {
            return terminal_error(
                &format!(
                    "request upload truncated ({} of {} bytes arrived). Nothing was sent upstream.",
                    body_bytes.len(),
                    cl
                ),
                vec![],
            );
        }
    }

    // Resolve which conversation this is. The ladder is header -> first-message
    // hash -> message-overlap (survives client-side compaction) -> anonymous.
    let ident = crate::session::resolve(
        &parts.headers,
        &body_bytes,
        &app.sessions_snapshot(),
        config::now_secs(),
    );
    let fp = &ident.fp.clone();
    let label = &ident.label.clone();
    let (client_id, client_label) =
        crate::session::detect_client(&parts.headers, route.flavor == ApiFlavor::Anthropic);
    let api_name = match route.flavor {
        ApiFlavor::Anthropic => "anthropic",
        ApiFlavor::OpenAi => "openai",
        ApiFlavor::Passthrough => "",
    };
    app.note_session_identity(
        &fp,
        ident.via,
        ident.compacted,
        &ident.hashes,
        client_id,
        &client_label,
        api_name,
    );

    let model = model_of(&body_bytes);
    let streaming = is_stream(&body_bytes);
    let mut errors: Vec<String> = Vec::new();
    // Did this request only succeed because we rotated a key or changed provider?
    // Counted so the dashboard can show saves the user never had to see.
    let mut needed_save = false;
    let mut first_provider: Option<String> = None;
    // Client-provided API key (1-20 chars accepted). When present, bypasses the
    // file-based key pool entirely — the client brings their own key.
    let client_key = parts
        .headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .filter(|v| {
            let len = v.len();
            len >= 1 && len <= 20
        })
        .map(|v| v.to_string());
    // Training metadata for this request. Per-attempt fields (ttfb, budget,
    // head_timeout) are refreshed before each attempt below.
    let mut meta = AttemptMeta {
        req_bytes: body_bytes.len() as u64,
        streaming,
        ..Default::default()
    };
    // Providers whose head timeout this request may later prove premature. On
    // success each of them (except the winner) gets a wasted-timeout mark,
    // which is what calibrates budgets in the 200-failure analysis.
    let mut maybe_wasted: Vec<String> = Vec::new();
    // Wall-clock deadline for the whole request, retries included. Without this
    // the ladder could run ~2.5h while the client timed out at 600s, so the user
    // saw a hang instead of an error.
    let deadline = std::time::Instant::now() + Duration::from_secs(config::REQUEST_DEADLINE_SECS);
    app.note_client_request();

    // Register as in flight so the dashboard can show which sessions are
    // actively working right now, not merely "seen recently".
    let flight = app.inflight_begin(
        &fp,
        if label.is_empty() {
            &client_label
        } else {
            &label
        },
        &model,
        client_id,
        streaming,
        body_bytes.len() as u64,
    );
    // Guard so the entry is removed on EVERY exit path, including early returns
    // and panics — a leaked entry would show a phantom live session forever.
    struct FlightGuard(Arc<App>, u64);
    impl Drop for FlightGuard {
        fn drop(&mut self) {
            self.0.inflight_end(self.1);
        }
    }
    let _flight_guard = FlightGuard(app.clone(), flight);

    // Round 0 is the fast path. Later rounds back off gently. There is no hard
    // round cap while the failure is transient — patience is the whole point.
    let mut round: u32 = 0;
    // Set when the time left is too short to fit a viable head budget. Declared
    // outside the round loop so the loop head turns it into the terminal error
    // without first sleeping through a backoff we already know is pointless.
    let mut out_of_time = false;
    loop {
        round += 1;

        // Deadline check before doing any more work. `out_of_time` lands here
        // too: the deadline was checked only at round top, so a round that began
        // with time left could run ~185s past it (one real 1M request took 487s
        // against a 480s deadline) and spent its last attempts on 19s and 5s
        // budgets that could not possibly succeed — while still marking two more
        // healthy keys slow.
        if out_of_time || std::time::Instant::now() >= deadline {
            let mut tail: Vec<String> = errors.iter().rev().take(6).cloned().collect();
            tail.reverse();
            app.event(
                "error",
                format!(
                    "request gave up after {}s ({round} rounds)",
                    config::REQUEST_DEADLINE_SECS
                ),
            );
            app.note_client_outcome(first_provider.as_deref().unwrap_or(""), false, false);
            return terminal_error(
                &format!(
                    "tabi-gateway: gave up after {}s. Every provider was failing or every key was \
                     out of funds. This is returned deliberately before your client's own timeout \
                     so you get a readable error instead of a hang. Open http://127.0.0.1:{}/ for \
                     live status.",
                    config::REQUEST_DEADLINE_SECS,
                    config::port()
                ),
                tail,
            );
        }

        // Offline: wait for the network. No key is touched, no provider blamed,
        // nothing surfaces to the client. This replaces
        // "Unable to connect ... [retrying in 6s attempt #4]" with a silent wait
        // that resumes the moment DNS resolves.
        if app.is_offline() {
            app.note_offline_hold();
            let mut waited = 0u64;
            while app.is_offline() && waited < 900 {
                tokio::time::sleep(Duration::from_secs(2)).await;
                waited += 2;
                // Cheap liveness check: a free models probe on any provider.
                if waited.is_multiple_of(10) {
                    let prov = app.providers.read().unwrap().first().cloned();
                    if let Some(p) = prov {
                        let host = p.host.to_string();
                        let pool = app.pool(p.id);
                        if let Some(k) = pool.first() {
                            if let upstream::Attempt::Ok(_) =
                                upstream::probe_models(&host, k).await
                            {
                                app.set_offline(false);
                                break;
                            }
                        }
                    }
                }
            }
        }

        let session = app.session(&fp);
        let preferred = route
            .pinned
            .clone()
            .or_else(|| session.as_ref().map(|s| s.provider.clone()));
        // Model-aware scoring: latency + recent error rate + circuit breaker +
        // whether the provider even advertises this model.
        let order = app.provider_order_for(preferred.as_deref(), &model);
        if order.is_empty() {
            return terminal_error(
                "tabi-gateway: no providers configured (key files empty or unreadable)",
                errors,
            );
        }

        // Keys tried this round. Cleared per round so a key whose cooldown has
        // since expired becomes eligible again.
        let mut tried: Vec<String> = Vec::new();
        let mut transient = false;

        for provider_id in &order {
            let Some(provider) = app.provider(provider_id) else {
                continue;
            };

            // Out of time: the inner loop set this. Stop the whole ladder rather
            // than walking the remaining providers only to re-derive the same
            // "no time left" answer for each of them.
            if out_of_time {
                break;
            }

            // Circuit breaker: skip a provider that is currently failing hard,
            // UNLESS it is the only one left to try. This is what stops repeated
            // `Bad Gateway` from costing a round trip on every request.
            if app.breaker_open(provider_id) && order.len() > 1 {
                errors.push(format!("{provider_id}: circuit open (recent failures)"));
                transient = true;
                continue;
            }

            // Sticky: keep this session on its existing key while it is usable.
            let mut sticky = session
                .as_ref()
                .filter(|s| &s.provider == provider_id)
                .map(|s| s.key.clone())
                .filter(|k| !tried.contains(k));

            if first_provider.is_none() {
                first_provider = Some(provider_id.clone());
            }
            for _ in 0..config::MAX_KEY_ATTEMPTS {
                let key = match sticky.take() {
                    Some(k) => k,
                    None => {
                        // Client brought their own key — use it directly, skip
                        // the pool. Only for the first provider in the order.
                        if let Some(ref ck) = client_key {
                            if provider_id == &order[0] {
                                ck.clone()
                            } else {
                                errors.push(format!("{provider_id}: no funded key available"));
                                transient = true;
                                break;
                            }
                        } else if route.path == "/v1/models" {
                            // /v1/models is free — any key works, skip hold check.
                            let cands = app.candidates_for_probe(provider_id, &tried);
                            if let Some(k) = cands.first() {
                                k.clone()
                            } else {
                                errors.push(format!("{provider_id}: no key available"));
                                transient = true;
                                break;
                            }
                        } else {
                            // Effective hold for THIS model, learned from previous
                            // 403s rather than assumed. new-api's ratio mode scales
                            // the hold with prompt size, so a constant would
                            // under-estimate on long conversations and hand out a
                            // key that then 403s.
                            let hold = app.hold_for(provider_id, &model);
                            let cands = app.candidates_with_hold(provider_id, &fp, &tried, hold);
                            let Some(k) = pick_verified(&app, &provider, cands, hold).await else {
                                errors.push(format!("{provider_id}: no funded key available"));
                                transient = true;
                                break;
                            };
                            k
                        }
                    }
                };
                tried.push(key.clone());

                app.inflight_phase(
                    flight,
                    provider_id,
                    provider.host,
                    &key,
                    round,
                    &format!("awaiting {}", provider.label),
                );
                // RAII so the counter is released on EVERY exit path, including
                // the client hanging up mid-await (hyper drops the whole service
                // future, so no code after the await would run).
                struct KeyBusy(Arc<App>, String);
                impl Drop for KeyBusy {
                    fn drop(&mut self) {
                        self.0.inflight(&self.1, -1);
                    }
                }
                app.inflight(&key, 1);
                let _busy = KeyBusy(app.clone(), key.clone());
                // Streaming head budget adapts to the provider's measured TTFB
                // AND to this request's size: a fast provider on a small context
                // fails over in ~45s, while a 2.7MB / ~1M-token body gets the
                // ~60s+ it actually needs (up to the Cloudflare edge ceiling).
                // Passing req_bytes is load-bearing — without it every context
                // size got one &identical budget, and the large ones could never
                // succeed no matter how many keys we burned.
                // Non-streaming heads arrive only after full generation, so they
                // keep the generous fixed budget.
                let head_budget = if streaming {
                    Duration::from_secs(app.head_budget_secs(provider_id, body_bytes.len() as u64))
                } else {
                    Duration::from_secs(config::UPSTREAM_TIMEOUT_SECS)
                };
                // Never wait past the request deadline. If what is left cannot
                // fit a viable attempt, do NOT start one: a 5s budget on a body
                // needing 60s is not a retry, it is a guaranteed timeout that
                // also marks a healthy key slow. Stop and let the loop head
                // return the terminal error.
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                if remaining < head_budget.min(Duration::from_secs(config::head_min_secs())) {
                    errors.push(format!(
                        "{provider_id}: {}s left, need {}s — stopping instead of a doomed attempt",
                        remaining.as_secs(),
                        head_budget.as_secs()
                    ));
                    out_of_time = true;
                    break;
                }
                 let head_budget = head_budget.min(remaining);
                 meta.budget_secs = head_budget.as_secs();
                 meta.ttfb_ms = None;
                 meta.head_timeout = false;

                 // Rewrite the model name if this provider has a mapping for it.
                 // A client asking for `claude-opus-5` can land on a provider whose
                 // channel names it something else — the map is the bridge.
                 let body_bytes = rewrite_model(&body_bytes, &provider.model_map);

                 let attempt = upstream::send_head(
                    provider.host,
                    &route.path,
                    &parts.method,
                    &parts.headers,
                    body_bytes.clone(),
                    &key,
                    route.flavor,
                    head_budget,
                )
                .await;
                drop(_busy);

                let head = match attempt {
                    HeadAttempt::Ok(h) => {
                        meta.proxy = h.proxy.clone();
                        h
                    }
                    HeadAttempt::Err {
                        message,
                        latency_ms,
                        proxy,
                    } => {
                        meta.proxy = proxy;
                        let c = classify::classify_transport(&message);
                        errors.push(format!("{provider_id}/{}: {message}", mask(&key)));
                        match c.class {
                            ErrClass::Offline => {
                                app.set_offline(true);
                                log_error(
                                    &app,
                                    "OFFLINE",
                                    provider_id,
                                    provider.host,
                                    &key,
                                    &model,
                                    0,
                                    &message,
                                    "held-for-network",
                                    &fp,
                                    round,
                                    latency_ms,
                                    None,
                                    None,
                                    &meta,
                                );
                                transient = true;
                            }
                            ErrClass::Timeout => {
                                // Slow, not dead: the provider may still be
                                // digesting a huge context. Soft count only, so we
                                // try the next key WITHOUT tripping the breaker
                                // on a provider that is merely busy.
                                app.note_provider_timeout(provider_id);
                                meta.head_timeout = true;
                                meta.ttfb_ms = Some(latency_ms);
                                if !maybe_wasted.iter().any(|x| x == provider_id) {
                                    maybe_wasted.push(provider_id.clone());
                                }
                                log_error(
                                    &app,
                                    "TIMEOUT",
                                    provider_id,
                                    provider.host,
                                    &key,
                                    &model,
                                    0,
                                    &message,
                                    "next-key (breaker untouched)",
                                    &fp,
                                    round,
                                    latency_ms,
                                    None,
                                    None,
                                    &meta,
                                );
                                errors.push(format!(
                                    "{provider_id}/{}: slow ({})",
                                    mask(&key),
                                    message.chars().take(80).collect::<String>()
                                ));
                                transient = true;
                                continue;
                            }
                            _ => {
                                app.note_provider_fail(provider_id);
                                // Only a timeout message means head-timeout; resets/connection
                                // errors are deaths, not slowness.
                                meta.head_timeout = message.contains("timed out");
                                meta.ttfb_ms = Some(latency_ms);
                                log_error(
                                    &app,
                                    "TRANSPORT",
                                    provider_id,
                                    provider.host,
                                    &key,
                                    &model,
                                    0,
                                    &message,
                                    "failed-over",
                                    &fp,
                                    round,
                                    latency_ms,
                                    None,
                                    None,
                                    &meta,
                                );
                                app.record_request(
                                    &fp,
                                    &label,
                                    provider_id,
                                    &key,
                                    &model,
                                    false,
                                    latency_ms,
                                    body_bytes.len() as u64,
                                    0,
                                    0.0,
                                    0,
                                    0,
                                );
                                transient = true;
                            }
                        }
                        break; // try the sibling provider
                    }
                };

                app.set_offline(false);

                // ── SUCCESS: commit headers and stream. ──────────────────────
                // This is the only place headers are written, and it happens only
                // after a 2xx — which is why all the retrying above stays
                // invisible to the client.
                if (200..300).contains(&head.status) {
                    // head.latency_ms IS time-to-first-byte: send_head returns as
                    // soon as the response head arrives, before the body is read.
                    app.note_latency_split(provider_id, head.latency_ms, head.latency_ms);
                    app.note_client_outcome(provider_id, true, needed_save);

                    for w in maybe_wasted.iter().filter(|x| *x != provider_id) {
                        app.note_wasted_timeout(w);
                    }
                    maybe_wasted.clear();
                    if streaming {
                        app.inflight_phase(
                            flight,
                            provider_id,
                            provider.host,
                            "",
                            round,
                            "streaming response",
                        );
                        // Cannot read usage without consuming the stream, so
                        // record the turn now and let the sweep reconcile spend.
                        app.record_request(
                            &fp,
                            &label,
                            provider_id,
                            &key,
                            &model,
                            true,
                            head.latency_ms,
                            head.bytes_up,
                            0,
                            0.0,
                            0,
                            0,
                        );
                        let mut resp = Response::new(pipe(head.body));
                        *resp.status_mut() =
                            StatusCode::from_u16(head.status).unwrap_or(StatusCode::OK);
                        copy_response_headers(&head.headers, resp.headers_mut());
                        return resp;
                    }

                    // Non-streaming: collect so exact cost/tokens are recorded.
                    let collected = match head.body.collect().await {
                        Ok(c) => c.to_bytes(),
                        Err(e) => {
                            errors.push(format!("{provider_id}: body read failed: {e}"));
                            app.note_provider_fail(provider_id);
                            transient = true;
                            break;
                        }
                    };
                    let (cost, in_tok, out_tok) = parse_usage(&collected);
                    app.record_request(
                        &fp,
                        &label,
                        provider_id,
                        &key,
                        &model,
                        true,
                        head.latency_ms,
                        head.bytes_up,
                        collected.len() as u64,
                        cost,
                        in_tok,
                        out_tok,
                    );
                    let mut resp = Response::new(full(collected));
                    *resp.status_mut() =
                        StatusCode::from_u16(head.status).unwrap_or(StatusCode::OK);
                    copy_response_headers(&head.headers, resp.headers_mut());
                    return resp;
                }

                // ── FAILURE: small body, so collect and classify. ────────────
                let blob = upstream::headers_blob(&head.headers);
                let err_body = head
                    .body
                    .collect()
                    .await
                    .map(|c| c.to_bytes())
                    .unwrap_or_default();
                // The head arrived, so this latency IS time-to-first-byte.
                meta.ttfb_ms = Some(head.latency_ms);
                let text = String::from_utf8_lossy(&err_body).to_string();
                let c = classify::classify_http(head.status, &blob, &text);
                let brief: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
                let brief: String = brief.chars().take(150).collect();

                app.record_request(
                    &fp,
                    &label,
                    provider_id,
                    &key,
                    &model,
                    false,
                    head.latency_ms,
                    head.bytes_up,
                    err_body.len() as u64,
                    0.0,
                    0,
                    0,
                );

                match c.class {
                    ErrClass::Quota => {
                        // The 403 states both numbers: exact remaining balance
                        // AND the hold this model actually required. Record both
                        // so neither is ever guessed again.
                        needed_save = true;
                        app.learn_from_quota(&key, c.remaining);
                        if let Some(req) = c.required {
                            app.learn_hold(provider_id, &model, req);
                        }
                        app.mark_dead(&key, config::COOLDOWN_QUOTA_SECS, c.detail.clone());
                        app.note_key_rotation(provider_id);
                        log_error(
                            &app,
                            "QUOTA",
                            provider_id,
                            provider.host,
                            &key,
                            &model,
                            head.status,
                            &text,
                            "rotated-key",
                            &fp,
                            round,
                            head.latency_ms,
                            c.remaining,
                            c.required,
                            &meta,
                        );
                        errors.push(format!("{provider_id}/{}: {}", mask(&key), c.detail));
                        continue; // genuinely dead key -> next key, same provider
                    }
                    ErrClass::Auth => {
                        needed_save = true;
                        app.mark_dead(&key, config::COOLDOWN_AUTH_SECS, "invalid key");
                        app.note_key_rotation(provider_id);
                        log_error(
                            &app,
                            "AUTH",
                            provider_id,
                            provider.host,
                            &key,
                            &model,
                            head.status,
                            &text,
                            "retired-key",
                            &fp,
                            round,
                            head.latency_ms,
                            None,
                            None,
                            &meta,
                        );
                        errors.push(format!("{provider_id}/{}: invalid key", mask(&key)));
                        continue;
                    }
                    ErrClass::Rate => {
                        // Healthy key, just busy. Cooldown only.
                        needed_save = true;
                        let ttl = c.retry_after.unwrap_or(config::COOLDOWN_RATE_SECS);
                        app.mark_dead(&key, ttl, "rate limited");
                        log_error(
                            &app,
                            "RATE",
                            provider_id,
                            provider.host,
                            &key,
                            &model,
                            head.status,
                            &text,
                            &format!("cooled-down {ttl}s"),
                            &fp,
                            round,
                            head.latency_ms,
                            None,
                            None,
                            &meta,
                        );
                        errors.push(format!("{provider_id}/{}: 429", mask(&key)));
                        transient = true;
                        continue;
                    }
                    ErrClass::Timeout => {
                        // HTTP-level slowness: 524 (origin still working) or 408.
                        // Same soft treatment as a transport head-timeout — rotate
                        // the key, leave the breaker alone.
                        needed_save = true;
                        app.note_provider_timeout(provider_id);
                        meta.head_timeout = true;
                        meta.ttfb_ms = Some(head.latency_ms);
                        if !maybe_wasted.iter().any(|x| x == provider_id) {
                            maybe_wasted.push(provider_id.clone());
                        }
                        log_error(
                            &app,
                            "TIMEOUT",
                            provider_id,
                            provider.host,
                            &key,
                            &model,
                            head.status,
                            &text,
                            "next-key (breaker untouched)",
                            &fp,
                            round,
                            head.latency_ms,
                            None,
                            None,
                            &meta,
                        );
                        errors.push(format!(
                            "{provider_id}/{}: slow HTTP {} ({})",
                            mask(&key),
                            head.status,
                            text.chars().take(60).collect::<String>()
                        ));
                        transient = true;
                        continue;
                    }
                    ErrClass::Waf => {
                        // Our UA/IP, not the key. Leave the key alone.
                        needed_save = true;
                        app.note_provider_fail(provider_id);
                        app.note_failover_from(provider_id);
                        log_error(
                            &app,
                            "WAF",
                            provider_id,
                            provider.host,
                            &key,
                            &model,
                            head.status,
                            &text,
                            "failed-over (key preserved)",
                            &fp,
                            round,
                            head.latency_ms,
                            None,
                            None,
                            &meta,
                        );
                        errors.push(format!("{provider_id}: WAF {} {brief}", head.status));
                        transient = true;
                        break; // sibling provider
                    }
                    ErrClass::NoChannel => {
                        // The provider has no backend for this model. Every
                        // sibling key returns the &identical answer, so trying
                        // more of them is pure waste — it burns request budget
                        // and, on 2026-09-04, drew a Cloudflare 429 after ~57
                        // attempts in 8 minutes. Cool the PROVIDER and move to
                        // its sibling; keys are untouched and unscored.
                        needed_save = true;
                        app.note_provider_no_channel(provider_id);
                        app.note_failover_from(provider_id);
                        log_error(
                            &app,
                            "NOCHANNEL",
                            provider_id,
                            provider.host,
                            &key,
                            &model,
                            head.status,
                            &text,
                            "provider cooled (keys untouched)",
                            &fp,
                            round,
                            head.latency_ms,
                            None,
                            None,
                            &meta,
                        );
                        errors.push(format!(
                            "{provider_id}: no channel for {model} — provider cooled {}s",
                            config::no_channel_cooldown_secs()
                        ));
                        transient = true;
                        break; // sibling provider
                    }
                    ErrClass::Upstream => {
                        // This is the `Bad Gateway [retrying in 40s]` case:
                        // Cloudflare 502 in front of the provider. Fail over
                        // instead of surfacing it.
                        needed_save = true;
                        app.note_provider_fail(provider_id);
                        app.note_failover_from(provider_id);
                        log_error(
                            &app,
                            "UPSTREAM",
                            provider_id,
                            provider.host,
                            &key,
                            &model,
                            head.status,
                            &text,
                            "failed-over",
                            &fp,
                            round,
                            head.latency_ms,
                            None,
                            None,
                            &meta,
                        );
                        errors.push(format!("{provider_id}: {} {brief}", head.status));
                        transient = true;
                        break;
                    }
                    ErrClass::Offline => {
                        app.set_offline(true);
                        transient = true;
                        break;
                    }
                    ErrClass::Client => {
                        // A genuine request problem (bad model, malformed body).
                        // Retrying cannot help, so pass it through verbatim —
                        // the caller needs to see it.
                        let mut resp = Response::new(full(err_body));
                        *resp.status_mut() =
                            StatusCode::from_u16(head.status).unwrap_or(StatusCode::BAD_REQUEST);
                        resp.headers_mut().insert(
                            HeaderName::from_static("content-type"),
                            HeaderValue::from_static("application/json"),
                        );
                        return resp;
                    }
                }
            }
        }

        // Nothing served this round.
        //
        // While the failure is transient we keep going indefinitely: 1700+ keys
        // and two providers mean recovery is near-certain, and waiting silently
        // is strictly better than handing the client a retryable error.
        let hard_cap = if transient { 600 } else { 40 };
        if round >= hard_cap {
            let mut tail: Vec<String> = errors.iter().rev().take(8).cloned().collect();
            tail.reverse();
            app.event("error", format!("request failed after {round} rounds"));
            app.note_client_outcome(first_provider.as_deref().unwrap_or(""), false, false);
            return terminal_error(
                &format!(
                    "tabi-gateway: exhausted {round} retry rounds across {} providers. \
                     Every key was out of funds or every upstream refused. \
                     Open http://127.0.0.1:{}/ for live status.",
                    order.len(),
                    config::port()
                ),
                tail,
            );
        }

        // Backoff: quick at first, then settle at 15s. Deliberately capped low —
        // a long sleep is indistinguishable from a hang.
        //
        // Skipped entirely when we are out of time: sleeping before returning an
        // error we have already decided on just adds latency to a failure.
        if out_of_time {
            continue; // loop head returns the terminal error
        }
        app.inflight_phase(
            flight,
            "",
            "",
            "",
            round,
            if transient {
                "retrying (transient failure)"
            } else {
                "retrying (keys dry)"
            },
        );
        let wait = match round {
            1..=3 => 1,
            4..=8 => 3,
            9..=20 => 7,
            _ => 15,
        };
        // Do not sleep past the deadline: waking up only to give up wastes the
        // remaining budget that could have gone to one more attempt.
        let left = deadline
            .saturating_duration_since(std::time::Instant::now())
            .as_secs();
        if left == 0 {
            continue; // loop head returns the terminal error
        }
        let wait = wait.min(left);
        if round == 1 || round.is_multiple_of(10) {
            eprintln!(
                "[router] round {round} found nothing ({}) — retrying in {wait}s",
                if transient { "transient" } else { "keys dry" }
            );
        }
        tokio::time::sleep(Duration::from_secs(wait)).await;
    }
}

/// Choose a key, confirming its balance with a FREE probe when the data is stale.
///
/// This is what makes 403s rare instead of routine: we learn the balance from
/// `/v1/dashboard/billing/usage` (no tokens spent) before risking a paid call.
async fn pick_verified(
    app: &Arc<App>,
    provider: &config::Provider,
    candidates: Vec<String>,
    hold: f64,
) -> Option<String> {
    let now = config::now_secs();

    for key in candidates.iter().take(10) {
        let (usage, last_probe) = {
            let g = app.lock();
            let k = g.keys.get(key)?;
            (k.usage_cents, k.last_probe)
        };
        let stale = usage.is_none() || now.saturating_sub(last_probe) > config::USAGE_STALE_SECS;

        if !stale {
            return Some(key.clone());
        }

        match upstream::probe_usage(provider.host, key).await {
            upstream::Attempt::Ok(r) if r.status == 200 => {
                if let Some(cents) = upstream::parse_usage_cents(&r.body) {
                    app.set_usage(key, cents);
                    let ok = {
                        let g = app.lock();
                        g.keys
                            .get(key)
                            .map(|k| k.usable(config::now_secs(), hold))
                            .unwrap_or(false)
                    };
                    if ok {
                        return Some(key.clone());
                    }
                    continue; // measured as empty: skip without spending anything
                }
                app.mark_probed(key);
                return Some(key.clone());
            }
            upstream::Attempt::Ok(r) if r.status == 401 => {
                app.mark_dead(key, config::COOLDOWN_AUTH_SECS, "probe: invalid key");
                continue;
            }
            // Probe inconclusive (WAF, 5xx, timeout). Do not punish the key —
            // just use it and let the real request decide.
            _ => {
                app.mark_probed(key);
                return Some(key.clone());
            }
        }
    }
    candidates.into_iter().next()
}

/// Mask a key for logs and the dashboard.
///
/// Byte slicing (`&k[..10]`) panics when the boundary falls inside a multi-byte
/// character, and `POST /api/keys/verify` accepts any JSON string starting with
/// `sk-`. A payload like `{"key":"sk-😀😀😀😀…"}` therefore aborted the whole
/// gateway, because the release profile sets `panic = "abort"`: every in-flight
/// stream dies and unsaved state is lost. Char-based truncation cannot panic.
fn mask(k: &str) -> String {
    let n = k.chars().count();
    if n < 14 {
        return "sk-…".into();
    }
    let head: String = k.chars().take(10).collect();
    let tail: String = k.chars().skip(n - 4).collect();
    format!("{head}…{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provs() -> Vec<config::Provider> {
        config::providers()
    }

    #[test]
    fn bare_v1_is_auto_routed() {
        let r = parse_route("/v1/messages", &provs());
        assert!(r.pinned.is_none());
        assert_eq!(r.path, "/v1/messages");
        assert_eq!(r.flavor, ApiFlavor::Anthropic);
    }

    #[test]
    fn provider_prefix_pins_and_strips() {
        let r = parse_route("/tabi/v1/chat/completions", &provs());
        assert_eq!(r.pinned.as_deref(), Some("tabi"));
        assert_eq!(r.path, "/v1/chat/completions");
        assert_eq!(r.flavor, ApiFlavor::OpenAi);

        let r = parse_route("/gorouter/v1/messages", &provs());
        assert_eq!(r.pinned.as_deref(), Some("gorouter"));
        assert_eq!(r.path, "/v1/messages");
    }

    #[test]
    fn auto_prefix_is_stripped_without_pinning() {
        let r = parse_route("/auto/v1/models", &provs());
        assert!(r.pinned.is_none());
        assert_eq!(r.path, "/v1/models");
        assert_eq!(r.flavor, ApiFlavor::Passthrough);
    }

    #[test]
    fn session_id_header_wins_over_body() {
        let mut h = HeaderMap::new();
        h.insert("x-session-id", HeaderValue::from_static("ses_abc123"));
        let (fp, label) = fingerprint(&h, br#"{"messages":[{"role":"user","content":"hi"}]}"#);
        assert_eq!(fp, "sid:ses_abc123");
        assert_eq!(label, "ses_abc123");
    }

    #[test]
    fn same_first_user_message_yields_same_fingerprint() {
        // Resuming a session must land on the same key, so growing the transcript
        // must NOT change the fingerprint.
        let h = HeaderMap::new();
        let a = br#"{"messages":[{"role":"system","content":"SYS"},{"role":"user","content":"build me a proxy"}]}"#;
        let b = br#"{"messages":[{"role":"system","content":"SYS"},{"role":"user","content":"build me a proxy"},{"role":"assistant","content":"ok"},{"role":"user","content":"continue"}]}"#;
        assert_eq!(fingerprint(&h, a).0, fingerprint(&h, b).0);
    }

    #[test]
    fn identical_system_prompt_different_first_user_splits_sessions() {
        // All agents share the model AND system prompt, so the first user message
        // is the only thing that can separate them.
        let h = HeaderMap::new();
        let a = br#"{"messages":[{"role":"system","content":"SAME"},{"role":"user","content":"task one"}]}"#;
        let b = br#"{"messages":[{"role":"system","content":"SAME"},{"role":"user","content":"task two"}]}"#;
        assert_ne!(fingerprint(&h, a).0, fingerprint(&h, b).0);
    }

    #[test]
    fn handles_array_content_blocks() {
        let h = HeaderMap::new();
        let body =
            br#"{"messages":[{"role":"user","content":[{"type":"text","text":"hello world"}]}]}"#;
        let (fp, _) = fingerprint(&h, body);
        assert_ne!(fp, "nosession");
    }

    #[test]
    fn garbage_body_does_not_panic() {
        let h = HeaderMap::new();
        assert_eq!(fingerprint(&h, b"not json at all").0, "nosession");
        assert_eq!(model_of(b"not json"), "");
        assert!(!is_stream(b"not json"));
        assert_eq!(parse_usage(b"not json"), (0.0, 0, 0));
    }

    #[test]
    fn parses_both_usage_shapes() {
        let anthropic = br#"{"usage":{"cost":0.00123,"input_tokens":7189,"output_tokens":2}}"#;
        assert_eq!(parse_usage(anthropic), (0.00123, 7189, 2));
        let openai = br#"{"usage":{"cost":0.5,"prompt_tokens":100,"completion_tokens":25}}"#;
        assert_eq!(parse_usage(openai), (0.5, 100, 25));
    }

    #[test]
    fn detects_streaming_requests() {
        assert!(is_stream(br#"{"stream":true}"#));
        assert!(!is_stream(br#"{"stream":false}"#));
        assert!(!is_stream(br#"{}"#));
    }

    #[test]
    fn terminal_error_is_parsable_by_both_clients() {
        // The whole point: a client must always find a message, never `<none>`,
        // and 400 must not trigger client-side backoff.
        let r = terminal_error("everything is dry", vec!["a".into()]);
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
        assert!(!r.status().is_server_error());
        assert_ne!(r.status().as_u16(), 429);
    }

    // ── panic safety (CRITICAL: `panic = "abort"` kills the whole gateway) ───

    #[test]
    fn mask_never_panics_on_multibyte_input() {
        // POST /api/keys/verify accepts any JSON string beginning with `sk-`, so
        // this reached mask() and aborted the process — taking every in-flight
        // stream and all unsaved state with it.
        for k in [
            "sk-😀😀😀😀😀😀😀😀😀😀",
            "sk-日本語のキーですこれは長いです",
            "sk-\u{0301}\u{0301}\u{0301}\u{0301}\u{0301}\u{0301}\u{0301}\u{0301}\u{0301}\u{0301}\u{0301}",
            "sk-ababababab🔑🔑🔑🔑",
        ] {
            let m = mask(k);
            assert!(m.contains('…'), "expected masking for {k:?}, got {m:?}");
        }
    }

    #[test]
    fn mask_still_masks_real_keys_and_hides_the_middle() {
        let m = mask("sk-abcdefghij1234567890wxyz");
        assert!(m.starts_with("sk-abcdefg"));
        assert!(m.ends_with("wxyz"));
        assert!(!m.contains("hij12345"), "middle must be hidden: {m}");
    }

    #[test]
    fn mask_handles_short_and_empty_input() {
        assert_eq!(mask(""), "sk-…");
        assert_eq!(mask("sk-abc"), "sk-…");
        // Exactly at the boundary: 13 chars is short, 14 is masked.
        assert_eq!(mask("sk-1234567890"), "sk-…");
        assert!(mask("sk-12345678901").contains('…'));
    }
    #[test]
    fn masks_keys_in_logs() {
        let m = mask("sk-abcdefghij1234567890wxyz");
        assert!(m.starts_with("sk-abcdefg"));
        assert!(m.ends_with("wxyz"));
        assert!(!m.contains("hij12345"));
    }
}
