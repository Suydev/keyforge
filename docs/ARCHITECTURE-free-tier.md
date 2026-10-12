# Free-tier egress architecture

How keyforge keeps working against a per-IP rate limit, and why it is built
this way. Written after the `FreeUsageLimitError` incident, from measurements
rather than assumption.

## The finding that shaped everything

`opencode.ai` (OpenCode Zen) enforces its free tier with a client-fingerprint
gate plus a **per-egress-IP** usage budget. Measured on this device:

| Attempt | Result |
|---|---|
| Fresh `ses_`/`msg_` ids, **direct** from device IP | `429 FreeUsageLimitError`, `retry-after: 55354` (~15.4 h) |
| Fresh `ses_`/`msg_` ids, **different proxy IP** | `429 FreeUsageLimitError`, `retry-after: 53982` |
| Fresh ids, **40 rotating proxy IPs** | **10 × `200` streaming a real `pong`**, 2 × `429`, 28 dead |

Conclusions that drove the design:

1. **Session id rotation does nothing.** A brand-new `ses_` through an
   already-burnt address still returned 429. The limiter is not keyed on the
   session id.
2. **The limiter is keyed on the source address.** Ten of forty fresh
   addresses answered; two were already spent. Budget is per-IP.
3. **The fingerprint gate is separate and passable.** `ua opencode/1.18+`,
   `x-opencode-session` (`ses_`+12+14 hex), `x-opencode-request`
   (`msg_`+12+14), `x-opencode-client: cli`, `x-opencode-project: global`,
   `stream: true`, and the four tool names `bash`/`glob`/`grep`/`read` —
   case-sensitively. This is what keyforge already injects.

OpenCode's own source agrees: `FreeUsageLimitError` is classified as a
**quota** error, not a throttle, and is marked **non-retryable**.

## Why "just retry" fails

The 429 carries a `retry-after` of hours. `classify.rs` clamps `retry-after`
to 900 s before it can become a persisted key cooldown. That clamp is correct
for a *key* — an unvalidated `Retry-After: 999999999999` would otherwise
retire a key for 31 years — but it is wrong for an *egress address*, whose
budget genuinely resets on the upstream's own clock. So the raw value is kept
alongside (`retry_after_raw`) for callers cooling something that follows the
upstream window.

Worse, the single shared `public` key meant one 429 grounded the entire
provider: cooling the only key for 900 s left zero candidates, and the router
retried into the same wall (the `round 12 → 32` loop in the error log).

## The design

**One conversation, one address.** A keyforge session is pinned to the egress
IP it last succeeded through (`SessionState.zen_egress`), so a conversation
owns that address's per-IP budget instead of the pool burning it. A resumed
session keeps its pin.

**A 429 rotates the address, never grounds the key.** For a zen host the
`ErrClass::Rate` branch:

- cools **only the egress IP**, for `retry_after_raw` bounded by
  `ZEN_429_EGRESS_COOL_MAX_SECS` (2 h);
- rolls the session's `ses_` id (cheap, and what the client asked for);
- clears the session pin, so the next attempt takes a fresh address;
- leaves the shared `public` key usable — the limit was never the key's.

**Egress is scoped to zen.** `proxy_zen_only()` (default on) routes
`opencode.ai` through the rotating pool and sends every other provider
**direct**. A free proxy that mangles TLS or draws a WAF block would otherwise
break paid upstreams that work perfectly direct.

**Only proven addresses carry traffic.** The pool ingests the whole public
list, but `require_vetted()` stays on: an IP becomes eligible only after it
streams a real pong through the zen gate. Ungated, LRU walked unvetted —
mostly dead — addresses, each burning the full head budget, and every request
timed out while the pool reported healthy members.

**Broken addresses are evicted, not retried.** A proxy whose `CONNECT`
answers 200 but whose TLS dies (`close_notify`, `UnknownIssuer`, connection
reset) is a MITM or a black hole. The connector's `note_ok` only proves
`CONNECT`, so the transport branch in `router.rs` now calls `note_fail` on the
named egress: it cools, and after `EVICT_AFTER_FAILS` it leaves the pool.

**Fail fast.** The head budget is sized for a direct upstream (120 s default
plus a size term). Handing that to a black-hole proxy cost the full budget per
attempt — measured p90 head latency was 170 s, exactly `HEAD_BUDGET_MAX`,
while a direct request finished in **1.2 s**. Proxied attempts are therefore
capped at `PROXY_HEAD_BUDGET_SECS` (25 s): a verified proxy answers in
seconds, so a slow one is dead and should be rotated past, not waited out.
Direct egress keeps the full modelled budget.

## Layout

| File | Role |
|---|---|
| `src/session.rs` | session identity; survives client-side compaction |
| `src/proxy.rs` | pool, LRU + pin, eviction, CONNECT tunnel, `note_zen_429` |
| `src/upstream.rs` | outbound HTTPS, zen fingerprint, `vet_zen_ping`, `direct_get` |
| `src/router.rs` | the ladder: key selection, per-class failure handling |
| `src/state.rs` | `SessionState.zen_egress` / `zen_session_id`, provider prune |
| `src/config.rs` | pool scoping, budgets, cooldowns, download source |
| `src/helpers.rs` | downloader, candidate ingest, pong vetting |

## Known limits

- Free proxies rot in minutes. The pool needs continuous harvesting; a
  one-shot list is stale almost immediately.
- Ten of forty verified addresses answered in testing. The pool is small by
  nature and must be refilled continuously.
- If zen's free pool is saturated **globally**, no amount of IP rotation
  helps within the window; the request must fail over to a paid provider.

## Provenance

The fingerprint contract was reverse-engineered from
[`jcleng/opencode-free-provider`](https://github.com/jcleng/opencode-free-provider),
which credits [`Fly143/OpenCode-Zen-free-api`](https://github.com/Fly143/OpenCode-Zen-free-api).
The per-IP budget conclusion is from direct measurement on this device, not
from either project.
