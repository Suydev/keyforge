//! Session identity — surviving compaction.
//!
//! # The problem
//! A session must map to a stable key: switching keys mid-conversation means a
//! different account wallet, lost prompt-cache locality, and spend attribution
//! that splits into two unrelated rows.
//!
//! Identifying a session is not obvious, because:
//!  * OpenCode sends no session header by default;
//!  * every agent here uses the SAME model and the SAME system prompt, so the
//!    system block carries no identifying information;
//!  * the full history is resent every turn, so the array grows constantly.
//!
//! Hashing the first user message handles all of that — it is unique per
//! conversation and stable as the transcript grows.
//!
//! # Why compaction breaks that
//! When a client compacts, it **replaces the head of the history with a summary
//! and keeps the tail**. The original first user message is gone, so its hash
//! changes and the conversation would look brand new.
//!
//! # The fix: a resolution ladder
//! 1. `x-session-id` (or similar) header — exact, if a client provides one.
//! 2. First-user-message hash — the fast path, correct pre-compaction.
//! 3. **Message-overlap matching** — compaction preserves the tail, so the
//!    surviving messages still hash identically. Finding a session that shares
//!    enough of them re-identifies it and records a compaction.
//! 4. Otherwise: genuinely new.
//!
//! System messages are deliberately excluded from the overlap set. They are
//! identical across every agent, so including them would make unrelated sessions
//! look similar.

use crate::state::SessionState;
use hyper::header::HeaderMap;
use std::collections::HashMap;

/// Headers a client may use to identify a conversation explicitly.
const SESSION_HEADERS: &[&str] = &[
    "x-session-id",
    "x-opencode-session",
    "x-conversation-id",
    "x-claude-session-id",
];

/// Matched messages required before two requests are treated as one session.
/// Two is enough to be decisive (a single shared "continue" is not), while
/// staying tolerant of aggressive compaction that keeps only a short tail.
const MIN_OVERLAP: usize = 2;

/// Most recent message hashes retained per session. Bounded so a long-running
/// conversation cannot grow state without limit.
pub const MAX_TRACKED_HASHES: usize = 32;

/// Identify which tool made the request, so the dashboard can show whether a
/// session came from Claude Code, OpenCode, or something added later.
///
/// Detection is layered because clients differ in how much they announce:
///   1. an explicit `x-client-name` header, if a tool sets one;
///   2. a recognisable `user-agent` substring;
///   3. protocol shape — `anthropic-version` means the Messages API, which in
///      practice means Claude Code or a direct Anthropic SDK;
///   4. otherwise "unknown", never a guess dressed up as fact.
///
/// Returns a stable short id plus a display label.
pub fn detect_client(headers: &HeaderMap, flavor_is_anthropic: bool) -> (&'static str, String) {
    let get = |n: &str| -> String {
        headers
            .get(n)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase()
    };

    // 1. explicit declaration
    let declared = get("x-client-name");
    if !declared.is_empty() {
        // Normalise the ones we know, otherwise pass the tool's own name through.
        if declared.contains("claude") {
            return ("claude-code", "Claude Code".into());
        }
        if declared.contains("opencode") {
            return ("opencode", "OpenCode".into());
        }
        return ("other", declared);
    }

    // 2. user-agent sniffing
    let ua = get("user-agent");
    for (needle, id, label) in [
        ("claude-cli", "claude-code", "Claude Code"),
        ("claude-code", "claude-code", "Claude Code"),
        ("anthropic", "anthropic-sdk", "Anthropic SDK"),
        ("opencode", "opencode", "OpenCode"),
        ("cline", "cline", "Cline"),
        ("roo", "roo-code", "Roo Code"),
        ("kilo", "kilo-code", "Kilo Code"),
        ("cursor", "cursor", "Cursor"),
        ("continue", "continue", "Continue"),
        ("aider", "aider", "Aider"),
        ("zed", "zed", "Zed"),
        ("openai-node", "openai-sdk", "OpenAI SDK"),
        ("openai-python", "openai-sdk", "OpenAI SDK"),
        ("node", "node-client", "Node client"),
        ("curl", "curl", "curl"),
    ] {
        if ua.contains(needle) {
            return (id, label.to_string());
        }
    }

    // 3. protocol shape. Only Anthropic-style clients send this header.
    if !get("anthropic-version").is_empty() || flavor_is_anthropic {
        return ("anthropic-api", "Anthropic API".into());
    }

    ("unknown", "unknown".into())
}

#[derive(Debug, Clone)]
pub struct Resolved {
    /// Stable session fingerprint.
    pub fp: String,
    /// Human label, when the client supplied one.
    pub label: String,
    /// True when this request was re-identified by overlap after its first user
    /// message changed — i.e. the client compacted the conversation.
    pub compacted: bool,
    /// Message hashes for this request, to be stored on the session.
    pub hashes: Vec<u64>,
    /// How the session was identified (shown in the dashboard).
    pub via: &'static str,
}

/// FNV-1a 64. Fast, allocation-free, and stable across runs — which matters
/// because these hashes are persisted.
fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in data {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

fn hex(h: u64) -> String {
    format!("{h:016x}")
}

/// Flatten a message's `content` into text.
///
/// Handles both shapes: a bare string, and an array of typed parts (Anthropic
/// content blocks, tool results, images).
fn content_text(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(parts) => {
            let mut out = String::new();
            for p in parts {
                if let Some(t) = p.get("text").and_then(|t| t.as_str()) {
                    out.push_str(t);
                } else if let Some(c) = p.get("content") {
                    // tool_result blocks nest their payload
                    out.push_str(&content_text(c));
                } else if let Some(id) = p.get("id").and_then(|i| i.as_str()) {
                    // tool_use blocks: the id is stable and identifying
                    out.push_str(id);
                }
            }
            out
        }
        other => other.to_string(),
    }
}

/// Per-message hashes for overlap matching.
///
/// Skips system messages (identical across agents) and anything too short to be
/// distinctive, so a shared `"ok"` cannot create a false match.
fn message_hashes(body: &[u8]) -> Vec<u64> {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) else {
        return Vec::new();
    };
    let mut out = Vec::new();

    // Anthropic puts the system prompt in a separate top-level field; OpenAI puts
    // it in `messages` with role=system. Either way we ignore it.
    if let Some(msgs) = v.get("messages").and_then(|m| m.as_array()) {
        for m in msgs {
            let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("");
            if role == "system" {
                continue;
            }
            let Some(c) = m.get("content") else { continue };
            let text = content_text(c);
            let trimmed = text.trim();
            if trimmed.len() < 24 {
                continue; // too generic to identify anything
            }
            // Cap the hashed span: identical prefixes are enough, and this keeps
            // hashing cost flat for very large tool outputs.
            let span: String = trimmed.chars().take(2000).collect();
            out.push(fnv1a(format!("{role}\u{1}{span}").as_bytes()));
        }
    }

    if out.len() > MAX_TRACKED_HASHES {
        out = out.split_off(out.len() - MAX_TRACKED_HASHES);
    }
    out
}

/// Hash of the first user message — the pre-compaction identity.
fn first_user_hash(body: &[u8]) -> Option<u64> {
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    let msgs = v.get("messages")?.as_array()?;
    for m in msgs {
        if m.get("role").and_then(|r| r.as_str()) == Some("user") {
            let text = content_text(m.get("content")?);
            if text.trim().is_empty() {
                return None;
            }
            let span: String = text.chars().take(4000).collect();
            return Some(fnv1a(span.as_bytes()));
        }
    }
    None
}

/// Resolve which session a request belongs to.
pub fn resolve(
    headers: &HeaderMap,
    body: &[u8],
    sessions: &HashMap<String, SessionState>,
    now: u64,
) -> Resolved {
    let hashes = message_hashes(body);

    // ── 1. explicit header ──────────────────────────────────────────────────
    for name in SESSION_HEADERS {
        if let Some(v) = headers.get(*name).and_then(|v| v.to_str().ok()) {
            let v = v.trim();
            if !v.is_empty() {
                return Resolved {
                    fp: format!("sid:{v}"),
                    label: v.to_string(),
                    compacted: false,
                    hashes,
                    via: "header",
                };
            }
        }
    }

    // ── 2. first user message ───────────────────────────────────────────────
    let first = first_user_hash(body);
    if let Some(h) = first {
        let fp = hex(h);
        if sessions.contains_key(&fp) {
            return Resolved {
                fp,
                label: String::new(),
                compacted: false,
                hashes,
                via: "first-message",
            };
        }

        // ── 3. overlap: did a known session just get compacted? ─────────────
        //
        // Only worth checking when the request carries enough history to be
        // recognisable. A genuinely new conversation has almost none.
        if hashes.len() >= MIN_OVERLAP {
            if let Some((best_fp, matched)) = best_overlap(&hashes, sessions, now) {
                if matched >= MIN_OVERLAP {
                    return Resolved {
                        fp: best_fp,
                        label: String::new(),
                        compacted: true,
                        hashes,
                        via: "overlap-after-compaction",
                    };
                }
            }
        }

        // ── 4. new session ──────────────────────────────────────────────────
        return Resolved {
            fp,
            label: String::new(),
            compacted: false,
            hashes,
            via: "new",
        };
    }

    // No usable first message (e.g. a bare completion call). Try overlap, then
    // fall back to a shared bucket rather than inventing an identity per request.
    if hashes.len() >= MIN_OVERLAP {
        if let Some((best_fp, matched)) = best_overlap(&hashes, sessions, now) {
            if matched >= MIN_OVERLAP {
                return Resolved {
                    fp: best_fp,
                    label: String::new(),
                    compacted: false,
                    hashes,
                    via: "overlap",
                };
            }
        }
    }

    Resolved {
        fp: "nosession".into(),
        label: String::new(),
        compacted: false,
        hashes,
        via: "anonymous",
    }
}

/// Session sharing the most message hashes with this request.
///
/// Only recently-seen sessions are considered: an hour-old conversation being
/// resumed is fine, but matching against something from days ago risks stitching
/// unrelated work together.
fn best_overlap(
    hashes: &[u64],
    sessions: &HashMap<String, SessionState>,
    now: u64,
) -> Option<(String, usize)> {
    const RECENT_SECS: u64 = 6 * 3600;

    let incoming: std::collections::HashSet<u64> = hashes.iter().copied().collect();
    let mut best: Option<(String, usize)> = None;

    for (fp, s) in sessions {
        if now.saturating_sub(s.last_seen) > RECENT_SECS {
            continue;
        }
        if s.msg_hashes.is_empty() {
            continue;
        }
        let matched = s.msg_hashes.iter().filter(|h| incoming.contains(h)).count();
        if matched == 0 {
            continue;
        }
        // Require a meaningful share of the smaller set, not just a lucky hit.
        let smaller = s.msg_hashes.len().min(incoming.len()).max(1);
        let ratio = matched as f64 / smaller as f64;
        if ratio < 0.25 {
            continue;
        }
        if best.as_ref().map(|(_, m)| matched > *m).unwrap_or(true) {
            best = Some((fp.clone(), matched));
        }
    }
    best
}

/// Merge new hashes into a session's tracked set, keeping the most recent.
pub fn merge_hashes(existing: &mut Vec<u64>, incoming: &[u64]) {
    for h in incoming {
        if !existing.contains(h) {
            existing.push(*h);
        }
    }
    if existing.len() > MAX_TRACKED_HASHES {
        let cut = existing.len() - MAX_TRACKED_HASHES;
        existing.drain(0..cut);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sessions_with(fp: &str, hashes: Vec<u64>, last_seen: u64) -> HashMap<String, SessionState> {
        let mut m = HashMap::new();
        m.insert(
            fp.to_string(),
            SessionState {
                key: "sk-test000000000000000".into(),
                provider: "tabi".into(),
                last_seen,
                msg_hashes: hashes,
                ..Default::default()
            },
        );
        m
    }

    const SYS: &str = "You are a coding agent with a long shared system prompt that every agent uses identically.";

    fn body(msgs: &[(&str, &str)]) -> Vec<u8> {
        let arr: Vec<serde_json::Value> = msgs
            .iter()
            .map(|(r, c)| serde_json::json!({ "role": r, "content": c }))
            .collect();
        serde_json::to_vec(&serde_json::json!({ "model": "claude-opus-5", "messages": arr }))
            .unwrap()
    }

    #[test]
    fn header_wins_and_is_labelled() {
        let mut h = HeaderMap::new();
        h.insert("x-session-id", "ses_abc123".parse().unwrap());
        let r = resolve(
            &h,
            &body(&[("user", "anything at all here to be hashed")]),
            &HashMap::new(),
            100,
        );
        assert_eq!(r.fp, "sid:ses_abc123");
        assert_eq!(r.label, "ses_abc123");
        assert_eq!(r.via, "header");
    }

    #[test]
    fn growing_a_conversation_keeps_one_identity() {
        let h = HeaderMap::new();
        let turn1 = body(&[
            ("system", SYS),
            ("user", "Build me a Rust proxy with retry semantics please"),
        ]);
        let turn2 = body(&[
            ("system", SYS),
            ("user", "Build me a Rust proxy with retry semantics please"),
            (
                "assistant",
                "Sure, here is the plan for the proxy implementation",
            ),
            ("user", "continue"),
        ]);
        let a = resolve(&h, &turn1, &HashMap::new(), 100);
        let sess = sessions_with(&a.fp, a.hashes.clone(), 100);
        let b = resolve(&h, &turn2, &sess, 200);
        assert_eq!(a.fp, b.fp);
        assert!(!b.compacted);
        assert_eq!(b.via, "first-message");
    }

    #[test]
    fn compaction_is_detected_and_session_survives() {
        // THE case this module exists for: the client replaces the head with a
        // summary, so the first user message — and its hash — changes.
        let h = HeaderMap::new();
        let before = body(&[
            ("system", SYS),
            ("user", "Build me a Rust proxy with retry semantics please"),
            (
                "assistant",
                "Here is a long answer about connection pooling and retries",
            ),
            (
                "user",
                "Now add a circuit breaker to the provider selection logic",
            ),
            (
                "assistant",
                "Added the breaker with exponential backoff as requested",
            ),
        ]);
        let first = resolve(&h, &before, &HashMap::new(), 100);
        let sess = sessions_with(&first.fp, first.hashes.clone(), 100);

        let after_compaction = body(&[
            ("system", SYS),
            (
                "user",
                "[summary] We were building a Rust proxy; breaker added.",
            ),
            (
                "user",
                "Now add a circuit breaker to the provider selection logic",
            ),
            (
                "assistant",
                "Added the breaker with exponential backoff as requested",
            ),
            ("user", "now wire it into the dashboard as well please"),
        ]);
        let second = resolve(&h, &after_compaction, &sess, 300);

        assert_eq!(second.fp, first.fp, "compaction must not fork the session");
        assert!(second.compacted, "should be flagged as compacted");
        assert_eq!(second.via, "overlap-after-compaction");
    }

    #[test]
    fn different_agents_sharing_a_system_prompt_stay_separate() {
        // System messages are excluded from hashing precisely so this holds.
        let h = HeaderMap::new();
        let a = resolve(
            &h,
            &body(&[
                ("system", SYS),
                ("user", "Investigate the failing latency tests"),
            ]),
            &HashMap::new(),
            100,
        );
        let sess = sessions_with(&a.fp, a.hashes.clone(), 100);
        let b = resolve(
            &h,
            &body(&[
                ("system", SYS),
                ("user", "Write documentation for the billing module"),
            ]),
            &sess,
            110,
        );
        assert_ne!(a.fp, b.fp);
        assert_eq!(b.via, "new");
    }

    #[test]
    fn short_generic_messages_do_not_create_false_matches() {
        let h = HeaderMap::new();
        let a = resolve(
            &h,
            &body(&[(
                "user",
                "Please refactor the provider scoring function today",
            )]),
            &HashMap::new(),
            100,
        );
        let mut sess = sessions_with(&a.fp, a.hashes.clone(), 100);
        // A brand-new conversation that merely contains "ok" / "continue".
        let noise = body(&[("user", "ok"), ("assistant", "sure"), ("user", "continue")]);
        let b = resolve(&h, &noise, &sess, 120);
        assert_ne!(
            b.fp, a.fp,
            "generic chatter must not join an unrelated session"
        );
        sess.clear();
    }

    #[test]
    fn stale_sessions_are_not_matched() {
        let h = HeaderMap::new();
        let msgs = &[
            ("user", "A distinctive first request about proxy internals"),
            (
                "assistant",
                "A distinctive reply describing the internals in detail",
            ),
        ];
        let a = resolve(&h, &body(msgs), &HashMap::new(), 100);
        // Same content, but the stored session was last seen 3 days ago.
        let sess = sessions_with("someoldfp", a.hashes.clone(), 100);
        let compacted = body(&[
            ("user", "[summary] earlier work"),
            ("user", "A distinctive first request about proxy internals"),
            (
                "assistant",
                "A distinctive reply describing the internals in detail",
            ),
        ]);
        let b = resolve(&h, &compacted, &sess, 100 + 4 * 86400);
        assert_ne!(b.fp, "someoldfp", "must not stitch onto days-old work");
    }

    #[test]
    fn handles_content_block_arrays() {
        let h = HeaderMap::new();
        let raw = serde_json::to_vec(&serde_json::json!({
            "messages": [
                { "role": "user", "content": [
                    { "type": "text", "text": "Explain the circuit breaker design in full detail" }
                ]}
            ]
        }))
        .unwrap();
        let r = resolve(&h, &raw, &HashMap::new(), 100);
        assert_ne!(r.fp, "nosession");
        assert_eq!(r.hashes.len(), 1);
    }

    #[test]
    fn garbage_body_is_anonymous_not_a_panic() {
        let h = HeaderMap::new();
        let r = resolve(&h, b"definitely not json", &HashMap::new(), 100);
        assert_eq!(r.fp, "nosession");
        assert_eq!(r.via, "anonymous");
    }

    #[test]
    fn merge_hashes_dedupes_and_caps() {
        let mut v = vec![1, 2, 3];
        merge_hashes(&mut v, &[3, 4, 5]);
        assert_eq!(v, vec![1, 2, 3, 4, 5], "no duplicates");

        let mut big: Vec<u64> = (0..MAX_TRACKED_HASHES as u64).collect();
        merge_hashes(&mut big, &[9_999, 10_000]);
        assert_eq!(big.len(), MAX_TRACKED_HASHES, "must stay bounded");
        assert_eq!(*big.last().unwrap(), 10_000, "keeps the newest");
    }

    #[test]
    fn tool_result_blocks_contribute_to_identity() {
        let h = HeaderMap::new();
        let raw = serde_json::to_vec(&serde_json::json!({
            "messages": [
                { "role": "user", "content": [
                    { "type": "tool_result", "tool_use_id": "toolu_1",
                      "content": "the command produced this distinctive long output text" }
                ]}
            ]
        }))
        .unwrap();
        let r = resolve(&h, &raw, &HashMap::new(), 100);
        assert_eq!(r.hashes.len(), 1, "tool results must be hashable");
    }
}
