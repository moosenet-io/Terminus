# CONST-02 — Constellation Aggregation API Layer

**Spec:** S97-constellation-gui (v3.22) · **Item:** CONST-02 · **Depends on:** CONST-01 (adaptation
plan, `docs/constellation/CONST-01-adaptation.md`) · **Consumed by:** CONST-04
(`constellation-web/src/lib/aggregationClient.ts` `httpAdapter`) and every downstream panel item.

## What this is

The Terminus-side half of the "browser only ever talks to Terminus" control-plane model: a
**compiled-in module** (`crate::constellation`, `src/constellation/`) merged into the existing
`axum::Router` in `crate::mcp_server::build_router` — deliberately **not** a broker worker. The
broker (`docs/architecture/broker.md`) exists to extract MCP **tool domains** onto an out-of-process
UDS/mTLS transport; this layer is an operator-facing HTTP API + static-asset host with no tool
domain to extract, so it is added as plain routes, exactly like the existing `/mcp` and
inference-proxy routes already are.

## Endpoint surface

Pinned to (and unit-tested against) `constellation-web/src/lib/aggregationClient.ts`'s `httpAdapter`
contract:

| Route | Handler | Notes |
|---|---|---|
| `GET /api/auth/me` | `constellation::auth::auth_me` | `{authenticated, username}` |
| `POST /api/auth/login` | `constellation::auth::auth_login` | body `{username,password}`; sets a session cookie |
| `POST /api/auth/logout` | `constellation::auth::auth_logout` | clears the session cookie |
| `GET /api/health` | `constellation::handle_health` | `{system,available,detail?}[]` for harmony/chord/lumina/terminus |
| `GET /api/terminus/config` | `constellation::handle_terminus_config` | `{modules:[{name,enabled,version?}],workerCount}` |
| `* /api/harmony/*path` | `constellation::proxy::proxy_harmony` | namespaced passthrough |
| `* /api/chord/*path` | `constellation::proxy::proxy_chord` | namespaced passthrough |
| `* /api/lumina/*path` | `constellation::proxy::proxy_lumina` | namespaced passthrough |
| `GET /ws` | `constellation::handle_ws_stub` | scaffold only — `501`, real relay is a follow-up item |
| `GET /*` (fallback) | `ServeDir`/`ServeFile` | the built `constellation-web` SPA, when `CONSTELLATION_WEB_DIST_DIR` is set |

## Architecture notes

- **Auth is a seam, not a gate (yet).** `crate::constellation::auth` ships only the minimum shape
  CONST-04's UI needs to build/run against. It accepts any non-empty username/password (no hardcoded
  credential — there is nothing real yet to check against) and sets an **unsigned** cookie. It does
  **not** enforce access control on `/api/*` today. CONST-03 replaces the cookie with a verified
  JWT/session (reusing this crate's existing `crate::pki` enrollment machinery, see `docs/architecture/auth.md`)
  and is expected to make mutating requests actually fail closed without an authenticated principal.
  Every `// CONST-03:` comment in `src/constellation/auth.rs` marks where that verification plugs in.
- **Proxy = the single door to Harmony/Chord/Lumina.** `crate::constellation::proxy` is the only place
  in this crate that forwards a browser request to one of those three backends' own HTTP APIs — no
  other module should grow a second ad-hoc client for one of them (mirrors S9's single-access-path
  principle, applied to these backend surfaces instead of GitHub/Gitea/Plane).
- **Graceful degradation, never a 500 cascade.** A backend that is unconfigured, unreachable, or too
  slow returns `200 OK` with `{"system":s,"available":false,"detail":reason}` — the same shape
  `/api/health` reports per-system — so one system's panel going down never breaks the others.
- **Masking is unconditional.** Every `/api/*` response body — local or proxied — passes through
  `crate::constellation::mask::mask_response` before it leaves this process. This is the load-bearing
  security property of the layer: see that module's doc for the fail-closed masking rule and its
  negative property test (`negative_property_no_planted_secret_survives_in_serialized_output`).
- **Every mutating request is audited.** `crate::constellation::audit` appends an S6-sanitized JSONL
  line (reusing `crate::gateway_framework::audit::sanitize`'s redaction regexes) for every
  `POST`/`PUT`/`PATCH`/`DELETE` through `/api/*`, to the path configured by
  `CONSTELLATION_AUDIT_LOG_PATH`.
- **`/ws` is scaffolded, not implemented.** `constellation-web`'s live event stream (engine/ralph-loop/
  log/tree_update events, ported from harmony-web's `useWebSocket`) needs a real same-origin,
  session-authenticated WebSocket relay to Harmony's own event source. That is out of this item's HTTP
  aggregation scope — `GET /ws` currently returns a clean, typed `501` instead of a bare `404` so the
  client fails predictably. Tracked as a follow-up (see the `// CONST-*:` note in
  `src/constellation/mod.rs`).

## Configuration

New env vars (see `.env.example`'s "CONST-02" section for the authoritative list with defaults):
`CONSTELLATION_HARMONY_URL`, `CONSTELLATION_CHORD_URL`, `CONSTELLATION_LUMINA_URL`,
`CONSTELLATION_WEB_DIST_DIR`, `CONSTELLATION_BACKEND_TIMEOUT_MS`, `CONSTELLATION_AUDIT_LOG_PATH`. None
of these are secret-shaped (they're backend base URLs and filesystem paths, not credentials), so —
matching this crate's existing convention (see `crate::config`'s module doc: terminus-rs has no
separate `SecretManager`/`vault::manager()` API; a plain env read of a runtime-materialized value IS
the vault read here) — they're plain `crate::config` helpers, not routed through a separate secret
store.

## Testing

Unit tests live alongside each submodule (`src/constellation/{mod,proxy,mask,audit,auth}.rs`,
`#[cfg(test)] mod tests`): proxy routing + backend-down degradation, the mask negative property test,
`/api/terminus/config` shape (module-prefix derivation + worker count), and audit sanitization
(redaction + truncation + JSONL round-trip). Run via the project's standard `cargo test --workspace`
gate (Stage 4a) on the deployment host per this repo's build-host rule — the dev box OOMs on a
workspace compile, see `CLAUDE.md`.
