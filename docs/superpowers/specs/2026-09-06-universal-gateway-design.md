# Universal Gateway Customization — Design

Date: 2026-09-06
Status: Approved in chat (sections 1–5)
Repo: `~/keyforge` (Rust, not a git repo; token-patch workflows assume the
repo will be git-initialised later this cycle)

## Goal

Turn the gateway from four hardcoded providers + const/env tuning into a
**universal multi-provider gateway** where any OpenAI/Anthropic-compatible host
can be added, edited, and deleted from the dashboard, and every tunable is
customizable live — without rebuilding or editing files.

## Non-goals

- Changing the core request pipeline semantics (key-pool rotation, failover
  ladder, "never emit a retryable error").
- Network exposure: still loopback-only (`BIND_ADDR` stays fixed).
- Dashboard auth: loopback binding remains the security boundary.
- Rewriting `router.rs`, `state.rs`, `classify.rs` internals beyond what the
  data model change requires.

## Section 1 — Universal provider model

Replace the hardcoded `config::providers()` list and `&'static str` provider
ids with an owned, persisted `ProviderCfg`:

```
ProviderCfg {
  id: String,                     // stable id used in routes/state/dashboard
  label: String,
  hosts: Vec<HostCfg> { host, enabled, note },   // primary first, backups after
  formats: Vec<String>,           // subset of ["anthropic","openai"]
  auth: AuthCfg { style, header_name?, value_prefix? },
  keys_file: Option<String>,      // optional external file (current mechanism)
  hold: f64, initial_guess: f64,  // pricing
  enabled: bool, bias: f64, note: String,
}
```

`AuthCfg`:

- `bearer`  → `Authorization: Bearer <key>` (default, matches today)
- `x-api-key` → `x-api-key: <key>` (TokenForge style)
- `custom` → `header_name: <value_prefix><key>`
- `none` → no auth header

`formats` gates which client-request flavors may be routed to this provider.
`["anthropic","openai"]` means both, matching new-api hosts. `keys_file`
remains optional; pasted keys live in `gateway-state.json`, keyed by provider,
as today. The binary ships **no enforced provider list** — only a first-boot
seed (Section 5).

## Section 2 — TuningCfg: every constant becomes editable

A persisted `tuning` block in `providers.json` holds every currently-`const` or
`env`-read tunable:

- Head budgets: `head_mult`, `head_min_secs`, `head_max_secs`, `head_default_secs`,
  `head_secs_per_mb`, `head_overhead_secs`, `head_size_safety`
- Timeouts: `upstream_timeout_secs`, `request_deadline_secs`, `max_body_bytes`,
  `probe_timeout_secs`, `client_body_timeout_secs`
- Cooldowns: `no_channel_cooldown_secs`, `cooldown_quota_secs`,
  `cooldown_auth_secs`, `cooldown_rate_secs`
- Key/session: `max_key_attempts`, `hold_safety_factor`, `session_active_secs`,
  `usage_stale_secs`
- Uptime/link: `uptime_probe_secs`, `uptime_retry_secs`, `uptime_fail_threshold`,
  `link_poll_secs`
- Sweep: `sweep_interval_secs`, `sweep_batch`, `sweep_concurrency`,
  `sweep_pacing_ms`
- Proxy: `proxy_maint_secs`, `proxy_vet_batch`, `proxy_vet_concurrency`,
  `proxy_vet_timeout_secs`
- Misc: `state_flush_secs`, `analysis_every_failures`, `keysync_interval_secs`

Precedence: **settings file wins → env var → compiled default**. The existing
`config::head_mult()`-style getter functions stay (no call-site churn); they
read from a runtime `Tunables` store seeded at boot and hot-updated by the
dashboard. Env vars remain an escape hatch when a field is absent.

## Section 3 — Dashboard "Customize" tab + API

New tab `Customize` with four sub-sections: **Providers, Routing, Tuning, Keys**.

Endpoints:

- `GET /api/providers` — config + live health per provider
- `POST /api/providers` — create (optional key-verify before save)
- `PUT /api/providers/{id}` — edit hosts/auth/pricing/enabled/bias/note
- `DELETE /api/providers/{id}` — delete; refused while sessions are in-flight;
  confirmed delete asks what to do with the provider's keys
- `POST /api/providers/{id}/keys` — paste one key, verify, adopt
- `PUT /api/settings` — update `routing` + `tuning` atomically

Every write: atomic temp-file + rename (existing `Settings::save` pattern),
then hot-apply + broadcast an SSE event. `RoutingCfg` is already read live per
request in `state.rs`; `TuningCfg` updates the `Tunables` store so getters pick
it up without a restart.

## Section 4 — Routing + auth changes

- **Flavor-aware routing**: `router.rs` provider selection additionally
  requires `provider.formats` to include the request flavor. Anthropic
  `/v1/messages` never lands on OpenAI-only hosts; OpenAI never lands on
  Anthropic-only hosts. Current four providers declare both → unchanged.
- **Per-provider auth in request path**: `build_request` drops the hardcoded
  dual-header injection and uses each provider's `AuthCfg`. `send_head`/`send`
  and proxy vetting (`vet_proxy`) carry the auth spec through. Free probes
  (`/v1/models`, billing) use the provider's auth too.
- Forced-prefix routes `/tabi/v1/...` become dynamic, generated from configured
  provider ids.

## Section 5 — Migration, call-site churn, testing

### Migration

On boot:

1. If `providers.json` has no `tuning` block, write defaults into the file.
2. If the provider list is empty (fresh run), seed it with the current four
   providers (tabi, gorouter, justwoker, kktoken) converted to the new schema,
   so nothing is lost on upgrade.

### Call-site churn

- `&'static str` provider ids → owned `String` across `state.rs`, `router.rs`,
  `api.rs`, `modelsync.rs`, `main.rs`.
- `App.providers` resolves from settings instead of `config::providers()`.
- Wire the dormant settings hot-reload (`settings_mtime` backlog item).
- Dynamic forced-prefix route matching in `main.rs::is_api_path`.

### Testing

Extend the existing suite (currently 167 tests, must stay green):

- Settings round-trip (providers + routing + tuning serialize/deserialize).
- Migration: fresh file → seeded; legacy file → tuning + new schema.
- Auth header builder: each style (bearer / x-api-key / custom / none).
- Flavor-deny routing: OpenAI-only provider not chosen for `/v1/messages`.
- Dashboard CRUD: add → list → edit → delete; delete-with-inflight refused.
- `cargo build --release` clean; `cargo test --release` green.

## Deployment notes

- `tabi restart` during implementation is allowed ONLY when this session is not
  routed through the gateway; otherwise first switch this session to a direct
  provider or accept a disconnect.
- State and config must survive restart: `providers.json` hot-applied, keys
  preserved in `gateway-state.json`.