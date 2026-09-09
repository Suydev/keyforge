//! Error classification.
//!
//! This is the most safety-critical logic in the gateway. A misclassification
//! either burns a good key or keeps retrying a dead one.
//!
//! Guiding rule: **never blame the key unless the upstream said so explicitly.**
//! An earlier version keyword-matched /invalid|quota|balance/ over the whole
//! body, which would kill healthy keys on unrelated errors (e.g. a model
//! refusing a prompt containing the word "invalid"). Now QUOTA requires TWO
//! independent structural signals, and an unrecognised 403 is treated as a WAF
//! problem (our UA/IP), not a key problem.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ErrClass {
    /// Key is out of money. Upstream stated the exact remaining balance.
    Quota,
    /// The provider was too slow (head timeout, HTTP 524, 408) — but a 524
    /// means the origin is STILL WORKING, so this is soft: rotate the key,
    /// do not trip the breaker. Distinct from Upstream (dead) on purpose.
    Timeout,
    /// Key is invalid/revoked.
    Auth,
    /// Rate limited. Key is HEALTHY — only needs a cooldown.
    Rate,
    /// Cloudflare / bot-check / HTML error page. NOT the key's fault.
    Waf,
    /// 5xx, connection reset, DNS failure, timeout. Provider-side or network.
    Upstream,
    /// The provider has no working backend for this model — new-api answered
    /// normally with `no available channel` / `model_not_found`. Every key on
    /// this provider will get the identical answer, so rotating them is pure
    /// waste: on 2026-09-04 this cycled ~57 attempts of 282-540KB across both
    /// providers in 8 minutes and earned a Cloudflare 429 rate-limit strike.
    /// Distinct from Upstream so the router can cool the PROVIDER and leave
    /// every key untouched.
    NoChannel,
    /// No network at all (DNS resolution failed / unreachable). Special-cased
    /// so we can hold the request instead of surfacing a hard failure.
    Offline,
    /// Genuine client-side problem (bad model name, malformed body). Pass
    /// through verbatim — retrying cannot help.
    Client,
}

#[derive(Clone, Debug)]
pub struct Classified {
    pub class: ErrClass,
    /// Remaining balance in USD, when the upstream told us.
    pub remaining: Option<f64>,
    /// Required hold in USD, when the upstream told us.
    pub required: Option<f64>,
    /// Retry-After in seconds, when present.
    pub retry_after: Option<u64>,
    /// Short human-readable summary for logs and the dashboard.
    pub detail: String,
}

impl Classified {
    fn of(class: ErrClass, detail: impl Into<String>) -> Self {
        Self {
            class,
            remaining: None,
            required: None,
            retry_after: None,
            detail: detail.into(),
        }
    }
}

/// Extract dollar amounts from a quota message.
///
/// Upstream uses a fullwidth dollar sign (`＄`, U+FF04) in Chinese messages,
/// e.g.  `预扣费额度失败, 用户剩余额度: ＄0.495838, 需要预扣费额度: ＄0.800000`
/// We accept both `＄` and ASCII `$`.
///
/// **Negatives are required.** An account can go overdrawn, and new-api reports
/// it verbatim: `用户额度不足, 剩余额度: ＄-0.145652`. An earlier version of this
/// parser stopped at the `-`, produced no amounts, and the whole message then
/// failed the two-signal QUOTA test — so an overdrawn key was classified WAF and
/// left marked healthy, to be retried forever.
fn dollar_amounts(text: &str) -> Vec<f64> {
    let mut out = Vec::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '$' || chars[i] == '＄' {
            let mut j = i + 1;
            // tolerate whitespace between sign and digits
            while j < chars.len() && chars[j].is_whitespace() {
                j += 1;
            }
            // optional sign, ASCII or fullwidth minus
            let mut neg = false;
            if j < chars.len() && (chars[j] == '-' || chars[j] == '−' || chars[j] == '－') {
                neg = true;
                j += 1;
            } else if j < chars.len() && chars[j] == '+' {
                j += 1;
            }
            let start = j;
            let mut seen_dot = false;
            while j < chars.len() && (chars[j].is_ascii_digit() || (chars[j] == '.' && !seen_dot)) {
                if chars[j] == '.' {
                    seen_dot = true;
                }
                j += 1;
            }
            if j > start {
                let s: String = chars[start..j].iter().collect();
                if let Ok(v) = s.parse::<f64>() {
                    out.push(if neg { -v } else { v });
                }
            }
            i = j.max(i + 1);
        } else {
            i += 1;
        }
    }
    out
}

fn contains_any(hay: &str, needles: &[&str]) -> bool {
    needles.iter().any(|n| hay.contains(n))
}

/// Does this body say "I have no backend for that model"?
///
/// new-api emits this as a normal JSON error when every channel serving a model
/// is disabled or unreachable. It is a PROVIDER-level fact: the answer is
/// identical for all 873 tabi keys and all 887 gorouter keys, so key rotation
/// cannot help and only burns request budget and WAF goodwill.
///
/// The machine-readable `code` is matched first because prose is localised and
/// varies between new-api versions.
fn is_no_channel(low: &str) -> bool {
    contains_any(
        low,
        &[
            // Machine-readable and version-stable. Matched first because prose is
            // localised and rewritten between upstream releases.
            "model_not_found",
            "no_available_channel",
            "overloaded_error",
            // English prose, both observed live on 2026-09-04.
            //
            // "all nodes exhausted" is the same condition wearing different
            // words: the relay answered normally, on a host whose website was
            // up, to say that every backend it could reach was gone. Missing it
            // cost a real outage — the ladder cycled six keys per provider
            // against a backend that had nothing to serve, ~57 attempts of
            // 282-540KB in 8 minutes across both providers, and earned a CDN
            // rate-limit strike that turned their outage into ours as well.
            "no available channel",
            "no channel available",
            "all nodes exhausted",
            "nodes exhausted",
            // Chinese prose, same condition.
            "无可用渠道",
            "没有可用渠道",
            "当前分组负载已饱和",
        ],
    )
}

/// Classify a transport-level failure (no HTTP response was received).
pub fn classify_transport(err: &str) -> Classified {
    let low = err.to_ascii_lowercase();

    // No DNS / no route => the device is offline, not the provider's fault.
    //
    // The route-level errnos matter as much as the DNS ones: when Android wifi
    // drops, a connect to a proxy's raw IP fails with EHOSTUNREACH ("No route to
    // host") or ENETUNREACH, and no DNS is involved at all. Missing those made
    // an offline device look like a provider outage.
    if contains_any(
        &low,
        &[
            // DNS
            "dns error",
            "failed to lookup",
            "name or service not known",
            "temporary failure in name resolution",
            "nodename nor servname",
            "no such host",
            "enotfound",
            "eai_again",
            // routing / link (EHOSTUNREACH 113, ENETUNREACH 101, ENETDOWN 100)
            "network is unreachable",
            "network unreachable",
            "no route to host",
            "host is unreachable",
            "network is down",
            "network down",
            "os error 101",
            "os error 113",
            "os error 100",
            "unreachable",
        ],
    ) {
        return Classified::of(ErrClass::Offline, format!("offline: {err}"));
    }

    if contains_any(&low, &["timed out", "timeout", "deadline"]) {
        return Classified::of(ErrClass::Timeout, format!("slow: {err}"));
    }
    if contains_any(
        &low,
        &[
            "connection reset",
            "broken pipe",
            "connection refused",
            "eof",
            "closed",
        ],
    ) {
        return Classified::of(ErrClass::Upstream, format!("transport: {err}"));
    }
    // Unknown transport failure: treat as upstream so we fail over rather than
    // burning keys.
    Classified::of(ErrClass::Upstream, format!("transport: {err}"))
}

/// Classify an HTTP error response.
///
/// `headers_blob` should be a lowercase concatenation of header names/values so
/// we can spot Cloudflare markers like `cf-ray`.
pub fn classify_http(status: u16, headers_blob: &str, body: &str) -> Classified {
    let head: String = body.chars().take(1200).collect();
    let low = head.to_ascii_lowercase();
    let trimmed = low.trim_start();

    let looks_html = trimmed.starts_with("<!doctype") || trimmed.starts_with("<html");
    let cf_marker = headers_blob.contains("cf-ray")
        || headers_blob.contains("cloudflare")
        || contains_any(
            &low,
            &["cloudflare", "attention required", "cf-error", "ray id"],
        );

    // ── 401 / explicit invalid token ─────────────────────────────────────────
    // Checked before quota: an invalid key can never be a funding problem.
    if status == 401
        || contains_any(
            &low,
            // NOTE: "no permission" is deliberately NOT here. It usually means
            // the key lacks access to a *model*, not that the key is invalid —
            // retiring the key for 7 days over that is wrong. It falls through to
            // Client, which surfaces the real reason to the caller.
            &[
                "invalid token",
                "unauthorized",
                "unauthenticated",
                "invalid api key",
            ],
        )
    {
        return Classified::of(ErrClass::Auth, "invalid or revoked key");
    }

    // ── 429 rate limit ───────────────────────────────────────────────────────
    if status == 429 || contains_any(&low, &["too many requests", "rate limit", "rate_limit"]) {
        // Clamp hard. This value is upstream-controlled and is persisted as a
        // cooldown, so an unvalidated `Retry-After: 999999999999` would retire a
        // key for 31 years — and survive a restart. 15 minutes is well beyond any
        // legitimate rate-limit window.
        const MAX_RETRY_AFTER: u64 = 900;
        let retry_after = headers_blob
            .split('\n')
            // Match the header exactly, not as a substring: `x-retry-after: 5`
            // must not be mistaken for the real header.
            .find_map(|line| line.strip_prefix("retry-after:"))
            .and_then(|rest| {
                rest.trim_start()
                    .split(|c: char| !c.is_ascii_digit())
                    .next()
                    .and_then(|d| d.parse::<u64>().ok())
            })
            .map(|v| v.clamp(1, MAX_RETRY_AFTER));
        let mut c = Classified::of(ErrClass::Rate, "rate limited");
        c.retry_after = retry_after;
        return c;
    }

    // ── QUOTA: requires TWO independent signals ──────────────────────────────
    // (a) a funding phrase, AND (b) at least one parsable dollar amount. Both
    // simplified and traditional Chinese, plus English fallbacks for other
    // new-api builds.
    //
    // Two distinct upstream messages exist and BOTH must be caught:
    //   pre-deduction refused (balance below the hold):
    //     预扣费额度失败, 用户剩余额度: ＄0.081996, 需要预扣费额度: ＄0.800000
    //   account already overdrawn:
    //     用户额度不足, 剩余额度: ＄-0.145652
    // The second has no 预扣费 phrase at all, so requiring it would misclassify
    // an overdrawn account as WAF and keep the key marked healthy forever.
    //
    // The same build ALSO emits an all-English variant with a machine-readable
    // code, observed live from tabitoken.com:
    //   {"code":"pre_consume_token_quota_failed","message":"token quota is not
    //    enough, token remain quota: ＄0.780000, need quota: ＄0.800000"}
    // None of the Chinese phrases appear in it, so it used to fall through to the
    // JSON-403 branch and become ErrClass::Client — surfaced verbatim to the
    // caller, key never rotated, exact balance never learned. The `code` field is
    // matched first because it is machine-readable and cannot be prose.
    let pre_deduct = contains_any(
        body,
        &[
            "pre_consume_token_quota_failed",
            "预扣费",
            "預扣費",
            "pre-deduct",
            "pre-charge",
            "pre_consume",
            "reserve quota",
        ],
    );
    let insufficient = contains_any(
        body,
        &[
            "额度不足",
            "額度不足",
            "余额不足",
            "餘額不足",
            "insufficient balance",
            "insufficient quota",
            "insufficient_quota",
            "insufficient user quota",
            "quota is not enough",
        ],
    );
    let remaining_phrase = contains_any(
        body,
        &[
            "剩余额度",
            "剩餘額度",
            "用户剩余",
            "用戶剩餘",
            "remaining quota",
            "remaining balance",
            "remain quota",
        ],
    );
    let amounts = dollar_amounts(body);

    // Any funding phrase paired with a real amount is conclusive.
    if (pre_deduct || insufficient || remaining_phrase) && !amounts.is_empty() {
        let mut c = Classified::of(ErrClass::Quota, "out of balance");
        c.remaining = amounts.first().copied();
        // The required amount is only present in the pre-deduction variant.
        c.required = if pre_deduct {
            amounts.get(1).copied()
        } else {
            None
        };
        c.detail = match (c.remaining, c.required) {
            (Some(r), Some(n)) => format!("balance ${r:.4} < required ${n:.4}"),
            (Some(r), None) if r < 0.0 => format!("overdrawn ${r:.4}"),
            (Some(r), None) => format!("balance ${r:.4}"),
            _ => "out of balance".into(),
        };
        return c;
    }

    // A funding phrase with no amount is still conclusive when the status says
    // payment/forbidden — we just do not learn the exact figure.
    if (insufficient || pre_deduct) && (status == 402 || status == 403) {
        let mut c = Classified::of(ErrClass::Quota, "out of balance (amount not reported)");
        c.remaining = Some(0.0);
        return c;
    }

    // ── Provider-side failures ───────────────────────────────────────────────
    // 524 deserves special care: Cloudflare sends it when the origin is STILL
    // WORKING but too slow. Treating it like a 502 would switch providers away
    // from one that was about to answer.
    if status == 524 {
        return Classified::of(ErrClass::Timeout, "origin slow (Cloudflare 524)");
    }
    // A 503 carrying `no available channel` is NOT a transport failure: new-api
    // answered correctly and told us it has no backend for this model. The site
    // is up, the relay is up, the key is fine, the money is fine — there is
    // simply nothing upstream to route to. No key can change that answer.
    //
    // This must be classified before the generic 5xx arm, which would otherwise
    // make it Upstream and send the router through six keys per provider.
    if is_no_channel(&low) {
        return Classified::of(
            ErrClass::NoChannel,
            "provider has no channel for this model",
        );
    }
    if status >= 500 {
        return Classified::of(ErrClass::Upstream, format!("upstream {status}"));
    }

    // ── Retryable 4xx that must NOT reach the client ─────────────────────────
    // Constraint: the gateway never emits a retryable status. 408 (the server
    // gave up waiting) means slowness — soft. 425 (Too Early) carries no timing
    // information, so it stays hard.
    if status == 408 {
        return Classified::of(ErrClass::Timeout, "server timed out waiting");
    }
    if status == 425 {
        return Classified::of(ErrClass::Upstream, "retryable 425");
    }

    // ── WAF ──────────────────────────────────────────────────────────────────
    // A 403 is only a WAF block when there is POSITIVE evidence of one: an HTML
    // body, or a Cloudflare marker together with an HTML-ish body.
    //
    // Previously any 403 without quota markers became WAF, and because these
    // providers sit behind Cloudflare (so `cf-ray` is on *every* response), a
    // plain JSON 403 was read as WAF -> `transient=true` -> up to 600 retry
    // rounds. That made ErrClass::Client effectively unreachable and could hang a
    // request for hours on an error that would never succeed.
    let json_body = trimmed.starts_with('{') || trimmed.starts_with('[');
    if looks_html || (cf_marker && !json_body) {
        return Classified::of(
            ErrClass::Waf,
            if status == 403 {
                "cloudflare challenge"
            } else {
                "html error page"
            },
        );
    }

    // A JSON 403 with no funding phrase is a real authorization refusal —
    // permission, model access, or a disabled channel. Retrying cannot help, so
    // surface it rather than spinning.
    if status == 403 {
        let brief: String = body.split_whitespace().collect::<Vec<_>>().join(" ");
        return Classified::of(
            ErrClass::Client,
            format!("forbidden: {}", brief.chars().take(160).collect::<String>()),
        );
    }

    // ── Everything else is the caller's problem; pass it through ─────────────
    let brief: String = body.split_whitespace().collect::<Vec<_>>().join(" ");
    Classified::of(
        ErrClass::Client,
        format!(
            "client {status}: {}",
            brief.chars().take(160).collect::<String>()
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // The exact production error string observed from tabitoken.com.
    const REAL_QUOTA: &str = r#"{"error":{"type":"new_api_error","message":"预扣费额度失败, 用户剩余额度: ＄0.495838, 需要预扣费额度: ＄0.800000 (request id: 202608311318293874546308268d9d6HaxTQ02M)"},"type":"error"}"#;
    const REAL_AUTH: &str = r#"{"error":{"code":"","message":"Invalid token (request id: 202608311559191809455718268d9d6FuzhrQfg)","type":"new_api_error"}}"#;
    // The all-English pre-consume refusal, captured live from tabitoken.com on
    // 2026-09-04 while replaying a 2.7MB request. Same provider, same condition
    // as REAL_QUOTA, entirely different wording plus a machine-readable code.
    const REAL_QUOTA_EN: &str = r#"{"error":{"message":"token quota is not enough, token remain quota: ＄0.780000, need quota: ＄0.800000 (request id: 202609041320379753596318268d9d6qWKnSw7H)","type":"new_api_error","param":"","code":"pre_consume_token_quota_failed"}}"#;

    #[test]
    fn english_pre_consume_403_is_quota_not_client() {
        // Before this, none of the Chinese quota phrases matched, so the body
        // fell through to the JSON-403 branch and became ErrClass::Client:
        // surfaced verbatim to the caller, key never rotated, balance never
        // learned. Hiding exactly this is why the gateway exists.
        let c = classify_http(403, "cf-ray: abc", REAL_QUOTA_EN);
        assert_eq!(c.class, ErrClass::Quota, "detail was {}", c.detail);
    }

    #[test]
    fn english_pre_consume_learns_both_amounts() {
        // Both figures matter: `remaining` pins this key's exact balance, and
        // `required` teaches the real hold for this model — which is what stops
        // the next selection from picking a key that 403s the same way.
        let c = classify_http(403, "", REAL_QUOTA_EN);
        assert_eq!(c.class, ErrClass::Quota);
        let r = c.remaining.expect("remaining amount");
        let n = c.required.expect("required amount");
        assert!((r - 0.78).abs() < 1e-9, "remaining was {r}");
        assert!((n - 0.80).abs() < 1e-9, "required was {n}");
        assert!(r < n, "this is a refusal precisely because balance < hold");
    }

    #[test]
    fn the_machine_readable_code_alone_is_enough() {
        // `code` cannot be prose, so it is trusted even when the message is
        // localised into wording we have never seen.
        let body = r#"{"error":{"message":"<unknown localisation>","code":"pre_consume_token_quota_failed"}}"#;
        assert_eq!(classify_http(403, "", body).class, ErrClass::Quota);
    }

    #[test]
    fn parses_fullwidth_dollar_amounts() {
        let a = dollar_amounts(REAL_QUOTA);
        assert_eq!(a.len(), 2, "expected two amounts, got {a:?}");
        assert!((a[0] - 0.495838).abs() < 1e-9);
        assert!((a[1] - 0.800000).abs() < 1e-9);
    }

    #[test]
    fn real_quota_403_is_quota() {
        let c = classify_http(403, "", REAL_QUOTA);
        assert_eq!(c.class, ErrClass::Quota);
        assert!((c.remaining.unwrap() - 0.495838).abs() < 1e-9);
        assert!((c.required.unwrap() - 0.8).abs() < 1e-9);
    }

    #[test]
    fn real_invalid_token_is_auth() {
        assert_eq!(classify_http(401, "", REAL_AUTH).class, ErrClass::Auth);
    }

    #[test]
    fn cloudflare_html_403_is_waf_not_quota() {
        let body = "<!DOCTYPE html><html><head><title>Attention Required! | Cloudflare</title>";
        let c = classify_http(403, "cf-ray: abc123", body);
        assert_eq!(c.class, ErrClass::Waf, "must not burn a key on a WAF block");
    }

    #[test]
    fn unknown_json_403_is_client_not_waf() {
        // Behaviour deliberately changed. A JSON 403 with no funding phrase and no
        // HTML is a real authorization refusal: retrying cannot fix it, so it must
        // reach the caller instead of entering the retry ladder.
        let c = classify_http(403, "", r#"{"error":{"message":"forbidden"}}"#);
        assert_eq!(c.class, ErrClass::Client);
    }

    #[test]
    fn prose_containing_invalid_does_not_become_quota() {
        // A model refusing a prompt must never be read as a funding problem.
        let body =
            r#"{"error":{"message":"invalid request: the field 'temperature' must be a number"}}"#;
        let c = classify_http(400, "", body);
        assert!(matches!(c.class, ErrClass::Auth | ErrClass::Client));
        assert_ne!(c.class, ErrClass::Quota);
    }

    #[test]
    fn quota_words_with_no_amount_still_classifies_when_status_agrees() {
        // Deliberate change of behaviour. Previously an amount was mandatory, but
        // the live overdrawn message proved that too strict:
        //   用户额度不足, 剩余额度: ＄-0.145652
        // parsed to no amounts (the `-` stopped the scanner), failed the test, and
        // was classified WAF — leaving a dead key marked healthy and retried
        // forever. Now a funding phrase plus a 402/403 is conclusive on its own;
        // we simply do not learn the exact figure.
        let c = classify_http(403, "", "预扣费额度失败 剩余额度 unknown");
        assert_eq!(c.class, ErrClass::Quota);
        assert_eq!(c.remaining, Some(0.0), "unknown balance treated as empty");
    }

    #[test]
    fn quota_words_on_a_success_status_are_not_quota() {
        // The status has to agree. Prose mentioning quota in a 200 body must not
        // retire a key.
        let c = classify_http(200, "", "your remaining quota is displayed here");
        assert_ne!(c.class, ErrClass::Quota);
    }

    #[test]
    fn overdrawn_negative_balance_is_quota_and_parses_the_sign() {
        // The exact live message that exposed the bug.
        let body = r#"{"error":{"message":"用户额度不足, 剩余额度: ＄-0.145652 (request id: 202609021205268045334498268d9d6XQVHiYTR)","type":"new_api_error"}}"#;
        let c = classify_http(403, "", body);
        assert_eq!(c.class, ErrClass::Quota);
        let r = c.remaining.expect("must parse a negative amount");
        assert!((r - -0.145652).abs() < 1e-9, "got {r}");
        assert!(c.detail.contains("overdrawn"), "detail was {}", c.detail);
    }

    #[test]
    fn pre_deduction_refusal_reports_both_amounts() {
        // The other live variant: balance is positive but below the hold.
        let body = r#"{"error":{"message":"预扣费额度失败, 用户剩余额度: ＄0.081996, 需要预扣费额度: ＄0.800000","type":"new_api_error"}}"#;
        let c = classify_http(403, "", body);
        assert_eq!(c.class, ErrClass::Quota);
        assert!((c.remaining.unwrap() - 0.081996).abs() < 1e-9);
        assert!((c.required.unwrap() - 0.8).abs() < 1e-9);
    }

    // ── regressions from the recovered review ────────────────────────────────

    #[test]
    fn json_403_is_client_not_waf() {
        // FINDING 3: these providers sit behind Cloudflare, so `cf-ray` is on
        // EVERY response. Treating any 403 as WAF sent plain authorization
        // refusals into a 600-round retry ladder — hours of hanging on an error
        // that could never succeed.
        let body = r#"{"error":{"message":"no permission for model claude-opus-9","type":"new_api_error"}}"#;
        let c = classify_http(403, "cf-ray: abc123\n", body);
        assert_eq!(c.class, ErrClass::Client, "detail: {}", c.detail);
    }

    #[test]
    fn html_403_is_still_waf_even_without_cf_marker() {
        let c = classify_http(403, "", "<!DOCTYPE html><html><body>blocked</body></html>");
        assert_eq!(c.class, ErrClass::Waf);
    }

    #[test]
    fn cf_marker_with_json_body_does_not_become_waf() {
        // The exact combination that broke: Cloudflare header + JSON error body.
        let c = classify_http(
            400,
            "cf-ray: xyz\n",
            r#"{"error":{"message":"bad model name"}}"#,
        );
        assert_eq!(c.class, ErrClass::Client);
    }

    #[test]
    fn retryable_4xx_never_reaches_the_client() {
        // FINDING 21: 408 and 425 are retryable in most HTTP clients. 408 means
        // the server was slow (soft); 425 carries no timing info (hard).
        assert_eq!(
            classify_http(408, "", "request timeout").class,
            ErrClass::Timeout
        );
        assert_eq!(
            classify_http(425, "", "too early").class,
            ErrClass::Upstream
        );
    }

    #[test]
    fn retry_after_is_clamped_to_a_sane_maximum() {
        // FINDING 20: unclamped, this is persisted as a cooldown — a hostile
        // `Retry-After` would retire a key for decades, across restarts.
        let c = classify_http(429, "retry-after: 999999999999\n", "slow down");
        assert_eq!(
            c.retry_after,
            Some(900),
            "must clamp, got {:?}",
            c.retry_after
        );

        let c = classify_http(429, "retry-after: 30\n", "slow down");
        assert_eq!(c.retry_after, Some(30), "legitimate values pass through");
    }

    #[test]
    fn similar_header_names_do_not_match_retry_after() {
        // Substring matching would have picked up `x-retry-after`.
        let c = classify_http(429, "x-retry-after: 5\n", "slow down");
        assert_eq!(c.retry_after, None, "must match the header exactly");
    }

    #[test]
    fn retry_after_http_date_is_ignored_not_misparsed() {
        let c = classify_http(429, "retry-after: Wed, 21 Oct 2026 07:28:00 GMT\n", "x");
        assert_eq!(c.retry_after, None);
    }
    #[test]
    fn dns_failure_is_offline() {
        assert_eq!(
            classify_transport("dns error: failed to lookup address information").class,
            ErrClass::Offline
        );
        assert_eq!(
            classify_transport("getaddrinfo ENOTFOUND gorouter.app").class,
            ErrClass::Offline
        );
    }

    // ── offline vs provider-down (uptime chart correctness) ──────────────────

    #[test]
    fn route_level_errnos_are_offline_not_provider_down() {
        // When Android wifi drops, a connect to a proxy's raw IP fails at the
        // ROUTE level with no DNS involved. Missing these made an offline device
        // paint red "provider down" bars on the uptime chart.
        for msg in [
            "error connecting: No route to host (os error 113)",
            "tcp connect error: Network is unreachable (os error 101)",
            "connect failed: Network is down (os error 100)",
            "Host is unreachable",
        ] {
            assert_eq!(
                classify_transport(msg).class,
                ErrClass::Offline,
                "should be Offline: {msg}"
            );
        }
    }

    #[test]
    fn dns_failures_remain_offline() {
        for msg in [
            "dns error: failed to lookup address information",
            "getaddrinfo ENOTFOUND gorouter.app",
            "Temporary failure in name resolution",
            "EAI_AGAIN tabitoken.com",
        ] {
            assert_eq!(classify_transport(msg).class, ErrClass::Offline, "{msg}");
        }
    }

    #[test]
    fn connection_refused_is_not_offline() {
        // A refused connection means something answered — the host is reachable,
        // so this is a provider/port problem, not our uplink.
        assert_eq!(
            classify_transport("Connection refused (os error 111)").class,
            ErrClass::Upstream
        );
    }

    #[test]
    fn reset_and_eof_stay_upstream() {
        assert_eq!(
            classify_transport("connection reset by peer").class,
            ErrClass::Upstream
        );
        assert_eq!(
            classify_transport("unexpected EOF").class,
            ErrClass::Upstream
        );
    }
    #[test]
    fn no_channel_is_its_own_class_not_a_generic_5xx() {
        // Both of these are HTTP 503 carrying ordinary JSON from a relay that
        // answered correctly on a host whose website was up. They mean "I have no
        // backend for this model", which is a PROVIDER fact: every sibling key
        // returns the identical answer, so rotating keys cannot help.
        //
        // Classifying them as Upstream sent the router through six keys per
        // provider. Observed 2026-09-04: ~57 attempts of 282-540KB in 8 minutes
        // across both providers, which earned a CDN rate-limit strike and turned
        // an upstream outage into a self-inflicted one on top of it.
        let no_ch = r#"{"error":{"code":"model_not_found","message":"No available channel for model claude-opus-5-thinking"}}"#;
        assert_eq!(classify_http(503, "", no_ch).class, ErrClass::NoChannel);

        let exhausted = r#"{"error":{"message":"all nodes exhausted; retry later","type":"overloaded_error","code":null}}"#;
        assert_eq!(
            classify_http(503, "", exhausted).class,
            ErrClass::NoChannel,
            "'all nodes exhausted' is the same condition in different words"
        );
    }

    #[test]
    fn no_channel_matches_on_code_alone_and_in_any_locale() {
        // The machine-readable code must be sufficient on its own, because prose
        // is localised and gets rewritten between upstream releases.
        assert_eq!(
            classify_http(503, "", r#"{"error":{"code":"model_not_found"}}"#).class,
            ErrClass::NoChannel
        );
        assert_eq!(
            classify_http(503, "", r#"{"error":{"type":"overloaded_error"}}"#).class,
            ErrClass::NoChannel
        );
        // Chinese wording for the same condition.
        assert_eq!(
            classify_http(503, "", r#"{"error":{"message":"无可用渠道"}}"#).class,
            ErrClass::NoChannel
        );
    }

    #[test]
    fn a_plain_5xx_is_still_upstream() {
        // The guard on the other side: NoChannel must not swallow genuine
        // provider faults, or a real outage stops counting against the provider
        // and the breaker never trips.
        assert_eq!(
            classify_http(503, "", "service unavailable").class,
            ErrClass::Upstream
        );
        assert_eq!(
            classify_http(500, "", "internal error").class,
            ErrClass::Upstream
        );
        // A 5xx is a provider fault whatever the body looks like: the 5xx arm is
        // checked before the WAF arm, so an HTML error page from a CDN in front
        // of a broken origin still counts against the provider rather than being
        // written off as our own egress problem.
        assert_eq!(
            classify_http(502, "", "<html>bad gateway</html>").class,
            ErrClass::Upstream
        );
        // 524 stays Timeout: the origin is still working, just slow.
        assert_eq!(classify_http(524, "", "timeout").class, ErrClass::Timeout);
    }

    #[test]
    fn timeout_is_soft_not_offline() {
        // Deliberate change: a timeout means "slow", not "dead" and not
        // "offline". It must rotate the key WITHOUT tripping the breaker, so a
        // provider digesting a huge context is not switched away from mid-work.
        assert_eq!(
            classify_transport("request timed out").class,
            ErrClass::Timeout
        );
        assert_eq!(
            classify_transport("timed out waiting for response head after 120s").class,
            ErrClass::Timeout
        );
        // ...but genuine offline signals stay Offline:
        assert_eq!(
            classify_transport("getaddrinfo ENOTFOUND gorouter.app").class,
            ErrClass::Offline
        );
        assert_eq!(
            classify_transport("dns error: failed to lookup address information").class,
            ErrClass::Offline
        );
    }

    #[test]
    fn five_hundred_is_upstream() {
        assert_eq!(
            classify_http(503, "", "service unavailable").class,
            ErrClass::Upstream
        );
    }

    #[test]
    fn rate_limit_parses_retry_after() {
        let c = classify_http(429, "retry-after: 30\r\n", "too many requests");
        assert_eq!(c.class, ErrClass::Rate);
        assert_eq!(c.retry_after, Some(30));
    }
}
