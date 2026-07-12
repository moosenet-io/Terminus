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
| `GET /*` (fallback) | `assets::WebAssets` embedded assets, or `ServeDir`/`ServeFile` when `CONSTELLATION_WEB_DIST_DIR` is set | the built `constellation-web` SPA — CONST-15: served from assets embedded in the binary by default; `CONSTELLATION_WEB_DIST_DIR` is now an optional filesystem dev override, not required for the UI to be served |

## Architecture notes

- **Auth is real (CONST-03).** `crate::constellation::auth` verifies the submitted login password
  against `CONSTELLATION_OPERATOR_SECRET` (constant-time compare, reusing
  `crate::pki::enroll::constant_time_eq`) and, on success, mints a signed session JWT via
  `crate::pki::enroll::mint_jwt_with_ttl` — the SAME `TERMINUS_JWT_SIGNING_KEY` HS256 signing
  primitive TCLI-02's enrollment JWT uses (see `docs/architecture/auth.md`), TTL from
  `CONSTELLATION_SESSION_TTL_SECONDS`. The cookie carries that JWT, not a plaintext value;
  `session_from_cookie` verifies its signature + expiry (`crate::pki::enroll::verify_jwt`) on every
  request. An unset `CONSTELLATION_OPERATOR_SECRET` fails every login attempt closed — never a
  default-allow. `crate::constellation::auth::require_session` is `axum` middleware layered (in
  `crate::constellation::mod`) over `/api/terminus/config` and the three proxied
  `/api/{harmony,chord,lumina}/*path` routes only: an unauthenticated request to any of those is
  rejected `401` before the handler runs (no backend dispatch). `/api/auth/{me,login,logout}` and
  `/api/health` stay reachable pre-auth (see `mod.rs`'s `public_router`/`protected_router` split).
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

New env vars (see `.env.example`'s "CONST-02"/"CONST-03" sections for the authoritative list with
defaults): `CONSTELLATION_HARMONY_URL`, `CONSTELLATION_CHORD_URL`, `CONSTELLATION_LUMINA_URL`,
`CONSTELLATION_WEB_DIST_DIR`, `CONSTELLATION_BACKEND_TIMEOUT_MS`, `CONSTELLATION_AUDIT_LOG_PATH`. None
of these are secret-shaped (they're backend base URLs and filesystem paths, not credentials), so —
matching this crate's existing convention (see `crate::config`'s module doc: terminus-rs has no
separate `SecretManager`/`vault::manager()` API; a plain env read of a runtime-materialized value IS
the vault read here) — they're plain `crate::config` helpers, not routed through a separate secret
store.

**CONST-03 adds:** `CONSTELLATION_SESSION_TTL_SECONDS` (non-secret, `crate::config`, default 3600)
and `CONSTELLATION_COOKIE_SECURE` (non-secret boolean, `crate::config`, default `false`) — plain
`crate::config` helpers like the rest of this section. `CONSTELLATION_OPERATOR_SECRET` (the login
credential) and `TERMINUS_JWT_SIGNING_KEY` (the session-signing key, shared with TCLI-02 enrollment)
are BOTH secret-shaped and are deliberately read directly via `std::env::var`/`env_nonempty` inside
`crate::constellation::auth`/`crate::pki::enroll` rather than a `crate::config` helper — same "plain
env read of a runtime-materialized value IS the vault read" convention, applied at the point of use.

## Testing

Unit tests live alongside each submodule (`src/constellation/{mod,proxy,mask,audit,auth}.rs`,
`#[cfg(test)] mod tests`): proxy routing + backend-down degradation, the mask negative property test,
`/api/terminus/config` shape (module-prefix derivation + worker count), and audit sanitization
(redaction + truncation + JSONL round-trip). Run via the project's standard `cargo test --workspace`
gate (Stage 4a) on the deployment host per this repo's build-host rule — the dev box OOMs on a
workspace compile, see `CLAUDE.md`.
