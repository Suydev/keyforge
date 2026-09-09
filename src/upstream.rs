//! Outbound HTTPS: the only place the gateway talks to a provider.
//!
//! One shared connection-pooled client. Keeping connections alive matters a lot
//! for battery: a fresh TLS handshake wakes the radio and burns far more energy
//! than reusing an idle socket.

use crate::config::{self, ApiFlavor};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::header::{HeaderMap, HeaderName, HeaderValue};
use hyper::{Method, Request, Response};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use std::cell::RefCell;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

type HttpsClient = Client<hyper_rustls::HttpsConnector<crate::proxy::ProxyConnector>, Full<Bytes>>;

/// Shared outbound proxy pool.
///
/// Requests egress through a rotating proxy so the provider does not see 1745
/// keys arriving from one device IP — the pattern that produces the Cloudflare
/// 502/524 HTML interstitials in the error log.
pub fn proxy_pool() -> &'static std::sync::Arc<crate::proxy::ProxyPool> {
    static POOL: OnceLock<std::sync::Arc<crate::proxy::ProxyPool>> = OnceLock::new();
    POOL.get_or_init(|| std::sync::Arc::new(crate::proxy::ProxyPool::new()))
}

/// TLS client config, shared by the pooled client and by proxy vetting.
///
/// Installs the ring provider on first use; a second install is a no-op, so
/// calling this from either path is safe.
fn tls_config() -> rustls::ClientConfig {
    let _ = rustls::crypto::ring::default_provider().install_default();
    rustls::ClientConfig::builder()
        .with_root_certificates(rustls::RootCertStore {
            roots: webpki_roots_owned(),
        })
        .with_no_client_auth()
}

fn client() -> &'static HttpsClient {
    static CLIENT: OnceLock<HttpsClient> = OnceLock::new();
    CLIENT.get_or_init(|| {
        // Install the ring crypto provider once. Ignore the error: a second
        // call just means it is already installed.
        let _ = rustls::crypto::ring::default_provider().install_default();

        let tls = tls_config();

        // The proxy connector sits underneath TLS, so tunnelling is transparent
        // to everything above and the connection pool still applies.
        let https = hyper_rustls::HttpsConnectorBuilder::new()
            .with_tls_config(tls)
            .https_only()
            .enable_http1()
            .wrap_connector(crate::proxy::ProxyConnector::new(proxy_pool().clone()));

        Client::builder(TokioExecutor::new())
            // ── Rotation and connection reuse are mutually exclusive here ────
            //
            // hyper's pool keys by `(scheme, authority)` only — it has no idea
            // which proxy tunnelled a socket. Worse, `one_connection_for` checks
            // the pool BEFORE invoking the connector, so on a hit
            // `ProxyConnector::call` never runs, `pick()` is never consulted, and
            // `USED_EGRESS` is never set.
            //
            // Measured consequence: 37 of 40 stored error records carried
            // `proxy: ""` while `directFallbacks` read 0 — with the pool enabled
            // and non-empty, so a genuine direct fallback would have been
            // counted. Empty therefore meant "reused socket": roughly 92% of
            // requests rode a connection whose egress we could not name, and
            // rotation was largely illusory.
            //
            // Upstream rate limits are per-IP as well as per-account, and one
            // egress IP is what got ten separate accounts limited together
            // (~60 HTTP 429 in 20 minutes). Exact rotation is worth more than
            // 90 seconds of socket warmth: the cost is one TLS handshake per
            // request, ~4/min at observed volume.
            .pool_idle_timeout(Duration::from_secs(90))
            .pool_max_idle_per_host(0)
            .build(https)
    })
}

fn webpki_roots_owned() -> Vec<rustls::pki_types::TrustAnchor<'static>> {
    webpki_roots::TLS_SERVER_ROOTS.to_vec()
}

/// Outcome of one upstream attempt.
pub struct UpstreamReply {
    pub status: u16,
    pub headers: HeaderMap,
    pub body: Bytes,
    pub latency_ms: u64,
    /// Bytes actually sent/received (for the data-transfer dashboard).
    pub bytes_up: u64,
    pub bytes_down: u64,
}

pub enum Attempt {
    Ok(UpstreamReply),
    /// Transport failure — no HTTP response arrived.
    Err {
        message: String,
        latency_ms: u64,
    },
}

/// Response with headers received but body NOT yet read.
///
/// This split is what makes streaming work. We must know the status before
/// committing headers to the client (so a 502 can be retried invisibly), but we
/// must NOT buffer a successful SSE body (or the user sees nothing until the
/// stream ends). So: await the head, branch on status, then either stream the
/// body straight through or collect the (small) error body and retry.
pub struct HeadReply {
    pub status: u16,
    pub headers: HeaderMap,
    pub body: Incoming,
    pub latency_ms: u64,
    pub bytes_up: u64,
    /// Egress that served this attempt: proxy `addr`, or empty when direct or
    /// unknown. Captured INSIDE the task-local scope — see `send_head`.
    pub proxy: String,
}

pub enum HeadAttempt {
    Ok(HeadReply),
    Err {
        message: String,
        latency_ms: u64,
        /// Egress in force when this failed, so a systematically bad proxy can
        /// actually be blamed. Empty when direct or unknown.
        proxy: String,
    },
}

/// Headers we must never forward: hop-by-hop, or ones we set ourselves.
const STRIP: &[&str] = &[
    "host",
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "content-length",
    "authorization",
    "x-api-key",
    "user-agent",
    "accept-encoding",
];

/// Build the outbound request. Shared by the buffered and streaming paths.
fn build_request(
    host: &str,
    path: &str,
    method: &Method,
    client_headers: &HeaderMap,
    body: Bytes,
    key: &str,
    flavor: ApiFlavor,
) -> Result<Request<Full<Bytes>>, String> {
    let uri = format!("https://{host}{path}");
    let mut req = Request::builder().method(method.clone()).uri(&uri);
    {
        let h = req.headers_mut().expect("builder has headers");
        for (name, value) in client_headers.iter() {
            let n = name.as_str().to_ascii_lowercase();
            if STRIP.contains(&n.as_str()) {
                continue;
            }
            h.insert(name.clone(), value.clone());
        }
        h.insert(
            HeaderName::from_static("authorization"),
            HeaderValue::from_str(&format!("Bearer {key}"))
                .unwrap_or_else(|_| HeaderValue::from_static("")),
        );
        h.insert(
            HeaderName::from_static("x-api-key"),
            HeaderValue::from_str(key).unwrap_or_else(|_| HeaderValue::from_static("")),
        );
        h.insert(
            HeaderName::from_static("user-agent"),
            HeaderValue::from_static(config::BROWSER_UA),
        );
        // No compression: keeps SSE readable and makes byte accounting real.
        h.insert(
            HeaderName::from_static("accept-encoding"),
            HeaderValue::from_static("identity"),
        );
        if flavor == ApiFlavor::Anthropic && !h.contains_key("anthropic-version") {
            h.insert(
                HeaderName::from_static("anthropic-version"),
                HeaderValue::from_static("2023-06-01"),
            );
        }
        if !body.is_empty() && !h.contains_key("content-type") {
            h.insert(
                HeaderName::from_static("content-type"),
                HeaderValue::from_static("application/json"),
            );
        }
    }
    req.body(Full::new(body))
        .map_err(|e| format!("bad request: {e}"))
}

/// Flatten a hyper error plus its whole source chain.
///
/// Critical for offline detection: hyper's top-level message is a generic
/// "client error (Connect)" and the DNS detail only appears deeper in the chain.
fn flatten_err<E: std::error::Error + 'static>(e: E) -> String {
    let mut msg = e.to_string();
    let mut src: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(&e);
    while let Some(s) = src {
        msg.push_str(": ");
        msg.push_str(&s.to_string());
        src = std::error::Error::source(s);
    }
    msg
}

/// Vet one proxy by doing the only thing that actually matters: a real request
/// to a real provider through it.
///
/// A generic HTTPS check is not a substitute. Free proxies routinely pass
/// `api.ipify.org` and then fail here, because the origin sits behind Cloudflare
/// which blocks known proxy ranges, and because some proxies strip or mangle the
/// `Authorization` header.
///
/// Returns the connect latency in ms on success.
///
/// Uses a one-off client bound to this single proxy rather than the shared pool,
/// so vetting cannot pollute the pool's connection reuse or its health counters.
pub async fn vet_proxy(
    proxy: &crate::proxy::ProxyEndpoint,
    host: &str,
    key: &str,
) -> Result<u64, String> {
    let started = Instant::now();
    let pool = std::sync::Arc::new(crate::proxy::ProxyPool::new());
    pool.add_endpoints(vec![proxy.clone()]);
    pool.set_enabled(true);

    let tls = tls_config();
    let https = hyper_rustls::HttpsConnectorBuilder::new()
        .with_tls_config(tls)
        .https_only()
        .enable_http1()
        .wrap_connector(crate::proxy::ProxyConnector::new(pool));
    let client: HttpsClient = Client::builder(hyper_util::rt::TokioExecutor::new())
        // No idle reuse: this client exists for one request.
        .pool_max_idle_per_host(0)
        .build(https);

    let req = match build_request(
        host,
        "/v1/models",
        &Method::GET,
        &HeaderMap::new(),
        Bytes::new(),
        key,
        ApiFlavor::OpenAi,
    ) {
        Ok(r) => r,
        Err(e) => return Err(e),
    };

    let timeout = Duration::from_secs(config::PROXY_VET_TIMEOUT_SECS);
    match tokio::time::timeout(timeout, client.request(req)).await {
        Ok(Ok(resp)) => {
            let status = resp.status().as_u16();
            if (200..300).contains(&status) {
                Ok(started.elapsed().as_millis() as u64)
            } else {
                // A 403 here is usually Cloudflare refusing the proxy's IP, not a
                // bad key — the same key works direct and through other proxies.
                Err(format!("HTTP {status}"))
            }
        }
        Ok(Err(e)) => Err(flatten_err(e)),
        Err(_) => Err(format!("timeout after {}s", config::PROXY_VET_TIMEOUT_SECS)),
    }
}

/// Send a request and return as soon as the response HEAD is available, leaving
/// the body unread.
///
/// This is the streaming path and the default for proxying. Awaiting only the
/// head lets us branch on status before committing anything to the client: an
/// error body is small and gets collected for retry, while a success body is
/// piped through untouched so SSE tokens arrive live.
// Eight parameters, one over clippy's default. They are the irreducible inputs
// to one HTTP call: where, what verb, what headers, what body, which key, which
// wire format, how long to wait. A parameter struct would add a type and a
// construction site without removing a single decision from the caller.
#[allow(clippy::too_many_arguments)]
pub async fn send_head(
    host: &str,
    path: &str,
    method: &Method,
    client_headers: &HeaderMap,
    body: Bytes,
    key: &str,
    flavor: ApiFlavor,
    head_timeout: Duration,
) -> HeadAttempt {
    let started = Instant::now();
    let bytes_up = body.len() as u64;

    let req = match build_request(host, path, method, client_headers, body, key, flavor) {
        Ok(r) => r,
        Err(message) => {
            // Nothing was sent, so no egress was chosen.
            return HeadAttempt::Err {
                message,
                latency_ms: started.elapsed().as_millis() as u64,
                proxy: String::new(),
            };
        }
    };

    // Timeout applies to obtaining the HEAD only. Once streaming begins there is
    // deliberately no overall deadline — a reasoning model can legitimately think
    // for minutes, and killing it mid-stream is exactly the truncation bug we are
    // avoiding.
    //
    // The egress address is read INSIDE the task-local scope, before it is torn
    // down, and returned alongside the result. Reading it from the caller after
    // the scope had already ended is why every one of 137 stored error records
    // carried `proxy: ""` — per-proxy blame was impossible even though the
    // proxies were demonstrably in use (direct access 403s at the WAF).
    let scoped = crate::proxy::USED_EGRESS.scope(RefCell::new(None), async move {
        let r = tokio::time::timeout(head_timeout, client().request(req)).await;
        (r, crate::proxy::used_egress().unwrap_or_default())
    });
    let (outcome, proxy) = scoped.await;
    let resp = match outcome {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            return HeadAttempt::Err {
                message: flatten_err(e),
                latency_ms: started.elapsed().as_millis() as u64,
                proxy,
            }
        }
        Err(_) => {
            return HeadAttempt::Err {
                message: format!(
                    "timed out waiting for response head after {}s",
                    head_timeout.as_secs()
                ),
                latency_ms: started.elapsed().as_millis() as u64,
                proxy,
            }
        }
    };

    let (parts, body) = resp.into_parts();
    HeadAttempt::Ok(HeadReply {
        status: parts.status.as_u16(),
        headers: parts.headers,
        body,
        latency_ms: started.elapsed().as_millis() as u64,
        bytes_up,
        proxy,
    })
}

/// Send one request to a provider with a specific key, buffering the whole body.
///
/// Used for the free probes (`/v1/models`, billing) where the body is tiny and
/// we want it in one piece. Proxying uses [`send_head`] instead.
// See the note on `send_head`: same argument list, same reasoning.
#[allow(clippy::too_many_arguments)]
pub async fn send(
    host: &str,
    path: &str,
    method: &Method,
    client_headers: &HeaderMap,
    body: Bytes,
    key: &str,
    flavor: ApiFlavor,
    timeout: Duration,
) -> Attempt {
    let started = Instant::now();
    let bytes_up = body.len() as u64;

    let req = match build_request(host, path, method, client_headers, body, key, flavor) {
        Ok(r) => r,
        Err(message) => {
            return Attempt::Err {
                message,
                latency_ms: started.elapsed().as_millis() as u64,
            }
        }
    };

    let scoped = crate::proxy::USED_EGRESS.scope(RefCell::new(None), client().request(req));
    let resp: Response<Incoming> = match tokio::time::timeout(timeout, scoped).await {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            return Attempt::Err {
                message: flatten_err(e),
                latency_ms: started.elapsed().as_millis() as u64,
            }
        }
        Err(_) => {
            return Attempt::Err {
                message: format!("timed out after {}s", timeout.as_secs()),
                latency_ms: started.elapsed().as_millis() as u64,
            }
        }
    };

    let status = resp.status().as_u16();
    let headers = resp.headers().clone();
    let collected = match tokio::time::timeout(timeout, resp.into_body().collect()).await {
        Ok(Ok(c)) => c.to_bytes(),
        Ok(Err(e)) => {
            return Attempt::Err {
                message: format!("body read failed: {e}"),
                latency_ms: started.elapsed().as_millis() as u64,
            }
        }
        Err(_) => {
            return Attempt::Err {
                message: "timed out reading body".into(),
                latency_ms: started.elapsed().as_millis() as u64,
            }
        }
    };

    Attempt::Ok(UpstreamReply {
        status,
        bytes_down: collected.len() as u64,
        body: collected,
        headers,
        latency_ms: started.elapsed().as_millis() as u64,
        bytes_up,
    })
}

/// Flatten headers into one lowercase blob for cheap marker matching
/// (`cf-ray`, `retry-after`) in the classifier.
pub fn headers_blob(h: &HeaderMap) -> String {
    let mut s = String::with_capacity(256);
    for (n, v) in h.iter() {
        s.push_str(&n.as_str().to_ascii_lowercase());
        s.push(':');
        s.push_str(v.to_str().unwrap_or(""));
        s.push('\n');
    }
    s.to_ascii_lowercase()
}

/// Free `GET /v1/models`. Used for uptime probing and key verification.
/// Spends no credits — verified on all three hosts.
pub async fn probe_models(host: &str, key: &str) -> Attempt {
    send(
        host,
        "/v1/models",
        &Method::GET,
        &HeaderMap::new(),
        Bytes::new(),
        key,
        ApiFlavor::Passthrough,
        Duration::from_secs(config::PROBE_TIMEOUT_SECS),
    )
    .await
}

/// Detect which API flavors a provider supports. Probes both the OpenAI
/// (`/v1/chat/completions`) and Anthropic (`/v1/messages`) endpoints with a
/// minimal request and reports which ones answer without an auth/404 error.
/// Returns `(openai_ok, anthropic_ok)`.
pub async fn probe_capabilities(host: &str, key: &str) -> (bool, bool) {
    let host = host.to_string();
    let key = key.to_string();
    let mut results = (false, false);
    for (path, flavor) in [
        ("/v1/chat/completions", ApiFlavor::OpenAi),
        ("/v1/messages", ApiFlavor::Anthropic),
    ] {
        // Minimal probe body — model name is a placeholder, we only care if the
        // endpoint answers (not 404/401/403). A real request carries the actual model.
        let body = match flavor {
            ApiFlavor::Anthropic => br#"{"model":"x","max_tokens":1,"messages":[{"role":"user","content":"x"}]}"#.to_vec(),
            _ => br#"{"model":"x","max_tokens":1,"messages":[{"role":"user","content":"x"}]}"#.to_vec(),
        };
        let ok = match send(
            &host,
            path,
            &Method::POST,
            &HeaderMap::new(),
            Bytes::from(body),
            &key,
            flavor,
            Duration::from_secs(config::PROBE_TIMEOUT_SECS),
        )
        .await
        {
            Attempt::Ok(r) => r.status != 404 && r.status != 401 && r.status != 403,
            Attempt::Err { .. } => false,
        };
        if flavor == ApiFlavor::OpenAi {
            results.0 = ok;
        } else {
            results.1 = ok;
        }
    }
    results
}

/// Free `GET /v1/dashboard/billing/usage` -> `{"total_usage": <cents spent>}`.
///
/// This is what makes balance knowable *before* sending a paid request.
/// Verified: a key reporting `12640` was the same key that 403'd with
/// `$0.495838` remaining, so initial was $126.90 — inside the observed range.
pub async fn probe_usage(host: &str, key: &str) -> Attempt {
    send(
        host,
        "/v1/dashboard/billing/usage",
        &Method::GET,
        &HeaderMap::new(),
        Bytes::new(),
        key,
        ApiFlavor::Passthrough,
        Duration::from_secs(config::PROBE_TIMEOUT_SECS),
    )
    .await
}

/// Parse `total_usage` (cents) out of a billing response.
pub fn parse_usage_cents(body: &[u8]) -> Option<f64> {
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    v.get("total_usage").and_then(|x| x.as_f64())
}

/// Parse model ids out of a `/v1/models` response.
pub fn parse_model_ids(body: &[u8]) -> Vec<String> {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) else {
        return Vec::new();
    };
    v.get("data")
        .and_then(|d| d.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|m| m.get("id").and_then(|i| i.as_str()).map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}
