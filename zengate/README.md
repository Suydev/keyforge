# zengate — zen free-tier egress tooling

`opencode.ai` (OpenCode Zen) enforces its free tier with a **per-egress-IP**
usage budget. Measured on a phone, not assumed:

| Attempt | Result |
|---|---|
| Fresh `ses_`/`msg_`, device IP | `429 FreeUsageLimitError`, `retry-after: 55354` (~15.4 h) |
| Fresh `ses_`/`msg_`, a different proxy IP | `429`, `retry-after: 53982` |
| Fresh ids across 40 rotating proxy IPs | **10 × `200` streaming a real pong**, 2 × `429`, 28 dead |

So rotating the session id alone changes nothing — the limiter is keyed on the
source address. What helps is a steady supply of verified egress IPs.

## poolkeeper.mjs — the background harvester

Runs on a timer (default 30 min) and:

1. pulls several public proxy lists (~9k candidates),
2. screens them for a CONNECT tunnel to `opencode.ai:443` (20 workers),
3. **proves survivors with a real zen chat pong** — full fingerprint (`ses_`/
   `msg_`/`ua`/`cli`/`project`) plus the four gate tools `bash`/`glob`/`grep`/
   `read`. Nothing is accepted on a `/v1/models` 200,
4. keeps only pongs faster than `MAX_TTFB_MS`, merged with the existing pool and
   ranked fastest-first,
5. writes `proxies.txt` atomically (temp + rename, so the gateway never reads a
   half-written file) and hot-reloads keyforge via `POST /api/proxies/reload`.

Free proxies rot within minutes — one load of 32 shed 17 dead inside 20 minutes
— which is exactly why this runs on a loop instead of once.

### Run

```sh
node zengate/poolkeeper.mjs
```

| Env | Default | Meaning |
|---|---|---|
| `POOL_FILE` | `~/git_gorouter_keyforge-token/proxies.txt` | pool written for keyforge |
| `KEYFORGE_URL` | `http://127.0.0.1:8787` | gateway to hot-reload |
| `INTERVAL_MIN` | `30` | minutes between cycles |
| `MAX_TTFB_MS` | `4000` | discard pongs slower than this |
| `CONNECT_W` / `VET_W` | `20` / `40` | concurrency per stage |
| `POOL_LOG` | `~/tmp/poolkeeper.log` | log file |

## gate.mjs — standalone gateway

A self-contained zen gateway for when you don't want keyforge in the path:
same pool discipline, one session pinned to one IP, and on zen's 429 it cools
*that IP* for the upstream's real `retry-after`, rolls the session id, and
rotates — while keeping the request alive.

```sh
node zengate/gate.mjs          # :8790
curl localhost:8790/__pool     # pool state, TTFB-ranked
curl localhost:8790/__health
```

## Why keyforge also owns this logic

The gateway needs the same behaviour in-process (it is the router for every
provider). `keyforge`'s Rust implementation is the production path; these
scripts exist because the Rust build is slow on-device and the pool needs
refreshing on a wall-clock schedule regardless. See
`docs/ARCHITECTURE-free-tier.md` in the parent directory for the full reasoning.
