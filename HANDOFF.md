# HANDOFF — tabi-gateway → Claude Opus 5 Thinking

Date: 2026-09-03. Previous session ran on muse-spark; all further coding goes
through you (claude-opus-5 family via the gateway or direct).

## What this is

`~/tabi-gateway/` — Rust async HTTP gateway (tokio + hyper + rustls) running on
Android/Termux (aarch64). It fronts three `new-api` deployments
(github.com/QuantumNous/new-api, sparse-cloned at `~/tmp/new-api/`) that serve
Claude models, owning **1745 API keys** so clients never see a dead key:

- `tabitoken.com` — 859 keys, flat $0.80/request pre-deduction hold
- `gorouter.app` — 885 keys, flat $0.30/request pre-deduction hold
- `api.justwoker.icu` — 1 key, per-token pricing

Clients: OpenCode (`gateway/*` provider → `http://127.0.0.1:8787/v1`) and Claude
Code (`ANTHROPIC_BASE_URL=http://127.0.0.1:8787`). Both API shapes supported:
`/v1/messages` (Anthropic) and `/v1/chat/completions` (OpenAI-compatible).

## Current status: FUNCTIONAL

- `cargo build --release` clean (13 warnings, 0 errors). Binary: 1.9MB.
- `cargo test --release`: **121/121 pass**.
- Running on :8787 via `tabi` launcher (`~/.local/bin/tabi`). Verified live:
  dashboard serves our HTML, `/api/health` OK, real proxied chat completion
  returned HTTP 200 in ~6s.
- State persists in `~/.config/tabi/gateway-state.json` (~400KB, 1745 keys).
- Old Node proxy (`~/tabi-proxy.mjs`) is **retired** (renamed to
  `tabi-proxy.mjs.retired`). Do NOT restart it — it squats :8787 and forwards
  unknown paths upstream (that's how the user once got tabitoken's homepage
  from localhost).

## How to work on it

```bash
cd ~/tabi-gateway
cargo build --release          # ~2-5 min on device
cargo test --release           # must stay green
tabi restart                   # rebuild picked up; safe ONLY if your own
                               # session is NOT routed through the gateway
tabi status                    # health + per-provider summary
tabi log                       # follow the log (~/tmp/tabi-gateway.log)
```

**CRITICAL LESSON (caused real outages):** never `pkill`/restart the gateway
while your own session is served through it — you cut your own connection.
Check first: if your model's provider in `~/.config/opencode/opencode.jsonc`
is `gateway/*`, switch to a direct provider (`justwoker/*`, `gorouter/*`) or
accept the disconnect. The `~/.local/bin/opencode` wrapper auto-starts the
gateway if :8787 is dead, so a fresh `opencode` invocation recovers it.

Other gotchas:
- `/tmp` does NOT exist on Termux. Use `~/tmp`.
- `TABI_STATE` env selects an alternate state file (used for the :8801 test
  instance). A `gateway-state.lock` PID lockfile prevents two instances sharing
  one state file — remove stale locks only after confirming no process holds them.
- Key files live in `~/git_gorouter_tabitoken/*-keys.txt` (gitignored — **this
  device is the only copy**). Commit `f28af9d "d"` once DELETED them; they were
  restored from `b1bbaf4`. Never `git clean` that repo.
- `state.rs` is ~2500 lines and still growing — the next refactor should split it.

## What is already implemented and verified

- Six-class error taxonomy (`classify.rs`): QUOTA needs two independent signals;
  WAF never burns a key; negative balances (`＄-0.145652`) parse; unclamped
  `Retry-After` fixed (was able to retire a key for 31 years).
- Balance-aware drain-first key selection; sticky session→key mapping persisted
  across restarts; first-user-message fingerprinting that survives compaction
  via message-overlap matching (`session.rs`).
- Adaptive streaming head budget from per-provider TTFB EWMA
  (`budget = clamp(ewma*3, 45s, 130s)`), env-tunable, no rebuild needed.
- Slow-vs-dead split: timeouts/524s count lightly, trip the breaker only after
  a long solo run, so a provider digesting a huge context isn't switched away
  from mid-work.
- HTTP CONNECT proxy pool (10 Webshare proxies, LRU, health-tracked, direct
  fallback) wired under hyper-rustls.
- Failure training data: every error recorded with req_bytes/streaming/ttfb/
  budget/proxy/head_timeout; analysis refreshes estimates every 200 failures.
- Dashboard (vanilla JS + canvas, no framework): overview, providers +
  leaderboard, models, sessions + detail drawer, keys + verify-and-add, traffic,
  events, errors tab, egress panel. SSE pushes on change only; pauses when the
  tab is hidden (battery).
- `tabi` CLI (`start|stop|restart|status|log|open|fg`); opencode wrapper at
  `~/.local/bin/opencode` auto-starts the gateway and REPLACES a squatter.
- `modelsync.rs`: polls free `/v1/models`, rewrites only the gateway block in
  `opencode.jsonc` (temp+rename, .bak, JSON-validated).
- `settings.rs`: runtime `providers.json` (multi-host failover incl. verified
  `tabitoken.cc` backup sharing tabi's wallet), tunable routing weights.
- Key verification endpoint: identifies owning provider via free probes, adopts
  only funded keys.

## NOT yet applied — the remaining work, in priority order

These come from 11 subagent review passes (10 parallel + 1 recovered from DB).
Highest value first.

**Applied 09-04** (see `~/1m-context-findings.md` for the measurements behind
them; all 121 tests green, NOT yet running — needs a restart):
- Size-aware head budget. `head_budget_secs(provider, req_bytes)` adds a size
  term (`req_MB * 20.3s * 2.0`) to the EWMA term. The scalar EWMA alone gave a
  281KB and a 2.7MB request the identical 45s, so ~1M contexts could never
  succeed on any key. New knobs: `TABI_HEAD_SECS_PER_MB`, `TABI_HEAD_SIZE_SAFETY`.
- Intra-round deadline guard (was item 1). An attempt is not started when the
  time left cannot fit a viable budget; `out_of_time` short-circuits the provider
  ladder and the backoff. Kills the 487s>480s overrun and the 19s/5s attempts
  that could not win but still marked keys slow.
- Dry providers are now *dropped* from the ladder, not merely sorted last —
  unless nobody is funded, in which case all are kept so the normal "no funded
  key" path still reports. justwoker cost a rung on all 12 attempts of the failed
  1M request.
- `reqBytes`/`budgetSecs`/`ttfbMs`/`streaming`/`headTimeout`/`proxy` are
  serialised into the API errorsLog and shown as three new Errors-tab columns
  (Body / Budget / Egress). `proxy` is now actually populated: it is read inside
  the task-local scope in `send_head` and returned on `HeadAttempt`, which is why
  all 137 prior records had `proxy: ""`.
- `opencode.jsonc` gateway models declare `limit: {context: 1000000, output:
  65535}`, and `modelsync.rs` writes that limit on every future rewrite. Without
  a declared limit opencode has no auto-compaction threshold, which is why the 1M
  session reached 1968 messages and never compacted.

Still outstanding:

1. **Sticky-key liveness gate** — sticky path reuses the session key without
   checking `usable()`/cooldown, guaranteeing a 403 round trip on resumed
   sessions with dead keys. Filter like `candidates_with_hold` does.
2. **`nosession` unpinning** — all unidentifiable requests share ONE session row
   and one key/wallet. Treat `via:"anonymous"` as stateless (spread, don't pin).
3. **Terminal SSE error on mid-stream death** — `PassBody` swallows read errors
   into clean EOF; Anthropic SDKs retry with backoff on truncation, which is the
   exact failure mode this gateway exists to prevent. Inject an error event when
   no `message_stop`/`[DONE]` was seen.
4. **Unique temp filenames in `save()`** — concurrent writers (Ctrl-C handler +
   flusher) share one tmp path; add pid+counter.
5. **Corrupt state → rename aside** (`state.corrupt-<ts>`) instead of silent fresh start.
6. **32MB caps on every `collect()`** (non-streaming body, error body, verify
   endpoint, incoming request) returning 400.
7. **Proxy connect + CONNECT timeout (5-8s)** with `note_fail`, or black-hole
   proxies stall to the full head budget without ever cooling.
8. **Per-proxy `Client` sharding** — hyper's pool keys by (scheme,host,port)
   only, so pooled connections reuse the first proxies and rotation is partly
   illusory. One small-pool Client per proxy.
9. **`set_usage` timestamp gate** — a probe started before a 403 but landing
   after it overwrites the exact 403 remainder with stale-derived balance.
10. **Session key updated at pick time, not on completion** — fixes ghost
    ownership (old key squats "owned" 45 min) and startup pile-on.
11. **`tabi up()` identity check** — verify `/api/health`, not just TCP, so a
    squatter is replaced instead of reported "already running".
12. **Multi-host failover actually wired** — `Settings::hosts()` exists but
    `send_head` always uses the primary; implement backup-host fallback.
13. **`sticky_escape_multiplier` wired** — defined, never read.
14. **Settings hot-reload** — `settings_mtime` never checked; poll it.
15. **Thundering-herd on probes** — singleflight marker so N agents don't probe
    the same unmeasured key simultaneously.
16. **TCP_NODELAY, header timeouts, connection cap** on the server.
17. **SSE `retry:`/`id:` fields** for correct client reconnection.
18. **Downsample uptime in snapshots** (720 samples × 3 providers dominates
    payload — send last ~120).
19. **Session eviction → archive** into `session_history` instead of dropping.
20. **`touch()` removed from `inflight_*`** — they force full state rewrites
    while persisting zero bytes of that data (~GB/day flash writes).
21. **Cache tokens in `parse_usage`** (`cache_creation_input_tokens` etc.).
22. **Expect-header strip; multi-value header append** (cookies collapse today).
23. **Terminal-error-after-headers guard** for streams that fail mid-way.
24. **`PassBody::size_hint` after done** — delegated hint can mismatch framing.
25. **Streaming `inflight` held till stream end** (currently released at head).
26. **PID lockfile: verify cmdline**, not just /proc existence (pid reuse).
27. **Errors vector capped at 8** (only last 8 ever read).
28. **`event()` should `touch()`** so SSE subscribers are notified.
29. Delete compiler-confirmed dead code (`hosts()` callers wired or removed,
    `note_failover`, `provider_scores`, test-only fns) — or implement what they
    promise. 11 such warnings remain.
30. Rename `is_api_path`/`parse_route` disagreement; HEAD/OPTIONS handling.
31. `connect_tunnel` 1xx handling (minor).
32. **Egress bandwidth measurement** (findings §8.6) — periodic down/up probe
    plus observed bytes/sec per request, so the size term of the head budget is
    fed by the *measured* link rate instead of the 20.3s/MB constant. This is what
    makes the budget self-calibrating on a mobile link.
33. **Feed timeouts into a large-request latency estimator** (findings §8.7) —
    only successes call `note_latency_split`, so big contexts are permanently
    excluded from the EWMA that sizes their own budget.

Explicitly DEFERRED (documented, do not implement without discussion):
- `panic = "unwind"` (risky profile change).
- Billing-endpoint as SOLE cost authority (contested semantics; current
  dual-estimator has passing tests pinning it).
- Delta SSE frames / split hot-cold state file (large refactors).
- tokio `watch` replacing the 700ms SSE poll (works fine; idle cost ~0).

## new-api facts established from source (~/tmp/new-api, sparse checkout)

- `TotalUsage = used/QuotaPerUnit*100` (cents spent) — free via billing endpoint.
- Pre-consume is pessimistic (`prompt+max_tokens`); settle refunds the unused
  hold idempotently. Refund logic: `relay/common/billing.go`.
- `RetryTimes` defaults to **0** — new-api itself does not retry; our retries
  are the only safety net.
- Relay timeouts: header 1800s, stream-idle 300s, per-write 30s. Our 120s head
  budget sits safely under all of them; the real ceiling is Cloudflare's edge
  (~100-140s observed 524s).
- Mid-stream upstream death = truncated stream, no error chunk. New-api never
  emits 524 itself — 524s come from the Cloudflare layer in front.
- Rate limits are per-USER and per-IP, not per-key. Rotating keys does NOT add
  headroom; separate egress IPs (the proxy pool) DO.
- No per-key concurrency limit exists. Two sessions on sibling keys compete for
  one account wallet (`TryReserveUserQuota`, Redis-atomic).
- Auth: everything after the first `-` in a key is a channel pin — forward keys
  byte-identical, never split or regenerate them.
- `reasoning_effort` invalid values are 4xx errors, not ignored. `-thinking`
  suffix = high effort; budget≤1024 = low, ≤8192 = medium.
- `usage.cost` exists ONLY on `/v1/chat/completions` responses, never on
  `/v1/messages`. Streaming cost comes only from the final chunk (OpenAI) or
  `message_delta` (Anthropic) — hence billing-delta reconciliation.
- `/api/usage/token/` returns exact remaining quota with only the sk- key —
  better than usage+guess. Not yet wired in; use it.
- Sibling keys from one account share one wallet: session pinning (one
  session, one key) is load-bearing, not cosmetic.

## Live numbers (2026-09-03, will be stale)

- Pool: tabi 851/859 alive (~$99k), gorouter 887/887 (~$44k), justwoker 0/1
  ($70 fully burned — a session left running on it drained $31 in ~25 min at
  ~$117/hr; per-token pricing is brutal for long thinking turns vs tabi's
  refundable $0.80 hold).
- Provider latency (TTFB EWMA): tabi ~63s, gorouter ~80s on ~800k-token contexts.
- OpenCode default: check `~/.config/opencode/opencode.jsonc` — was
  `justwoker/claude-opus-5-thinking` during the burn; should be `gateway/*` or
  `gorouter/*` now.
- SeekAI key rotated at some point; current key in `seekai` provider block.

## Files

- Gateway: `~/tabi-gateway/src/*.rs` (config, classify, state ~2500 lines,
  upstream, router, session, settings, helpers, api, modelsync, proxy, main)
- Dashboard: `~/tabi-gateway/web/` (index.html, app.js ~1345 lines, charts.js, app.css)
- Launchers: `~/.local/bin/tabi`, `~/.local/bin/opencode` (wrapper)
- Keys: `~/git_gorouter_tabitoken/*-keys.txt` (gitignored, only copy)
- State: `~/.config/tabi/gateway-state.json` + `.lock`
- Design system used for dashboard: `~/.config/opencode/skills/ui-ux-pro-max`
- Brainstorming skill installed: `~/.config/opencode/skills/brainstorming`
- Prior recovery notes: `~/opencode-db-notes.md`, `~/oc2cc.py`, `~/load_cc.sh`
