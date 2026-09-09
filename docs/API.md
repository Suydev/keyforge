# API

Two surfaces on one port. The proxy surface (`/v1/...`) is what clients talk to;
the dashboard surface (`/api/...`) is read-mostly introspection.

Everything binds `127.0.0.1` and there is **no authentication**. The gateway holds
every key in memory, so exposing it to a network would be handing out the pool.
`BIND_ADDR` is a compile-time constant for that reason.

Anything not matching a route below returns a **local 404**. Unknown paths are
never forwarded upstream — an earlier Node proxy did forward them, which meant a
stray browser request to `/` was signed with a real API key and relayed to the
provider.

---

## Proxy surface

| method | path | shape |
| --- | --- | --- |
| POST | `/v1/messages` | Anthropic Messages |
| POST | `/v1/chat/completions` | OpenAI Chat Completions |
| GET | `/v1/models` | model list |
| * | `/v1/...` | anything else is passed through with the wire format inferred |

### Provider pinning

Prefix the path with a provider `id` to force it:

```
/provider-a/v1/messages           only provider-a
/provider-b/v1/chat/completions    only provider-b
/auto/v1/messages                  explicit auto-route (same as bare /v1)
```

Useful for isolating a provider when debugging. Note that pinning removes the
failover that makes the gateway worth running — a pinned request fails when that
provider fails.

### Authentication

Send anything, or nothing. The gateway replaces the client's credential with a key
from its own pool:

```sh
curl http://127.0.0.1:8787/v1/chat/completions \
  -H 'content-type: application/json' \
  -H 'authorization: Bearer unused' \
  -d '{"model":"claude-opus-5","messages":[{"role":"user","content":"hi"}]}'
```

For Anthropic-shaped clients:

```sh
export ANTHROPIC_BASE_URL=http://127.0.0.1:8787
export ANTHROPIC_API_KEY=unused
```

`anthropic-version` is injected when absent.

### Session identity

Requests carrying `x-session-id` (or `x-opencode-session`, `x-conversation-id`)
are grouped, and each session is pinned to one key. This matters when sibling keys
share a wallet — two sessions on sibling keys compete for one balance.

Without a header, identity is inferred from first-message content, with overlap
matching so a session survives client restarts and context compaction. Sending an
explicit header is more reliable and cheaper.

### What the client never sees

Retries. Headers are written exactly once, after a 2xx, so a response that took
twenty attempts across two providers is indistinguishable from one that took one.

That guarantee has a consequence worth knowing: **once headers are committed the
gateway cannot fail over**. A stream that dies mid-body cannot be retried onto
another key, because the client already has a 200 and a partial body.

### Terminal errors

When the gateway genuinely cannot succeed, it returns a body carrying *both* error
shapes so either SDK can parse it:

```json
{
  "type": "error",
  "error": {
    "type": "api_error",
    "code": "gateway_exhausted",
    "message": "tabi-gateway: gave up after 480s. Every provider was failing or every key was out of funds.",
    "detail": [
      "provider-a/sk-4vfx…CaQR: slow (timed out waiting for response head after 45s)",
      "provider-b: no funded key available"
    ]
  }
}
```

`detail` is the per-attempt trail. It is the fastest way to distinguish "every key
is broke" from "the provider has no backend" from "your model name is wrong".

The gateway **never emits a retryable status**. A 429 or 503 from an upstream is
handled internally, not relayed — relaying it would invite the client to retry
work the gateway is already retrying.

---

## Dashboard surface

### Read

| method | path | returns |
| --- | --- | --- |
| GET | `/` | the dashboard (vanilla JS, no build step) |
| GET | `/api/health` | `{"ok":true,"offline":false,"rev":N}` |
| GET | `/api/snapshot` | everything the dashboard needs, one object |
| GET | `/api/stream` | SSE; pushes only on change |
| GET | `/api/keys?provider=id` | per-key balances, masked |
| GET | `/api/session?fp=...` | one session in detail |
| GET | `/api/history?res=minute\|hour\|day` | traffic buckets |
| GET | `/api/uptime` | per-provider uptime samples |
| GET | `/api/errors` | recent failures, full detail |
| GET | `/api/inflight` | requests in flight right now |
| GET | `/api/leaderboard` | provider ranking with score components |
| GET | `/api/proxies` | egress pool health |
| GET | `/api/settings` | effective runtime settings |

`HEAD` is accepted on every static asset and returns the same headers as `GET`
with no body, per RFC 9110. Static assets are sent `no-store`: they are compiled
into the binary and carry no validator, so a merely-revalidating directive still
allowed a stale ES module to persist in the browser's module map across reloads.

### Write

| method | path | effect |
| --- | --- | --- |
| POST | `/api/keys/verify` | verify a pasted key; adopt it if funded |
| POST | `/api/keys/probe` | probe unmeasured keys now |
| POST | `/api/refresh` | trigger a free balance sweep |
| POST | `/api/proxies/toggle` | enable or disable the egress pool |
| POST | `/api/proxies/reload` | re-read the proxy file |
| POST | `/api/models/sync` | poll upstream model lists |

All writes are local-only operations on gateway state. None of them spends money:
key verification and probing use the provider's free `/v1/models` endpoint.

### `/api/health`

```json
{ "ok": true, "offline": false, "rev": 1047 }
```

`rev` increments on every state change, which makes it a cheap liveness *and*
activity check. `offline` is true when the device has no network at all — during
which requests are held rather than failed.

### `/api/snapshot`

One object, deliberately, so the dashboard renders from a consistent view rather
than stitching together endpoints that disagree.

```json
{
  "offline": false,
  "rev": 1047,
  "uptimeSecs": 138612,
  "activeSessions": 2,
  "workingSessions": 0,
  "providers": [ { "id": "provider-a", "alive": 863, "keys": 879,
                   "funds": 100611.0, "ttfbMs": 15657.0, "uptimePct": 99.4,
                   "requests": 701, "errors": 385, "cost": 288.04 } ],
  "models":   [ { "id": "claude-opus-5", "requests": 60, "avgMs": 52212 } ],
  "sessions": [ { "label": "ses_...", "provider": "provider-a",
                  "key": "sk-JoXF…cwF4", "turns": 11, "errors": 3 } ],
  "inflight": [ { "elapsedSecs": 157, "attempts": 4, "round": 1,
                  "phase": "awaiting Provider A" } ],
  "errorsLog":     [ "..." ],
  "errorsByClass": { "TIMEOUT": 26, "UPSTREAM": 39, "WAF": 113 },
  "minutes":       [ "..." ],
  "events":        [ "..." ],
  "totals":        { "requests": 340, "errors": 106, "cost": 311.44 }
}
```

Payload size is capped rather than unbounded: 120 uptime samples of 720 retained,
120 minute buckets, 40 errors, 24 sessions. A full history would dominate every
push and show up as UI lag. `/api/history` and `/api/uptime` serve the full series
when a chart needs it.

Aggregates are computed **before** the cap. `uptimePct` averages all 720 retained
samples, and `sessionsTotal` counts every tracked session rather than the 24 sent
— so no field silently comes to mean something narrower than its name.

Absolute timestamps accompany every derived duration (`started` beside
`elapsedSecs`, `lastSeen` beside any age). Server-computed durations freeze
between pushes, and the SSE stream only pushes when the revision moves, so a
client that displays them verbatim will show a stalled clock during exactly the
long waits that matter most. Recompute locally from the absolute value.

### `/api/errors`

The most useful endpoint when something is wrong. Each record:

```json
{
  "t": 1788461445,
  "class": "TIMEOUT",
  "provider": "provider-a",
  "host": "api.example.com",
  "key": "sk-3Vve…T3w1",
  "model": "claude-opus-5-thinking",
  "status": 0,
  "message": "timed out waiting for response head after 45s",
  "action": "next-key (breaker untouched)",
  "session": "sid:ses_...",
  "round": 1,
  "latencyMs": 45000,
  "reqBytes": 728064,
  "budgetSecs": 123,
  "ttfbMs": 45000,
  "headTimeout": true,
  "streaming": true,
  "proxy": "198.105.121.200:6462",
  "remaining": null,
  "required": null
}
```

Four fields carry most of the diagnostic weight:

- **`class`** — which of the eight error kinds this was, and therefore what the
  gateway did about it
- **`action`** — what it actually did, in words
- **`reqBytes` with `budgetSecs`** — the correlation that explains most timeout
  clusters. If `budgetSecs` is small while `reqBytes` is large, the budget was the
  problem, not the provider
- **`proxy`** — which egress served the attempt, so a systematically bad proxy is
  attributable

`remaining` and `required` are populated on quota refusals: the exact balance left
and the exact hold the model needed. Those two numbers are what let the gateway
stop guessing at holds.

### `/api/stream`

Server-sent events, pushing a full snapshot on change. Two properties that exist
for battery rather than elegance:

- pushes happen **only when state changes**, not on a timer
- the client closes the stream when the tab is hidden and reopens on return

An idle dashboard therefore costs approximately nothing.

---

## Wire format handling

The gateway is a proxy, not a translator. It does not convert between the
Anthropic and OpenAI shapes — a request arriving at `/v1/messages` goes upstream
as `/v1/messages`. What it does per-format:

| | Anthropic | OpenAI |
| --- | --- | --- |
| path | `/v1/messages` | `/v1/chat/completions` |
| injected header | `anthropic-version` if absent | — |
| stream terminator | `event: message_stop` | `data: [DONE]` |
| usage location | `message_delta` frame | final chunk |
| `usage.cost` | not present | present |

Because `usage.cost` is absent on the Anthropic path, spend on that surface is
reconciled from the balance sweep rather than read from the response. This is why
cost figures for Anthropic traffic can lag a few minutes behind reality.

---

## Practical notes

**Do not restart the gateway from a client that is routed through it.** You will
sever your own connection mid-request. `tabi status` first.

**A 400 with `gateway_exhausted` is a real answer, not a bug.** It means every
provider was failing or every key was out of funds for the full 480s deadline.
Check `detail` — the trail usually names the cause precisely.

**Watch `/api/inflight` when something feels stuck.** `attempts` and `round`
climbing with `elapsedSecs` tells you the ladder is working. `attempts` frozen
tells you it is waiting on one upstream.
