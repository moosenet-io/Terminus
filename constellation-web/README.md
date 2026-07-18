# constellation-web

The Lumina Constellation control-plane UI (spec `S97-constellation-gui`, item **CONST-04**).
A React 18 / TypeScript 5 / Vite 5 single-page app, adapted from `harmony/harmony-web`
(see `docs/constellation/CONST-01-adaptation.md` in the CONST-01 worktree for the full
inventory + reuse map this was built from).

This item ships the **shell only**: layout, auth, the aggregation client, and the module
registry, plus one example panel (Terminus config). The other system panels (CONST-05..12)
are separate build items that register into the same registry — the shell does not change
when they land.

## Two patterns everything else builds on

### 1. The aggregation client (`src/lib/aggregationClient.ts`)

This is the **only** module in the app allowed to call `fetch` or read `window.location`.
Every hook, panel, or component that needs backend data goes through the exported
`getAggregationClient()` singleton — never `fetch` directly. This keeps the browser's only
network surface to same-origin `/api/{harmony,chord,lumina,terminus}/*` calls, cookie-based
(`credentials: 'include'`), no hardcoded hosts.

It has two implementations of the same `AggregationClient` interface:

- **`mockAdapter`** — canned, in-memory data. Default. Lets the whole app build, run, and be
  reviewed with zero backend present.
- **`httpAdapter`** — real fetch against `/api/...`, the same origin the SPA is served from.

Selected via the `VITE_AGG_MODE` env var (`mock` | `http`), default `mock`. Swapping to a
real backend is a build-time env change, not a code change.

**Endpoints/shapes CONST-02 (the real Terminus-side aggregation layer) needs to serve** —
this is the contract the httpAdapter already assumes:

| Method | Path | Response |
|---|---|---|
| GET | `/api/auth/me` | `{ authenticated: boolean; username: string \| null }` |
| POST | `/api/auth/login` (body `{username,password}`) | same as above |
| POST | `/api/auth/logout` | 200/204 |
| GET | `/api/health` | `{ system: 'harmony'\|'chord'\|'lumina'\|'terminus'; available: boolean; detail?: string }[]` |
| GET | `/api/terminus/config` | `{ modules: { name: string; enabled: boolean; version?: string }[]; workerCount: number }` |
| any | `/api/{system}/{path}` | generic passthrough used by `client.request<T>()` for panel-specific reads that don't have a typed method yet |

### 2. The module registry (`src/lib/moduleRegistry.ts`)

Panels register a descriptor instead of the shell hardcoding a route table:

```ts
registerPanel({
  id: 'terminus.config',
  system: 'Terminus',      // nav group: Harmony | Chord | Lumina | Terminus | Providers | Status
  title: 'Config',
  path: '/terminus/config',
  icon: '⚙',
  available: true,          // false (or absent registration) => panel never renders
  component: TerminusPanel,
});
```

`App.tsx`/`Sidebar.tsx` only ever call `getAvailablePanels()` / `getPanelsBySystem()` — a
panel whose backend capability doesn't exist yet is either not registered at all, or
registered with `available: false`; either way it silently doesn't render. No crash, no
placeholder page.

## Adding a panel

1. Create `src/panels/<system>/<Name>Panel.tsx`. Read data via
   `getAggregationClient().<namespace>.<method>()` — add a typed method to
   `AggregationClient` (and both adapters) if one doesn't exist yet, or use the generic
   `client.request<T>(system, path)` escape hatch in the meantime.
2. Add a `registerPanel({...})` call in `src/panels/registerPanels.ts`.
3. Nothing else changes — the shell picks it up automatically.

## No-secrets-in-browser rule

`useAuth`/`useApi` hold auth state in memory (React state) only, via session cookies
(`/api/auth/*`). Nothing is ever written to `localStorage`/`sessionStorage` — this is a
hard rule for this app (harmony-web's `localStorage['harmony_soma_api_key']` + `prompt()`
fallback was deliberately dropped, not ported). Vault-referenced secrets (provider API keys,
etc., landing in CONST-08+) must be surfaced as a vault key *name* with a set/rotate
affordance, never a round-tripped value.

## Real-time relay (`/ws`, CONST-18)

`GET /ws` (`src/constellation/ws.rs` on the Terminus side, not in this package) is a
session-authenticated, masked WebSocket relay -- the same cookie-JWT check
`require_session` uses is verified BEFORE the upgrade is ever accepted, so an
unauthenticated caller gets a `401`, never a half-open socket. Once accepted, it dials
Harmony's own event socket (`CONSTELLATION_HARMONY_WS_URL`, a Terminus-side env var --
see `.env.example`) and pipes events to the browser, each wrapped as `{source:'harmony',
event:...}` and passed through the SAME `mask_response` masking every `/api/*` response
gets. If `CONSTELLATION_HARMONY_WS_URL` is unset, the relay still accepts the upgrade
(auth already passed) but immediately sends a typed WebSocket close frame (code `4000`,
"no upstream configured") and the app falls back to 30s polling -- this is expected,
degraded-but-functional behavior, not an error to chase down. A typed close of `4001`
("upstream unreachable") means the relay dialed Harmony's socket and lost/never got it
after its bounded reconnect budget was exhausted -- same polling fallback applies.
`ws.connect()` (`src/lib/aggregationClient.ts`) already treats every close uniformly
(reconnect/backoff, then fall back to polling) -- no client-side branch on the close code
is required for this item; a future item MAY use the code to distinguish "no backend
configured" from "backend flapped" in the UI if that becomes useful.

## Dev / build

```sh
npm install
npm run dev        # vite dev server, :5174, proxies /api and /ws to :3100 by default
npm run typecheck  # tsc --noEmit
npm run build       # tsc --noEmit && vite build -> dist/
```

Set `VITE_AGG_MODE=http` (e.g. in `.env.local`) to point the app at a real backend instead
of the mock adapter.

## Embedded build (CONST-15)

`dist/` is **committed** into the repo (not gitignored) and embedded directly into the
`terminus_primary` binary via `include_dir` (`src/constellation/assets.rs`,
`include_dir!("$CARGO_MANIFEST_DIR/constellation-web/dist")`). This is deliberate: the fleet's build-on-dest pipeline
(`constellation-updater`, moosenet-spec v3.23) runs a **cargo-only** build on the deploy
host with no npm/node toolchain — the committed dist is what makes that possible. The
embedded UI is always served same-origin by the binary in production, so it is always
built with `VITE_AGG_MODE=http` (never the mock adapter).

**Whenever the UI changes, rebuild and recommit `dist/`:**

```sh
VITE_AGG_MODE=http npm run build
git add -f constellation-web/dist
```

`CONSTELLATION_WEB_DIST_DIR` remains available as an optional filesystem override for local
dev against a live-reloading build — when set, the binary serves from that directory
instead of the embedded assets (see `src/constellation/mod.rs`).
