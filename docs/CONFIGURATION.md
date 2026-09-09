# Configuration

Three files and a handful of environment variables. Everything has a compiled-in
default, so an empty configuration is valid — the gateway will start, find no
keys, and tell you so.

| what | where | reloads live |
| --- | --- | --- |
| providers, hosts, routing weights | `~/.config/tabi/providers.json` | yes |
| API keys | wherever `keys_file` points | on restart |
| egress proxies | `~/proxies.txt` | on restart |
| runtime state (written by the gateway) | `~/.config/tabi/gateway-state.json` | — |
| tuning overrides | environment | on restart |

---

## Key files

One key per line. Blank lines and `#` comments are ignored; duplicates are
de-duplicated on load.

```
# ~/keys/provider-a-keys.txt
sk-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
sk-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
```

**Never modify a key string.** On `new-api` upstreams everything after the first
`-` can encode a channel pin, and the server parses it. Forward keys
byte-identically — do not split, normalise, or regenerate them.

At startup and on a slow sweep, each key's balance is probed via the provider's
**free** `/v1/models` endpoint, so an unfunded or revoked key is discovered
without spending anything. Startup prints the count and total known balance per
provider, which is the fastest way to confirm your files are being read:

```
tabi       859 keys   851 usable  $99140.90 known  (+0 new)
gorouter   887 keys   887 usable  $44106.60 known  (+0 new)
```

`keys_file` paths are relative to `$HOME`. A wrong path shows up as `0 keys`,
not as an error.

### Keeping keys safe

These files are frequently the only copy of something with real money on it.
`.gitignore` covers `*-keys.txt`, `proxies.txt`, `accounts.db`, and
`gateway-state.json` by pattern, so an accidental `git add -A` cannot stage them.
Note that `gateway-state.json` contains **every key in plaintext** alongside
balances — treat it exactly like the key files.

---

## providers.json

Written on first run. Editable while the gateway runs; changes are picked up
without a restart.

```json
{
  "version": 1,
  "providers": [
    {
      "id": "provider-a",
      "label": "Provider A",
      "hosts": [
        { "host": "api.example.com",  "enabled": true, "note": "primary" },
        { "host": "api2.example.com", "enabled": true, "note": "backup" }
      ],
      "keys_file": "keys/provider-a-keys.txt",
      "hold": 0.80,
      "initial_guess": 120.0,
      "enabled": true,
      "bias": 0.0,
      "note": "flat per-request hold, refundable"
    }
  ],
  "routing": { "...": "see below" }
}
```

### Provider fields

| field | meaning |
| --- | --- |
| `id` | short internal name; also the path prefix, so `/provider-a/v1/messages` forces this provider |
| `label` | display name for the dashboard |
| `hosts` | one or more hosts. The first enabled one is primary |
| `keys_file` | path to the key file, relative to `$HOME` |
| `hold` | per-request pre-deduction in dollars. **A starting guess, not a promise** — see below |
| `initial_guess` | assumed balance for a key not yet probed. Guess low |
| `enabled` | `false` removes the provider from routing without deleting its config |
| `bias` | manual routing nudge. Negative prefers this provider, positive avoids it |

**`hold` is learned, not trusted.** Upstreams that pre-deduct a hold report the
exact figure in the 403 they return when a key cannot cover it. The gateway
records that per `(provider, model)` and uses the observed value from then on.
Guessing low is therefore safe and self-correcting; guessing high strands money
on every key.

A new session will only be handed a key holding at least `hold × 2.5`
(`HOLD_SAFETY_FACTOR`), so a fresh conversation does not start on a key that dies
two turns in.

### Routing weights

```json
"routing": {
  "slow_multiplier": 2.0,
  "min_samples": 3,
  "error_weight": 4.0,
  "streak_weight": 0.5,
  "streak_halflife_secs": 120,
  "breaker_trip": 3,
  "breaker_backoff_secs": [15, 45, 120],
  "missing_model_penalty": 50.0,
  "probe_heals_score": true,
  "session_stickiness": true,
  "sticky_escape_multiplier": 4.0
}
```

| field | effect |
| --- | --- |
| `slow_multiplier` | how much measured latency counts against a provider's score |
| `min_samples` | latency readings required before latency influences routing. Stops one unlucky request setting policy |
| `error_weight` | how much the error-rate EWMA counts. Higher means faster abandonment |
| `streak_weight` | additional penalty per consecutive failure |
| `streak_halflife_secs` | how quickly a failure streak decays once things recover |
| `breaker_trip` | consecutive failures before a provider is skipped entirely |
| `breaker_backoff_secs` | escalating skip durations. Capped so recovery is never far away |
| `missing_model_penalty` | score penalty for a provider not known to serve the requested model |
| `probe_heals_score` | let a successful free probe clear accumulated penalty |
| `session_stickiness` | keep one conversation on one key. Load-bearing when sibling keys share a wallet |
| `sticky_escape_multiplier` | how much worse a sticky key must be before abandoning it |

---

## Egress proxies

`~/proxies.txt`, one endpoint per line. Three formats:

```
host:port:user:pass          # what most providers export
http://user:pass@host:port
host:port                    # unauthenticated
```

**Why this matters more than it looks.** Upstream rate limits are commonly
per-IP as well as per-account. With every request leaving one address, ten
separate accounts were rate-limited together — the shared factor was the IP, not
the wallet. Rotation is load-bearing, not cosmetic.

Selection is least-recently-used across healthy endpoints. A proxy that fails is
cooled down rather than dropped, so a transient blip does not permanently shrink
the pool. If every proxy is cooling, traffic goes **direct** — degraded egress
beats a failed request.

Override the path with `KEYFORGE_PROXIES`, or disable the pool entirely with
`KEYFORGE_NO_PROXY=1`.

---

## Environment variables

Every one of these has a default. Set them only to change something.

### Server

| variable | default | meaning |
| --- | --- | --- |
| `KEYFORGE_PORT` | `8787` | listen port. Always binds `127.0.0.1` — never a public interface |
| `KEYFORGE_STATE` | `~/.config/tabi/gateway-state.json` | alternate state file, for running a second instance |
| `KEYFORGE_PROXIES` | `~/proxies.txt` | proxy list location |
| `KEYFORGE_NO_PROXY` | unset | `1` disables the pool; all traffic goes direct |

A PID lockfile beside the state file prevents two instances from sharing one
file. If you want a test instance, give it its own `KEYFORGE_STATE` **and** its own
`KEYFORGE_PORT`.

### Streaming head budget

How long to wait for an upstream's response head before moving to the next key.
The full derivation, with the measurements behind each number, is in
[TUNING.md](TUNING.md) — do not change these by guesswork.

```
budget = clamp( ewma_ttfb × MULT + (SECS_PER_MB × req_MB + OVERHEAD) × SAFETY,
                MIN, MAX )
```

| variable | default | meaning |
| --- | --- | --- |
| `KEYFORGE_HEAD_MULT` | `3.0` | multiplier on the provider's measured TTFB |
| `KEYFORGE_HEAD_MIN_SECS` | `45` | floor, so a fast provider fails over fast |
| `KEYFORGE_HEAD_MAX_SECS` | `170` | ceiling. Near Cloudflare's edge timeout; beyond it you are waiting on nothing |
| `KEYFORGE_HEAD_DEFAULT_SECS` | `120` | base term for a provider with no measurements yet |
| `KEYFORGE_HEAD_SECS_PER_MB` | `35.2` | measured seconds of prefill + upload per MB of body |
| `KEYFORGE_HEAD_OVERHEAD_SECS` | `15.0` | the gateway's own buffer-and-forward cost |
| `KEYFORGE_HEAD_SIZE_SAFETY` | `1.35` | safety factor on the size term |

### Cooldowns and polling

| variable | default | meaning |
| --- | --- | --- |
| `KEYFORGE_NO_CHANNEL_COOLDOWN_SECS` | `45` | how long to skip a provider that reported "no available channel" |
| `KEYFORGE_LINK_POLL_SECS` | `60` | Wi-Fi sampling interval (Android only) |

---

## Constants that are not configurable

These are in `src/config.rs` with the reasoning attached. They are compile-time
because changing them safely requires understanding *why* they are what they are,
and an environment variable invites changing them without that.

| constant | value | why this value |
| --- | --- | --- |
| `REQUEST_DEADLINE_SECS` | 480 | must sit below the client's own timeout (~600s for most SDKs), or the client gives up first and the user sees a hang instead of a readable error |
| `MAX_KEY_ATTEMPTS` | 6 | keys tried on one provider before failing over to its sibling |
| `HOLD_SAFETY_FACTOR` | 2.5 | balance multiple required to hand a key to a *new* session |
| `COOLDOWN_QUOTA_SECS` | 6h | balances get topped up, so an exhausted key is worth retrying |
| `COOLDOWN_AUTH_SECS` | 7d | an invalid key is effectively permanent |
| `COOLDOWN_RATE_SECS` | 60 | overridden by `Retry-After` when the upstream sends one |
| `SESSION_ACTIVE_SECS` | 45m | how long a session owns its key exclusively |
| `MAX_BODY_BYTES` | 32 MB | OOM guard on a memory-constrained device |
| `UPSTREAM_TIMEOUT_SECS` | 300 | non-streaming requests, where the head only arrives after full generation |
| `SWEEP_BATCH` / `SWEEP_CONCURRENCY` | 120 / 4 | deliberately gentle: enough to keep balances fresh, not enough to look like an attack |

---

## Runtime state

`~/.config/tabi/gateway-state.json` — written by the gateway, not for hand
editing. Holds keys, balances, per-provider statistics, sessions, history
buckets, and the recent error log. Writes are coalesced (`STATE_FLUSH_SECS = 20`)
because flash wear is a real constraint on a tablet.

It is forward-compatible: unknown fields are ignored and missing ones take
defaults, so upgrading does not require migration. A corrupt file is renamed
aside rather than silently replaced, so the data can be inspected.

**It contains every key in plaintext.** Same handling as the key files.

---

## Verifying a configuration change

```sh
tabi restart && tabi status
```

`tabi status` prints per-provider key counts, usable counts, funds, measured
latency, and uptime. If a provider shows `0 keys`, the `keys_file` path is wrong.
If it shows keys but `0 usable`, they are unfunded or revoked — the dashboard's
key tab shows per-key detail.
