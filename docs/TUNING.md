# Tuning

Every number in this document was measured on the device the gateway runs on. A
few of them replaced earlier guesses that were wrong in ways that caused real
outages, and those corrections are recorded here too — the wrong values are as
instructive as the right ones.

---

## The problem a fixed timeout cannot solve

How long should the gateway wait for an upstream's response head before giving up
on a key and trying the next?

Measured time-to-first-byte, same provider, same key, streaming, varying only
request size:

| request body | approx tokens | TTFB |
| --- | --- | --- |
| 281,305 B | ~100k | **11.9s** |
| 952,057 B | ~340k | **35.5s** |
| 2,702,917 B | ~965k | **95.3s**, then 99.4s on repeat |

Any constant is wrong at one end. Set it to 45s and the 2.7MB request can never
succeed. Set it to 120s and a dead provider on a small request holds the client
for two minutes before failing over.

### The outage this caused

The original implementation derived the budget from one time-to-first-byte EWMA
per provider. That EWMA is trained on whatever traffic dominates — small turns
and free `/v1/models` probes — so it sat around 14.4s, giving
`14.4 × 3 = 43s`, which clamped up to the 45s floor.

**A 281KB request and a 2.7MB request were handed the same 45s.** The small one
needed 12s and passed. The large one needed ~97s and could not pass on any key,
on any provider, ever. Not slow — arithmetically impossible.

The failure mode was ugly. One turn: 12 attempts across two providers, 487s
wall-clock, ~32MB uploaded, zero tokens returned. Then the client retried and
paid it again.

There was a second-order trap. Only *successes* feed the EWMA, so those timeouts
were excluded from the very measurement that would have taught the budget to
accommodate them. The system could not learn its way out.

---

## The formula

```
base      = ewma_ttfb × MULT                    (or DEFAULT if unmeasured)
size_term = (SECS_PER_MB × req_MB + OVERHEAD) × SAFETY
budget    = clamp(base + size_term, MIN, MAX)
```

| constant | value | derivation |
| --- | --- | --- |
| `MULT` | 3.0 | absorbs normal variance without mistaking slow for stuck |
| `SECS_PER_MB` | 35.2 | measured, see below |
| `OVERHEAD` | 15.0 | the gateway's own cost, measured, see below |
| `SAFETY` | 1.35 | covers variance on a measured slope |
| `MIN` | 45 | a fast provider must fail over fast, not hang to the ceiling |
| `MAX` | 170 | just past the observed worst case, near the CDN edge timeout |
| `DEFAULT` | 120 | generous for an unmeasured provider — killing a first-seen host on a tight budget punishes exploration |

### SECS_PER_MB = 35.2

Linear fit through the two smaller measurements:

```
ttfb ≈ 2.0s + 35.2s per MB
```

Checked against the point it was not fitted to: predicts **97.0s** at 2.70MB,
observed **95.3s** and **99.4s**. Within 2s.

An earlier revision used **20.3 s/MB**. That came from a fit whose large point
was a *failure* rather than a measurement, and it understated the slope by 42%.
It appeared to work only because `SAFETY` was `2.0` and absorbed the error.

That distinction matters more than it sounds. A correct slope with a modest
safety factor is not interchangeable with a wrong slope and a large one — the
second is only correct at the single payload size where the two errors happen to
cancel. And since `KEYFORGE_HEAD_SIZE_SAFETY` exists precisely so it can be tuned,
anyone lowering it would have silently under-budgeted every large request.

### OVERHEAD = 15.0

The gateway is not free. Paired A/B on one 952KB payload, both arms pinned to the
same provider, alternating so upstream drift affected both equally:

| round | direct | via gateway |
| --- | --- | --- |
| 1 | 40.2s | 53.8s |
| 2 | 47.1s | 43.1s |
| 3 | 41.5s | 70.6s |
| **mean** | **42.9s** | **55.8s** |
| **spread** | 7s | **28s** |

**+12.9s mean, and four times the variance.** 15.0 carries a little margin.

The cause is in the request path: the whole client body is collected before the
upstream connection is opened, then cloned per key attempt. So the gateway
serialises upload-in and upload-out where a direct client streams through, and a
2.7MB body means a 2.7MB heap copy per retry.

It is a **constant, not a per-MB term**, because it tracks body size but not
provider speed. Folding it into `SECS_PER_MB` would double-count it against the
EWMA base.

It is also **not a law of nature.** Streaming the body through instead of
collecting it would remove most of this. When that lands, lower the constant.

### MAX = 170

Requirement through the gateway: ~97s upstream + ~13s overhead ≈ **110s typical**,
with an observed tail near **140s**.

The previous ceiling was 130s, and it clipped requests that would otherwise have
succeeded. A 2.7MB body computed *to* the clamp and only completed because
failover to a second provider bought extra wall-clock — which looks like success
and is actually luck.

Do not raise it much beyond 170s. The CDN in front of these upstreams answers for
the origin at roughly **128-140s** (observed as 524s landing in that window).
Past that you are waiting for a response that will never arrive.

### Worked example

2.70MB body, provider EWMA 14.4s:

```
base      = 14.4 × 3.0                     = 43.2s
size_term = (35.2 × 2.64 + 15.0) × 1.35    = 145.9s
budget    = clamp(189.1, 45, 170)          = 170s
```

Clamped, with the ceiling doing its job. A 281KB body on the same provider:

```
base      = 43.2s
size_term = (35.2 × 0.27 + 15.0) × 1.35    = 33.1s
budget    = clamp(76.3, 45, 170)           = 76s
```

Both sized to the request rather than to an average of unrelated traffic.

---

## Local link quality

Every latency measurement includes the local network hop. When that degrades,
TTFB rises for reasons the provider is not responsible for — and the naive
response is to mark keys slow, rotate them, and trip breakers on hosts that were
answering perfectly well. The provider gets blamed for the device's antenna.

On Android the gateway samples `dumpsys wifi` via `rish` and reads Android's own
verdict on the link:

```
RSSI: -55   Link speed: 86Mbps   score: 60   isUsable: true
rec[99]: rssi=-59 link=86 tx=96.1 rx=254.4 score=60
```

`score` and `isUsable` are preferred over RSSI thresholds because Android already
folds in retransmits and throughput. RSSI can look healthy while retries spike.

| quality | source | budget multiplier |
| --- | --- | --- |
| Unknown | never sampled, or non-Android | **1.0** |
| Good | score ≥ 50, or RSSI ≥ -60 | 1.0 |
| Fair | score 30-49, or RSSI ≥ -75 | 1.2 |
| Poor | score < 30, RSSI < -75, or `isUsable: false` | 1.5 |

Two deliberate choices:

**`Unknown` is 1.0.** An absent sampler is not evidence of a bad link. Any other
default would silently change routing on every non-Android deployment.

**A weak link never refuses or delays a request.** A -70dBm link still moves
~500KB/s perfectly well. Gating traffic on signal strength would invent an outage
that does not exist. The only outputs are a wider budget and an easier retry
cadence.

---

## Provider cooldown for "no available channel"

`NO_CHANNEL_COOLDOWN_SECS = 45`.

When an upstream answers `{"error":{"message":"no available channel"}}` or
`"all nodes exhausted"`, the site is up, the relay is up, the key is fine, the
money is fine — there is simply no backend to route to. **Every sibling key
returns the identical answer**, so key rotation cannot help.

Treating this as a generic 5xx cost real damage: ~57 attempts of 282-540KB across
two providers in 8 minutes, which earned a rate-limit strike from the CDN and
turned an upstream outage into a self-inflicted one.

45s is short enough that a recovered channel is picked up quickly, and long
enough that a sustained outage cannot generate more than about one attempt per
provider per 45s.

Critically, this **does not touch `consecutive_fails` or the error EWMA**. The
host is healthy and must not be scored as broken, or it stays demoted in the
routing order long after its channels return.

---

## Request deadline

`REQUEST_DEADLINE_SECS = 480`, and it is a ceiling on the *whole* request
including every retry.

It must sit below the client's own timeout — most SDKs default to around 600s. An
earlier version allowed the ladder to run for hours, so the client gave up first
and the user saw a hang rather than an explanation. A readable terminal error in
time is strictly better.

The deadline is also checked **before each key attempt**, not only at the top of
a retry round. Without that, a round beginning with 100s left would run ~185s
past the deadline, and the final attempts would be handed budgets too small to
possibly succeed — 19s, then 5s, on a body needing 110s. Those are not retries,
they are guaranteed failures that also mark healthy keys slow.

---

## Battery

The target is ≤5%/hour on a tablet, which shapes several numbers that would
otherwise be set for freshness.

| setting | value | reasoning |
| --- | --- | --- |
| `UPTIME_PROBE_SECS` | 120 | uses the free `/v1/models` endpoint; never spends credit |
| `SWEEP_INTERVAL_SECS` | 30m | balances change slowly |
| `SWEEP_BATCH` / `CONCURRENCY` | 120 / 4 | gentle enough not to look like an attack, and to keep the radio from staying hot |
| `SWEEP_PACING_MS` | 120 | spreads a batch instead of bursting it |
| `STATE_FLUSH_SECS` | 20 | coalesced writes; flash wear is real |
| `LINK_POLL_SECS` | 60 | `dumpsys wifi` spawns a JVM and emits ~200KB. Link quality changes on the scale of walking across a room, so faster sampling buys nothing |

The release profile is part of this: `opt-level = "z"`, `lto = true`,
`codegen-units = 1`, `panic = "abort"`. A smaller resident set means less memory
pressure and less battery on a device with 8GB shared with everything else. It
costs several minutes per build, which is why CI runs the debug profile for tests
and pays for release only on a tag.

---

## Measuring it yourself

Reproducing the size ladder takes three payloads and a stopwatch. The essentials:

- **Send a browser User-Agent.** CDNs in front of these upstreams commonly block
  `curl/*` by UA. A 403 arrives in ~1.2s and looks exactly like a fast healthy
  response, which will invalidate everything you measure afterwards.
- **Use `stream: true`** so `time_starttransfer` is the response head rather than
  the completed generation.
- **Use distinct session identifiers** per payload, or session stickiness will
  pin them all to one key and serialise your test.
- **Run detached.** A 1M-token request outlives most tool timeouts.
- **Repeat the largest payload.** One reading cannot distinguish a slope from an
  outlier. That is exactly the mistake that produced the wrong 20.3 s/MB.

```sh
curl -sS -N -m 400 -o /dev/null \
  -w '%{http_code} ttfb=%{time_starttransfer}s total=%{time_total}s\n' \
  -A 'Mozilla/5.0 (X11; Linux aarch64) AppleWebKit/537.36 Chrome/128.0' \
  -H "Authorization: Bearer $KEY" -H 'content-type: application/json' \
  --data-binary @payload.json \
  https://provider.example.com/v1/chat/completions
```

To separate provider latency from gateway overhead, alternate direct and gateway
requests rather than running them in blocks — upstream conditions drift, and
alternating cancels most of that.
