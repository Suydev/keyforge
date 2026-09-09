# Architecture

Roughly 9,700 lines of Rust in twelve modules, plus a dashboard with no build
step. No framework, no database, eleven direct dependencies.

The shape follows from two constraints. The gateway must never emit a retryable
error, and it runs on an Android tablet inside a ~5%/hour battery budget. Most of
what follows is one of those two constraints made concrete.

---

## Module map

```
main.rs        server, routing table, signal handling, graceful shutdown
router.rs      the retry ladder — the core of the thing
upstream.rs    outbound requests, header hygiene, streaming head
classify.rs    the error taxonomy
state.rs       keys, sessions, providers, history; the one shared mutex
session.rs     session identity, including survival across compaction
config.rs      constants, each with its reasoning attached
settings.rs    runtime-editable providers and routing weights
api.rs         dashboard JSON and SSE
proxy.rs       egress pool: LRU, health, direct fallback
helpers.rs     background tasks — probes, sweeps, flushing
modelsync.rs   keeps a client's model list in step with the providers
link.rs        local Wi-Fi quality (Android only, inert elsewhere)
```

Sizes are lopsided on purpose. `state.rs` is the largest because it is the single
place mutable state lives; `classify.rs` carries the most tests because
misclassifying an error is the most expensive mistake available.

---

## Request lifecycle

```
client
  │
  ├─ parse route            provider pin? which wire format?
  ├─ collect body           capped at 32MB
  ├─ identify session       header, else first-message overlap
  │
  ├─ ROUND 1 ─────────────────────────────────────────────────
  │    for each provider, ordered by score:
  │      for up to 6 keys:
  │        ├─ check deadline — skip if a viable attempt cannot fit
  │        ├─ compute head budget from EWMA + body size + link
  │        ├─ pick egress proxy (LRU)
  │        ├─ send, await response HEAD only
  │        │
  │        ├─ 2xx ──────► commit headers, stream body, done
  │        └─ else ─────► classify, act, continue or break
  │
  ├─ ROUND 2, 3, ...       backoff, while failures look transient
  └─ deadline at 480s ───► terminal error with the attempt trail
```

### Headers are written exactly once

After a 2xx, never before. This is the mechanism that makes retrying invisible: a
response that took twenty attempts is byte-identical to one that took one.

The consequence is worth stating plainly, because it constrains everything
downstream: **once headers are committed, failover is impossible.** A stream that
dies mid-body cannot be moved to another key — the client already holds a 200 and a
partial body. Mid-stream failure is therefore a genuinely different problem from
pre-header failure, not a variation of it.

### The deadline is checked per attempt

Not only at the top of a round. Checking only at round boundaries meant a round
starting with 100s remaining would run ~185s past the deadline, and its last
attempts would receive budgets too small to possibly succeed — 19s, then 5s, on a
body needing 110s.

Those are not retries. They are guaranteed failures that also mark healthy keys
slow, poisoning the routing data. The guard breaks out instead:

```rust
if remaining < head_budget.min(head_min_secs) { break }
```

---

## The error taxonomy

Eight classes, because treating failures uniformly is what makes a naive proxy
destructive. Each gets a different action:

| class | means | key | provider | retry |
| --- | --- | --- | --- | --- |
| `Quota` | out of money, exact remainder reported | **retired 6h** | untouched | next key |
| `Auth` | invalid or revoked | **retired 7d** | untouched | next key |
| `Rate` | healthy but busy | cooled | untouched | next key |
| `Timeout` | slow, not dead | untouched | soft count | next key |
| `Waf` | CDN challenge — our egress | **untouched** | soft count | sibling provider |
| `NoChannel` | no backend for this model | **untouched** | **cooled 45s** | sibling provider |
| `Upstream` | genuine 5xx, reset | untouched | counted, breaker on streak | sibling provider |
| `Offline` | no network at all | untouched | untouched | hold and wait |

Four distinctions in that table were learned the hard way.

**`Waf` must never burn a key.** A CDN 403 is about the egress IP or the
User-Agent; the key is perfectly good. An early version retired keys on these and
destroyed a large part of the pool before the cause was found.

**`Timeout` is not `Upstream`.** A 524 means the origin is *still working* but
slow. Treating it as death switches away from a provider that was about to answer.
It counts softly and does not trip the breaker.

**`NoChannel` is a provider fact, not a key fact.** When an upstream answers
`no available channel` or `all nodes exhausted`, every sibling key returns the
identical answer. Rotating them is pure waste — and measurably harmful: ~57
attempts of 282-540KB across two providers in 8 minutes earned a CDN rate-limit
strike and turned an upstream outage into a self-inflicted one. It cools the
provider and deliberately does **not** touch `consecutive_fails` or the error
EWMA, because the host is healthy and must not stay demoted after recovery.

**`Quota` requires two independent signals.** Retiring a funded key on a misread
costs real money, so one ambiguous message is not enough.

### Classification is textual, and that is a liability

Upstreams report the same condition in different words across versions and
locales. `classify.rs` matches machine-readable codes first, then English prose,
then Chinese prose. Real cases in the test suite:

- a fullwidth dollar sign (`＄-0.145652`) in a balance message
- an unclamped `Retry-After` that once retired a key for 31 years
- an English quota refusal that matched none of the Chinese phrase lists, fell
  through to `Client`, and never rotated the key

Each of those is a test now. They are the specification.

---

## State

One `Mutex<Inner>` holding everything: keys, sessions, providers, history buckets,
the error log. A `RwLock` or per-field locks would be more elegant and would buy
nothing — writes are frequent but microscopic, and the contention never
materialises at this scale. Simpler is worth more here than theoretically faster.

```
keys      sk-... -> { provider, balance, usage, dead_until, reason, last_probe }
sessions  fp     -> { provider, key, turns, bytes, hashes, api, client }
providers id     -> { ewma_ttfb, ewma_err, consecutive_fails, open_until, uptime }
history          -> minute / hour / day buckets, append-only ring
errors           -> capped log, full detail per record
```

### What is learned rather than configured

- **Per-request holds.** Read from the exact figure in a quota 403, per
  `(provider, model)`. Configured values are starting guesses only.
- **Balances.** From the provider's free `/v1/models` probe, so the pool stays
  accurate without spending.
- **Time-to-first-byte.** Per-provider EWMA, feeding both routing and timeouts.
- **Failure patterns.** Every error records `req_bytes`, `budget_secs`, `ttfb_ms`,
  `streaming`, and `proxy`, so the timeout model can be corrected against reality
  instead of intuition.

### Persistence

`~/.config/tabi/gateway-state.json`, coalesced every 20s because flash wear is a
real constraint. Forward-compatible — unknown fields ignored, missing ones
defaulted — so upgrades need no migration. A corrupt file is renamed aside rather
than silently replaced, so it can be inspected.

A PID lockfile prevents two instances sharing one state file. A live process holds
its own copy in memory and writes on exit, so hand-editing the file underneath a
running gateway loses the edit.

---

## Session identity

Sibling keys can share one wallet, so two sessions on sibling keys compete for one
balance. Pinning a session to a key is load-bearing, not cosmetic.

Identity resolves in order:

1. `x-session-id` (or `x-opencode-session`, `x-conversation-id`) — exact, cheap
2. hash of the first user message — stable across reconnects
3. **message overlap** — survives context compaction, when the first message is
   gone entirely
4. anonymous — everything unidentifiable

Step 3 exists because agents compact their context. Without it, every compaction
looked like a brand-new session, took a fresh key, and abandoned whatever balance
the old one had reserved.

Step 4 is the weak point. All unidentifiable requests currently share one identity,
and therefore one key and one wallet. Anonymous traffic should be treated as
stateless and spread rather than pinned.

---

## Egress

Upstream rate limits are commonly per-IP as well as per-account. With every
request leaving one address, ten separate accounts were rate-limited together —
the shared factor was the IP, not the wallet.

`proxy.rs` implements a `tower_service::Service<Uri>` sitting *underneath*
`hyper-rustls`, so TLS and connection pooling work unchanged and tunnelling is
transparent to everything above.

- least-recently-used selection across healthy endpoints
- a failed proxy is **cooled, not dropped** — a blip should not shrink the pool
- if every proxy is cooling, traffic goes **direct**: degraded egress beats a
  failed request
- the egress actually used is recorded per attempt via a task-local, so a
  systematically bad proxy is attributable

Two known limitations, both about rotation being less effective than it looks:

**hyper's pool keys by `(scheme, host, port)`**, with no knowledge of which proxy
tunnelled a socket. Warm connections are therefore reused across proxies for the
idle timeout, so rotation is partly illusory. The fix is one `Client` per proxy.

**LRU has one-second granularity.** Sessions connecting inside the same second
read identical `last_used` values and tie-break by iteration order, so they all get
the *same* proxy. Millisecond resolution fixes it.

---

## Local link quality

Every latency measurement includes the local network hop. When Wi-Fi degrades,
TTFB rises for reasons the provider is not responsible for, and the naive response
is to mark keys slow and trip breakers on hosts that were answering fine.

`link.rs` samples `dumpsys wifi` through Shizuku's `rish`, preferring Android's own
`score` and `isUsable` over hand-rolled RSSI thresholds — Android already accounts
for retransmits and throughput, and RSSI can look healthy while retries spike.
Falls back to `termux-wifi-connectioninfo` when Shizuku is absent.

Three deliberate properties:

- **Off the request path.** Sampling spawns a process; it runs on its own OS thread
  every 60s and callers read a cached snapshot.
- **`Unknown` costs nothing.** Non-Android, or not yet sampled, means a multiplier
  of exactly 1.0. Any other default would silently change routing everywhere the
  sampler cannot run.
- **It never refuses a request.** A -70dBm link still moves ~500KB/s. Gating on
  signal strength would invent an outage. The only outputs are a wider budget and
  an easier retry cadence.

---

## Dashboard

Vanilla JavaScript and canvas. No framework, no bundler, no build step — the
assets are `include_str!`-embedded, so the binary is self-contained.

That is a battery decision as much as a taste one. Server-sent events push a
snapshot **only when state changes**, never on a timer, and the client closes the
stream when the tab is hidden. An idle dashboard costs approximately nothing.

Snapshot payloads are capped rather than complete: 120 uptime samples of 720
retained, 120 minute buckets, 40 errors, 24 sessions. Sending full history made
every push dominate the payload and showed up as UI lag. Charts that need the full
series fetch it explicitly.

Counts and percentages are computed **before** the cap, not after. `uptimePct`
averages all 720 retained samples and `sessionsTotal` counts every session, so
trimming the payload never quietly narrows what a number means.

### Rendering is reconciled, not rebuilt

Each list is keyed by a stable identity — provider id, in-flight id, session
fingerprint — and a diff creates only new rows, patches existing ones in place,
and removes departed ones. Entrance animation is applied on creation only, and
the class is dropped on `animationend`.

Rebuilding with `innerHTML` was visibly wrong, not merely slower. Three things
broke: entrance animations replayed on every push so cards flashed every few
seconds; the pulsing in-flight dot restarted; and transient state inside rows —
focus, text selection mid-copy, the ticker-owned elapsed cell — was destroyed.
The `animationend` cleanup matters for the leaderboard specifically, because it
reorders by score and `insertBefore` on a connected node restarts a running
animation.

### Time is recomputed locally

Durations come from the absolute `started` timestamp, advanced by a 1s client
ticker. Server-computed `elapsedSecs` freezes between pushes, and since
`inflight_phase` only bumps the revision on a phase *change*, a request waiting
130s for a response head produced no pushes at all — the dashboard looked dead
while the gateway was working correctly.

A freshness pill states seconds since the last push and turns amber past 45s.
The heartbeat alone is 20s, so silence past that is a real fault rather than
"nothing changed" — and a green dot on frozen data is the most misleading thing
a live dashboard can show.

### Repaints are coalesced, pushes are not

State updates on every push so data is never stale; the repaint is throttled to
the selected speed tier (Fast 250ms / Moderate 1s / Slow 4s). A push arriving
inside the window is not dropped — it repaints at the end of it with the newest
data. The DOM work, not the network, was the cost.

### Assets are `no-store`

`no-cache` without an ETag or `Last-Modified` still let an ES module linger in
the browser's module map across reloads, which produced a genuinely confusing
bug: a fixed dashboard throwing an error from a line that no longer existed in
the served file. Over loopback the re-download is ~75KB and costs nothing
measurable. `HEAD` is served identically to `GET` minus the body, per RFC 9110.

---

## Testing

141 tests. No mocks, no network, no fixtures — all pure logic, which is why they
run in the debug profile in CI and finish in about a tenth of a second.

| module | tests | what they pin |
| --- | --- | --- |
| `state.rs` | 49 | key selection, balances, breakers, history, budget arithmetic |
| `classify.rs` | 28 | the taxonomy, including every incident above |
| `router.rs` | 15 | route parsing, usage extraction, deadline behaviour |
| `proxy.rs` | 11 | endpoint parsing, LRU rotation, cooling, direct fallback |
| `session.rs` | 10 | identity, compaction survival, collision resistance |
| `link.rs` | 9 | `dumpsys` parsing, quality buckets, the inert default |
| `api.rs` | 8 | masking, snapshot caps, bucket merging |
| `settings.rs` | 7 | runtime settings round-trip |
| `modelsync.rs` | 4 | config rewriting without clobbering |

Several tests encode a specific past failure rather than a general property. Those
are the valuable ones — they are the difference between "we fixed it" and "it
cannot come back".

The release profile is expensive by design (`lto`, `codegen-units = 1`,
`opt-level = "z"`), so CI runs debug for tests and pays for release only on a tag.

---

## Known gaps

Honest list, in rough priority order.

1. **Mid-stream death looks like a clean EOF.** Truncation is invisible to the
   client, and SDKs treat truncation as retryable — the exact failure this gateway
   exists to prevent. Needs a synthetic terminal error event injected per wire
   format.
2. **Body is buffered, then cloned per attempt.** Costs ~13s of latency and a
   2.7MB heap copy per retry. Streaming through would remove most of the measured
   gateway overhead.
3. **Per-proxy `Client` sharding**, so hyper's pool stops defeating rotation.
4. **Millisecond LRU resolution**, so same-second sessions do not share one proxy.
5. **Anonymous requests should be stateless**, not pinned to one shared key.
6. **Multi-host failover is defined but not wired** — `Settings::hosts()` exists,
   `send_head` always uses the primary.
7. **Settings hot-reload** — `settings_mtime` is tracked but never checked.
8. **`state.rs` is ~2,800 lines** and should be split.

Items 6 and 7 are why `dead_code` is allowed at the crate level: the machinery is
present, tested, and scheduled, not accidental.
