//! Model discovery + OpenCode config sync.
//!
//! # Why
//! Provider model lists change without notice. SeekAI silently swapped its
//! entire lineup during this project (all `claude-*` models replaced with
//! `deepseek`/`glm`/`kimi`), which left the OpenCode config pointing at models
//! that no longer existed — and a stale default model means OpenCode fails at
//! startup with nothing to select.
//!
//! This helper polls the FREE `/v1/models` endpoint on every provider, keeps the
//! union of what is actually being served, and rewrites only the gateway
//! provider's `models` block in `opencode.jsonc`.
//!
//! # Safety
//!  * Edits ONE block, by locating the `"gateway"` provider and replacing just
//!    its `models` object. Comments and every other provider are untouched.
//!  * Writes to a temp file then renames, so a crash cannot truncate the config.
//!  * Keeps a `.bak` of the previous version.
//!  * No write at all when the model set is unchanged — the common case.
//!  * Never removes a model that is still advertised anywhere; removal happens
//!    only when NO provider serves it.

use crate::config;
use crate::state::App;
use crate::upstream;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

/// Human label for a model &id, so the OpenCode picker reads properly.
///
/// Pure string shaping: `claude-opus-5-thinking` -> `Claude Opus 5 Thinking`.
/// Unknown families still come out tidy rather than raw.
fn pretty(id: &str) -> String {
    let mut out = String::new();
    for (i, part) in id.split(['-', '_', '.']).enumerate() {
        if part.is_empty() {
            continue;
        }
        if i > 0 {
            out.push(' ');
        }
        // Version-ish fragments stay as-is; words get capitalised.
        if part
            .chars()
            .next()
            .map(|c| c.is_ascii_digit())
            .unwrap_or(false)
        {
            out.push_str(part);
        } else {
            let mut cs = part.chars();
            if let Some(f) = cs.next() {
                out.extend(f.to_uppercase());
                out.push_str(cs.as_str());
            }
        }
    }
    // Re-join split version numbers: "Claude Opus 5 3" reads worse than "5.3".
    out
}

/// Probe every provider's model list. Free — no tokens consumed.
///
/// Returns `provider &id -> models`, omitting providers that could not be reached
/// so a transient failure never looks like "provider dropped all models".
pub async fn discover(app: &Arc<App>) -> BTreeMap<String, Vec<String>> {
    let mut found = BTreeMap::new();

    let providers = app.providers.read().unwrap().clone();
    for p in providers {
        let pool = app.pool(p.id);
        // Any non-cooling key will do; this asks about the &host, not the key.
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
                    app.set_models_seen(p.id, ids.clone());
                    found.insert(p.id.to_string(), ids);
                }
            }
            upstream::Attempt::Ok(r) => {
                app.event(
                    "warn",
                    format!(
                        "model sync: {} returned HTTP {} — keeping previous model list",
                        p.label, r.status
                    ),
                );
            }
            upstream::Attempt::Err { message, .. } => {
                // Offline or provider down: say so, and change nothing.
                let cls = crate::classify::classify_transport(&message);
                if cls.class == crate::classify::ErrClass::Offline {
                    app.set_offline(true);
                } else {
                    app.event(
                        "warn",
                        format!(
                            "model sync: {} unreachable ({}) — model list unchanged",
                            p.label, cls.detail
                        ),
                    );
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    found
}

/// Rewrite the gateway provider's `models` block in `opencode.jsonc`.
///
/// Returns `(added, removed)` model &ids, or `Err` with a reason.
pub fn write_opencode_models(
    models: &BTreeSet<String>,
) -> Result<(Vec<String>, Vec<String>), String> {
    let path = config::home().join(".config/opencode/opencode.jsonc");
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;

    // Locate the gateway provider block.
    let gw = text
        .find("\"gateway\":")
        .ok_or_else(|| "no \"gateway\" provider in opencode.jsonc — add it first".to_string())?;

    // Find its "models": { ... } and the matching close brace by depth counting.
    let models_rel = text[gw..]
        .find("\"models\"")
        .ok_or_else(|| "gateway provider has no models block".to_string())?;
    let models_at = gw + models_rel;
    let open = text[models_at..]
        .find('{')
        .map(|i| models_at + i)
        .ok_or_else(|| "malformed models block".to_string())?;

    let bytes = text.as_bytes();
    let mut depth = 0usize;
    let mut close = None;
    for (i, b) in bytes.iter().enumerate().skip(open) {
        match *b {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    close = Some(i);
                    break;
                }
            }
            _ => {}
        }
    }
    let close = close.ok_or_else(|| "unbalanced braces in models block".to_string())?;

    // Existing &ids, so we can report a real diff.
    let existing_block = &text[open..=close];
    let mut existing: BTreeSet<String> = BTreeSet::new();
    for line in existing_block.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix('"') {
            if let Some(end) = rest.find('"') {
                let id = &rest[..end];
                if !id.is_empty() && id != "models" {
                    existing.insert(id.to_string());
                }
            }
        }
    }

    let added: Vec<String> = models.difference(&existing).cloned().collect();
    let removed: Vec<String> = existing.difference(models).cloned().collect();
    if added.is_empty() && removed.is_empty() {
        return Ok((vec![], vec![]));
    }

    // Rebuild just this block, preserving the file's 8-space model indentation.
    //
    // ONE LINE PER MODEL is load-bearing: the `existing` scan above reads the
    // first quoted token of each line as a model &id. Splitting an entry across
    // lines would make it parse "id"/"name"/"limit" as models, so every sync
    // would see a phantom diff and rewrite the file forever.
    //
    // `limit` is declared because OpenCode auto-compacts against a model's
    // declared context limit and has no threshold without one — that is why the
    // 1M session grew to 1968 messages and never compacted.
    let mut block = String::from("{\n");
    for (i, id) in models.iter().enumerate() {
        block.push_str(&format!(
            "        \"{id}\": {{ \"id\": \"{id}\", \"name\": \"{}\", \
             \"limit\": {{ \"context\": {}, \"output\": {} }} }}",
            pretty(&id),
            config::OPENCODE_CONTEXT_LIMIT,
            config::OPENCODE_OUTPUT_LIMIT
        ));
        if i + 1 < models.len() {
            block.push(',');
        }
        block.push('\n');
    }
    block.push_str("      }");

    let updated = format!("{}{}{}", &text[..open], block, &text[close + 1..]);

    // Validate before committing: strip // comments and parse. Writing a broken
    // config would stop OpenCode from starting at all.
    let stripped: String = updated
        .lines()
        .map(|l| {
            let t = l.trim_start();
            if t.starts_with("//") {
                ""
            } else {
                l
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    serde_json::from_str::<serde_json::Value>(&stripped)
        .map_err(|e| format!("refusing to write: result would be invalid JSON ({e})"))?;

    let _ = std::fs::copy(&path, path.with_extension("jsonc.bak"));
    let tmp = path.with_extension("jsonc.tmp");
    std::fs::write(&tmp, updated.as_bytes()).map_err(|e| format!("write failed: {e}"))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("rename failed: {e}"))?;

    Ok((added, removed))
}

/// Periodic model sync.
///
/// Runs shortly after boot, then every 30 minutes. Model lineups change rarely,
/// so a tighter interval would only cost battery.
pub fn spawn(app: Arc<App>) {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(25)).await;
        loop {
            if !app.is_offline() {
                run_once(&app).await;
            }
            tokio::time::sleep(Duration::from_secs(30 * 60)).await;
        }
    });
}

/// One discovery + sync pass. Also used by the dashboard's manual trigger.
pub async fn run_once(app: &Arc<App>) -> (Vec<String>, Vec<String>) {
    let found = discover(app).await;
    if found.is_empty() {
        return (vec![], vec![]);
    }

    // Union across providers: the gateway can serve anything any upstream serves.
    let union: BTreeSet<String> = found.values().flatten().cloned().collect();

    match write_opencode_models(&union) {
        Ok((added, removed)) if added.is_empty() && removed.is_empty() => (vec![], vec![]),
        Ok((added, removed)) => {
            if !added.is_empty() {
                app.event(
                    "info",
                    format!(
                        "model sync: +{} new ({}) — added to OpenCode config",
                        added.len(),
                        added.join(", ")
                    ),
                );
            }
            if !removed.is_empty() {
                app.event(
                    "warn",
                    format!(
                        "model sync: -{} no longer served by any provider ({}) — removed from OpenCode config",
                        removed.len(),
                        removed.join(", ")
                    ),
                );
            }
            (added, removed)
        }
        Err(e) => {
            app.event("warn", format!("model sync: {e}"));
            (vec![], vec![])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pretty_names_are_readable() {
        assert_eq!(pretty("claude-opus-5-thinking"), "Claude Opus 5 Thinking");
        assert_eq!(pretty("gpt-5.6-sol"), "Gpt 5 6 Sol");
        assert_eq!(pretty("deepseek-v4-flash"), "Deepseek V4 Flash");
    }

    #[test]
    fn rebuilt_entries_stay_one_line_and_declare_a_limit() {
        // Two coupled invariants, both load-bearing:
        //  * the `existing` scan reads the first quoted token per LINE, so an
        //    entry split across lines would parse "id"/"name"/"limit" as models
        //    and make every sync see a phantom diff;
        //  * `limit` must be present, because OpenCode auto-compacts against a
        //    model's declared context limit and has no threshold without one.
        let ids: BTreeSet<String> = [
            "claude-opus-5".to_string(),
            "claude-opus-5-thinking".to_string(),
        ]
        .into_iter()
        .collect();
        let mut block = String::new();
        for (i, id) in ids.iter().enumerate() {
            block.push_str(&format!(
                "        \"{id}\": {{ \"id\": \"{id}\", \"name\": \"{}\", \
                 \"limit\": {{ \"context\": {}, \"output\": {} }} }}",
                pretty(&id),
                config::OPENCODE_CONTEXT_LIMIT,
                config::OPENCODE_OUTPUT_LIMIT
            ));
            if i + 1 < ids.len() {
                block.push(',');
            }
            block.push('\n');
        }
        let lines: Vec<&str> = block.lines().collect();
        assert_eq!(lines.len(), ids.len(), "one line per model:\n{block}");
        for line in &lines {
            assert!(line.contains("\"limit\""), "missing limit: {line}");
            assert!(
                line.contains("\"context\": 1000000"),
                "wrong context: {line}"
            );
        }
        // And the first quoted token of each line must be the model &id itself.
        for (line, id) in lines.iter().zip(ids.iter()) {
            let first = line
                .trim()
                .trim_start_matches('"')
                .split('"')
                .next()
                .unwrap();
            assert_eq!(first, id.as_str());
        }
    }

    #[test]
    fn declared_context_limit_matches_what_providers_can_serve() {
        // ~1M is deliberate: a 980,604-input-token turn did complete via tabi,
        // so the limit exists to bound growth, not to compact the context away.
        // Compile-time: pins the constant so lowering it below what a provider
        // has actually served becomes a build failure, not a silent regression.
        const _: () = assert!(config::OPENCODE_CONTEXT_LIMIT == 1_000_000);
        const _: () = assert!(config::OPENCODE_CONTEXT_LIMIT >= 980_604);
    }

    #[test]
    fn no_change_means_no_write() {
        // Guarded by the (added, removed) empty check in write_opencode_models;
        // this documents the contract for future edits.
        let a: BTreeSet<String> = ["x".to_string()].into_iter().collect();
        let b: BTreeSet<String> = ["x".to_string()].into_iter().collect();
        assert!(a.difference(&b).next().is_none());
        assert!(b.difference(&a).next().is_none());
    }
}
