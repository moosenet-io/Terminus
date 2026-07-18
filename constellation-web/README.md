# constellation-web

The Lumina Constellation control-plane UI (spec `S119-constellation-gui-v2`, building on the
CONST-04 foundation). A React 18 / TypeScript 5 / Vite 5 single-page app, adapted from
`harmony/harmony-web` (see `docs/constellation/CONST-01-adaptation.md` in the CONST-01
worktree for the full inventory + reuse map this was built from) and re-architected as a
**module registry v2 + two-tier shell** by CONST-16 (`docs/constellation/CONST-GUI-SPEC.md`).

## Three patterns everything else builds on

### 1. The aggregation client (`src/lib/aggregationClient.ts`)

This is the **only** module in the app allowed to call `fetch`, read `window.location`, or
touch `localStorage` (the last one only via the `prefs` seam below). Every hook, panel, or
component that needs backend data goes through the exported `getAggregationClient()`
singleton — never `fetch` directly. This keeps the browser's only network surface to
same-origin `/api/{harmony,chord,lumina,terminus}/*` calls, cookie-based
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

#### The `prefs` seam (CONST-16)

`client.prefs.get<T>(key)` / `client.prefs.set<T>(key, value)` is the **only** place
`localStorage` is allowed to appear anywhere in this app (grep-gated). It's an allowlisted,
non-secret store for exactly two keys — `'layout'` (the Overview canvas' card order + hidden
set) and `'density'` (Comfortable | Compact) — nothing else may ever be stored there; passing
any other key throws. If you need to persist new UI state, either fold it into one of those
two shapes or don't add it to this seam (open a spec item — this is deliberately not a
general key-value store).

### 2. The module registry (`src/lib/moduleRegistry.ts`)

**Modules** (CONST-16) sit above panels: a module is one fleet system's presence in the GUI —
a global-bar tab, a health binding, and the group of panels underneath it. Register one at
import time in `registerPanels.ts`:

```ts
registerModule({
  id: 'chord',              // ModuleId: harmony | chord | lumina | muse | terminus | models | mint
  title: 'Chord',
  icon: '⚡',
  healthSystem: 'chord',    // which /api/health entry gates this module's availability
  order: 2,                 // fixed global-bar order — never reorders at runtime
});
```

A module is available to `getAvailableModules(health)` iff it's registered AND its
`healthSystem` entry in the given health snapshot reports `available: true`. `App.tsx` applies
a 2-cycle stale-while-degrading grace to the raw `/api/health` poll before calling this — a
system stays reported available through `GRACE_CYCLES` consecutive misses (an explicit
`available: false`, vanishing from the payload entirely, or a wholesale poll failure all count
as a miss); only the miss *after* that — the `GRACE_CYCLES + 1`-th in a row — actually hides
its module's tab. One flaky poll never yanks a module out from under the operator.

**Panels** are unchanged in contract from CONST-04 — only the `system` field's type changed,
from the old capitalized `SystemGroup` ('Harmony' | 'Chord' | ... | 'Providers' | 'Status') to
a lowercase `ModuleId` that matches a registered module directly:

```ts
registerPanel({
  id: 'terminus.config',
  system: 'terminus',       // ModuleId, not the old SystemGroup label
  title: 'Config',
  path: '/terminus/config',
  icon: '⚙',
  available: true,          // false (or absent registration) => panel never renders
  component: TerminusPanel,
});
```

The legacy `SystemGroup` type and `legacySystemGroupToModuleId()` map are kept only so old
code/tests referencing 'Status'/'Providers' have a defined mapping ('Status' → `harmony`,
since the Analytics/Engine-Diagram panels render Harmony/Chord pipeline data and the
top-level 'Status' group dissolves into Overview; 'Providers' → `terminus`) — no panel
registration should use those labels going forward.

`App.tsx`/`GlobalBar.tsx`/`ModuleRail.tsx` only ever call `getAvailableModules()` /
`getAvailablePanels()` / `getPanelsByModule()` — a panel or module whose backend capability
doesn't exist yet is either not registered at all, or registered with `available: false` /
never reporting healthy; either way it silently doesn't render. No crash, no placeholder page.

### 3. The shell: two-tier nav + card canvas (`src/App.tsx`, CONST-16, §3.1 of the spec)

- **`GlobalBar`** (top, `src/components/GlobalBar.tsx`) is the module switcher — replaces the
  old single `Sidebar`. Renders the wordmark (`Wordmark.tsx`), one tab per available module
  (health dot + degraded indicator), a `⌘/Ctrl+K` "go to panel" trigger, the density toggle,
  and the account chip.
- **`ModuleRail`** (left, `src/components/ModuleRail.tsx`) renders the *active* module's
  panels (`getPanelsByModule`). Responsive: icon-only rail below 1100px width, a drawer
  overlay (triggered from `GlobalBar`'s hamburger) below 760px.
- **The Overview card canvas** (`/overview`, the default route, `src/panels/overview/`) is one
  seven-region `ModuleCard` per available module (drag handle, StatusPill, kind/role line,
  metric row, last-activity line, Open/Configure + Hide, an expandable body). Operators can
  drag-reorder, hide, and re-add cards ("+ Add widget"); a card focused with the keyboard
  reorders via `⌘/Ctrl+arrow`. Layout + density persist **only** via `client.prefs`.

## Adding a panel

1. Create `src/panels/<module>/<Name>Panel.tsx`. Read data via
   `getAggregationClient().<namespace>.<method>()` — add a typed method to
   `AggregationClient` (and both adapters) if one doesn't exist yet, or use the generic
   `client.request<T>(system, path)` escape hatch in the meantime.
2. Add a `registerPanel({...})` call in `src/panels/registerPanels.ts`, with `system` set to
   an existing (or newly `registerModule`d) `ModuleId`.
3. Nothing else changes — the shell picks it up automatically.

## No-secrets-in-browser rule

`useAuth`/`useApi` hold auth state in memory (React state) only, via session cookies
(`/api/auth/*`). The **only** browser storage anywhere in this app is the `client.prefs` seam
above, and it may hold only the two non-secret, allowlisted keys described there — this is a
hard rule for this app (harmony-web's `localStorage['harmony_soma_api_key']` + `prompt()`
fallback was deliberately dropped, not ported, and CONST-16's prefs seam does not reopen that
door — it's structurally incapable of storing a credential shape). Vault-referenced secrets
(provider API keys, etc., landing in CONST-08+) must be surfaced as a vault key *name* with a
set/rotate affordance, never a round-tripped value.

## Roles (CONST-27)

There are exactly two session tiers, both minted onto the same signed JWT from CONST-03 (no
new auth system, no per-module ACLs — YAGNI for a single-operator fleet):

- **operator** — full read/write. Also the default for a session token with no `role` claim
  at all (every session minted before CONST-27 shipped), so a live login survives the deploy.
- **viewer** — read-only. Logs in against `CONSTELLATION_VIEWER_SECRET` (a *second*,
  distinct <secret-manager>-provisioned secret checked after the operator secret) and gets a
  structural `403 {"error":"forbidden","required_role":"operator"}` from the server on every
  mutating method (`POST`/`PUT`/`PATCH`/`DELETE`) — see
  `src/constellation/auth.rs::enforce_viewer_role_gate` and its `.env.example` entry.

**The enforcement is server-side, not this app.** `getAggregationClient().auth.me()` returns
a `role` field (`'operator' | 'viewer' | null`), republished app-wide via
`AuthRoleContext`/`useAuthRole()` (`src/hooks/AuthRoleContext.tsx`) so `RoleGate`
(`src/components/RoleGate.tsx`) can wrap a mutating control and render it disabled with an
"operator role required" tooltip for a viewer session. That's a courtesy only — proven by the
Rust test suite issuing a direct `POST` as a viewer and asserting `403`, independent of
whatever this UI renders. Currently gated: the harmony dashboard's engine/build/mode/
inference-mix/compression/command controls (`EngineControls`, `BuildControls`,
`ModeSelector`, `InferenceMixSlider`, `ConversationBar`). Chord and Muse have no mutating
panels yet in this checkout (tracked separately under CONST-05..14/CONST-28 and the Muse
sprints) — gate their write controls with the same `RoleGate` when those panels land, and
the palette's *action* commands (not yet built — today's `MiniPalette` in `GlobalBar.tsx` is
navigation-only) the same way once CONST-25 adds them.

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
