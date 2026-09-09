# Troubleshooting

Start here:

```sh
tabi status                                    # is it up, and what does it think?
curl -s localhost:8787/api/health              # {"ok":true,"offline":false,"rev":N}
curl -s localhost:8787/api/errors | head -c 2k # what actually failed
tabi log                                       # follow the log
```

The error log is the single most useful artifact. Four fields carry most of the
diagnosis: `class` (what kind of failure), `action` (what the gateway did),
`reqBytes` with `budgetSecs` (whether the timeout was the problem), and `proxy`
(which egress served it).

---

## Reading the error classes

| `class` | what it actually means | your move |
| --- | --- | --- |
| `QUOTA` | key out of money; upstream reported the exact remainder | none — key retired 6h, pool self-heals |
| `AUTH` | key invalid or revoked | remove it from the key file |
| `RATE` | rate limited. **Often per-IP, not per-key** | check egress rotation |
| `TIMEOUT` | slow, not dead. Includes 524 | check `reqBytes` vs `budgetSecs` |
| `WAF` | CDN challenge — your egress, not the key | check User-Agent and proxies |
| `NOCHANNEL` | provider has no backend for this model | wait; nothing you can do |
| `UPSTREAM` | genuine 5xx or connection reset | wait, or check the provider's status page |
| `OFFLINE` | the device has no network | check the device, not the provider |
| `TRANSPORT` | connection died before any response | usually local network or proxy |

---

## "Every request times out on large contexts"

**Symptom.** Small requests work. Large ones fail on every key, on every provider,
repeatedly, and the gateway gives up after ~480s.

**Check:**

```sh
curl -s localhost:8787/api/errors | python3 -c '
import json,sys
for e in json.load(sys.stdin)[:10]:
    print(f"{e[\"class\"]:9} req={(e.get(\"reqBytes\") or 0)//1024:>5}KB budget={e.get(\"budgetSecs\")}s")'
```

**If `reqBytes` is large while `budgetSecs` is small, the budget is the cause.**
A 2.7MB body needs ~110s through the gateway; a 45s budget cannot serve it on any
key, ever. This is not slowness, it is arithmetic.

**Fix.** Raise the size term:

```sh
KEYFORGE_HEAD_SECS_PER_MB=35.2 KEYFORGE_HEAD_MAX_SECS=170 tabi restart
```

Those are the measured defaults. If your link is slower than the one they were
measured on, raise `SECS_PER_MB`. Do not raise `MAX_SECS` much past ~170 — the CDN
in front of most upstreams answers for the origin at 128-140s, so beyond that you
are waiting for a response that will never arrive. See [TUNING.md](TUNING.md).

**Also check whether the context should be that large.** If the client is an agent,
it may have grown its context without bound because no context limit was declared.
Declaring one lets it compact instead.

---

## "It says every provider failed, but their sites are up"

**Symptom.** `gateway_exhausted` after 480s. The provider's web dashboard loads
fine.

**Check the message body, not just the status:**

```sh
curl -s localhost:8787/api/errors | grep -o '"message":"[^"]*"' | sort -u | head
```

**`no available channel` or `all nodes exhausted` means the site is up and the
backend is gone.** The relay answered correctly; it simply has nothing to route to.
Every key returns the identical answer, so nothing you change locally helps.

The gateway cools that provider for 45s rather than cycling keys — because cycling
keys against a backend outage once produced ~57 attempts in 8 minutes and earned a
CDN rate-limit strike, turning their outage into an additional local problem.

**Your move: wait.** Watch `tabi status` — uptime recovers on its own.

---

## "429 rate limited, but I have hundreds of keys"

**These limits are usually per-IP and per-account, not per-key.** Rotating keys
adds no headroom at all; rotating egress IPs does.

**Check whether rotation is actually happening:**

```sh
curl -s localhost:8787/api/errors \
  | grep -o '"proxy":"[^"]*"' | sort | uniq -c | sort -rn
```

Mostly `"proxy":""` means requests went **direct** — one IP for everything, which
is exactly the shape that produces per-IP limits across many accounts.

```sh
curl -s localhost:8787/api/proxies     # pool health, what is cooling
```

**Causes of unexpected direct egress:**

- no `~/proxies.txt`, or every line malformed
- `KEYFORGE_NO_PROXY=1` set
- every proxy cooling after failures — direct is the deliberate fallback
- **connections being reused.** hyper's pool keys by `(scheme, host, port)` and
  does not know which proxy tunnelled a socket, so warm connections are reused
  across proxies. Rotation is real but weaker than it looks.

---

## "403 Forbidden on every request"

Two very different causes, distinguished by the response body.

**HTML body mentioning a CDN** — a WAF block. Almost always the User-Agent. Many
CDNs in front of these upstreams block `curl/*` outright. The gateway sends a
browser UA, so this usually only affects your own direct testing:

```sh
# 403 — blocked by UA
curl -H "Authorization: Bearer $KEY" https://provider.example.com/v1/models

# 200 — same key, same host
curl -A 'Mozilla/5.0 (X11; Linux aarch64) AppleWebKit/537.36 Chrome/128.0' \
     -H "Authorization: Bearer $KEY" https://provider.example.com/v1/models
```

Worth knowing because a UA-blocked 403 arrives in ~1.2s and looks exactly like a
fast, healthy response. It will silently invalidate any measurement taken after it.

**JSON body** — a real authorisation refusal. Check the model name first: a
provider-prefixed name like `gateway/claude-opus-5` or `provider-a/claude-opus-5`
is a client misconfiguration. The prefix belongs in the *path*
(`/provider-a/v1/messages`), never in the JSON `model` field.

---

## "The gateway died when I restarted it"

You restarted it from a client that was routed through it, and cut your own
connection.

```sh
tabi status     # always check first
```

If the client you are working in points at `127.0.0.1:8787`, either switch it to a
direct provider first or accept the disconnect. A launcher wrapper that auto-starts
the gateway will recover it on the next invocation.

---

## "Zero keys at startup"

```
provider-a   0 keys   0 usable   $0.00 known
```

`keys_file` paths are **relative to `$HOME`**, and a wrong path is not an error —
it is an empty file list.

```sh
cat ~/.config/tabi/providers.json | grep keys_file
ls -la ~/keys/                       # does the file exist?
wc -l ~/keys/provider-a-keys.txt     # one key per line?
```

Keys must be byte-identical to what the provider issued. On `new-api` upstreams
everything after the first `-` can encode a channel pin that the server parses —
never split, normalise, or regenerate a key.

---

## "Keys show as alive but every request says out of funds"

The configured `hold` is too low, so keys pass the balance filter and then fail the
upstream's pre-deduction check.

This self-corrects: the 403 reports the exact hold required, and the gateway records
it per `(provider, model)` and uses the observed value from then on. If it is not
correcting, the refusal is being misclassified — check whether those errors appear
as `QUOTA` (correct, learning) or as `CLIENT` (wrong, learning nothing):

```sh
curl -s localhost:8787/api/errors | grep -o '"class":"[^"]*"' | sort | uniq -c
```

A quota refusal landing in `CLIENT` means the wording did not match any known
phrase. Those refusals are localised and version-dependent, and the phrase lists
in `classify.rs` are the fix.

---

## "Requests hang with no error"

```sh
curl -s localhost:8787/api/inflight
```

- **`attempts` and `round` climbing with `elapsedSecs`** — the ladder is working.
  It will return within 480s either way.
- **`attempts` frozen** — waiting on one upstream. Compare `elapsedSecs` against
  `budgetSecs` from the errors log.
- **`inflight` empty but your client is waiting** — the request never reached the
  gateway. Check the client's base URL and port.

A completely silent hang past 480s should be impossible: the deadline exists
specifically to return a readable error before the client's own timeout fires.

---

## "Uptime shows a provider down when it is fine"

Check whether **all** providers dipped simultaneously:

```sh
curl -s localhost:8787/api/uptime | head -c 500
```

Three simultaneous outages means the local network dropped, not three providers.
The uptime prober requires consecutive failures before drawing a provider down, and
discards samples when everything fails at once — but a brief local blip can still
leave a mark.

`OFFLINE` in the error log confirms it: that class is only used when DNS or routing
failed entirely, which is a local condition.

---

## "Cost figures look wrong"

Two known reasons:

**Anthropic-path spend lags.** `usage.cost` is absent on `/v1/messages` responses,
so spend on that surface is reconciled from the balance sweep rather than read from
the response. It can trail by minutes.

**Pre-deduction holds are refunded.** Many upstreams reserve pessimistically and
refund the unused portion. Mid-flight figures show the reservation, not the
settled cost.

```sh
curl -X POST localhost:8787/api/refresh    # force a free balance sweep
```

---

## Build and install problems

| symptom | cause and fix |
| --- | --- |
| `linker cc not found` | no C compiler. `pkg install clang` / `apt install build-essential` / `xcode-select --install` |
| build killed partway | out of memory. `cargo build --release -j1`. On a tablet, plug in first — release builds pin every core for minutes |
| `another keyforge is already using ...` | the PID lockfile doing its job. `tabi stop`, or use a separate `KEYFORGE_STATE` for a second instance |
| `port 8787 is held by something that is not the gateway` | a squatter. `tabi restart` replaces it |
| clippy fails on a fresh clone | Rust version skew — some lints are version-dependent. Check your `rustc -V` against CI's |

---

## Collecting information for a bug report

```sh
{
  echo "=== version ==="; git -C ~/keyforge rev-parse --short HEAD
  echo "=== rustc ==="; rustc -V
  echo "=== health ==="; curl -s localhost:8787/api/health
  echo "=== providers ==="; tabi status
  echo "=== errors by class ==="
  curl -s localhost:8787/api/errors | grep -o '"class":"[^"]*"' | sort | uniq -c
  echo "=== recent errors ==="; curl -s localhost:8787/api/errors | head -c 4000
  echo "=== log tail ==="; tail -n 60 ~/tmp/keyforge.log
} > report.txt
```

**Redact before sharing.** Keys are masked in the API output, but the log and the
error `message` fields can contain full URLs and upstream responses. Check
`report.txt` for `sk-` before posting it anywhere.
