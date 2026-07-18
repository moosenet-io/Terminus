## Constellation aggregation API (CONST-02)

A compiled-in module (`crate::constellation`, `src/constellation/`) — merged into the same
`axum::Router` `/mcp` and the inference-proxy routes use, **not** a broker worker (see
[`docs/architecture/broker.md`](docs/architecture/broker.md)) — serving the `constellation-web`
control-plane UI's backend surface at the same origin:

- `GET/POST /api/auth/{me,login,logout}` — real signed-session auth (CONST-03): `login` verifies the
  submitted password against `CONSTELLATION_OPERATOR_SECRET` (constant-time compare, fail-closed if
  unset) and, on success, sets an HttpOnly session cookie carrying a `TERMINUS_JWT_SIGNING_KEY`-signed
  JWT (the same HS256 signing primitive TCLI-02's enrollment JWT uses, `crate::pki::enroll`); `me`/the
  proxy handlers verify that JWT's signature + expiry, never trust the cookie's raw value. See
  `src/constellation/auth.rs`'s module doc for the exact verification path.
- `GET /api/health` — per-system reachability (`harmony`/`chord`/`lumina`/`terminus`) — public,
  unauthenticated (read-only liveness).
- `GET /api/terminus/config` — the compiled-in tool registry's module list + broker worker count —
  **requires a valid session** (CONST-03 guard).
- `* /api/{harmony,chord,lumina}/*path` — namespaced backend proxies (`src/constellation/proxy.rs`):
  the single door this crate forwards browser requests to those three backends through, degrading a
  down/unconfigured backend to a structured `available:false` response rather than a `500` cascade.
  **Requires a valid session** (CONST-03 guard) — an unauthenticated request is rejected `401` before
  any backend dispatch.
- `GET /ws` — scaffolded only (`501`); the full live-event WebSocket relay is a follow-up item.
- The built `constellation-web` static bundle, served as a SPA fallback when
  `CONSTELLATION_WEB_DIST_DIR` is configured.

Every `/api/*` response is secret-masked before egress (`src/constellation/mask.rs` — the layer's
load-bearing security property) and every mutating request is S6-sanitized and audit-logged
(`src/constellation/audit.rs`). See
[`docs/constellation/CONST-02-aggregation-api.md`](docs/constellation/CONST-02-aggregation-api.md)
for the full endpoint contract, architecture notes, and new `.env.example` config keys.

