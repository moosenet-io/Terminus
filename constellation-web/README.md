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
same-origin `/api/{harmony,chord,lumina,muse,terminus}/*` calls, cookie-based
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
| GET | `/api/health` | `{ system: 'harmony'\|'chord'\|'lumina'\|'muse'\|'terminus'; available: boolean; detail?: string }[]` |
| GET | `/api/terminus/config` | `{ modules: { name: string; enabled: boolean; version?: string; toolCount?: number; tools?: string[] }[]; workerCount: number }` (`toolCount`/`tools` are CONST-28, additive — a pre-CONST-28 backend response is still valid, just without them) |
| GET | `/api/terminus/activity?limit=N` | `{ entries: { ts: string; method: string; path: string; principal: string \| null; system: string }[] }` — tail of the CONST-02 mutating-request audit log; **never body content**. `limit` asks for fewer entries, never more than the server's own `CONSTELLATION_ACTIVITY_TAIL_LIMIT` cap (default 200). A missing/empty audit log yields `{entries: []}`, `200 OK` — never an error. CONST-28's client additionally degrades to `{available:false}` on 404/501/error (see Terminus module panels below). |
| any | `/api/{system}/{path}` | generic passthrough used by `client.request<T>()` for panel-specific reads that don't have a typed method yet |

**CONST-21 — the Models/MINT read API** (`src/constellation/models_api.rs`, spec
`docs/constellation/CONST-GUI-SPEC.md` §8). Feeds the Model Library (CONST-22) and MINT
(CONST-23/24) modules. Every endpoint is protected (session cookie, same guard as everything
above), masked, read-only `GET`, and reuses the existing `src/intake/{storage,catalog,
discovery}` read layer — no second database pool, no MCP self-calls. List endpoints take
`limit` (default 50, max 500) + `offset` and report a `total`. `epoch` follows
`EpochSelector`: absent ⇒ current epoch, `epoch=all` ⇒ every epoch, else ⇒ that one epoch.

| Method | Path | What it returns |
|---|---|---|
| GET | `/api/terminus/models?scope=&q=&category=&status=&serving=&limit=&offset=` | paged, joined fleet-catalog ⋈ discovery-brochure ⋈ advisor-matrix ⋈ serving keep-warm view |
| GET | `/api/terminus/models/{name}` | one model's identity/brochure/serving/operational/catalog sections (each independently `null` when that source has nothing; `404` only when the name is unknown to every source) |
| GET | `/api/terminus/mint/summary?epoch=` | the Overview stat-tile payload (models profiled, run counts, fleet-best model, GPU-hours, current epoch) |
| GET | `/api/terminus/mint/dimensions?models=&epoch=` | the capability-radar payload (8 assistant dimensions, fleet-wide normalized, + fleet median) |
| GET | `/api/terminus/mint/matrix?epoch=` | the coverage heatmap (fleet-catalog cells) |
| GET | `/api/terminus/mint/runs?suite=code\|context\|agent&…&limit=&offset=` | paged raw run rows (table-view / drill-down source). `epoch` applies to `suite=code` only — `context`/`agent` runs tables are epoch-less, so an explicit specific epoch there is a `400` (absent / `epoch=all` proceed) |
| GET | `/api/terminus/mint/box?metric=total_time_ms\|code_quality_score&…` | server-side quartiles + outliers per model (raw rows never reach the browser) |
| GET | `/api/terminus/mint/language-stats?language=&epoch=` | per-model/language rollup (the Pareto-scatter source) |
| GET | `/api/terminus/mint/failures?epoch=&task_category=` | per-model failure-class counts, top-5 + "other" fold |
| GET | `/api/terminus/mint/context-profiles?models=` | per-model context-tier arrays + `max_context_safe` |
| GET | `/api/terminus/mint/activity?range=` | runs/day by suite + the current epoch marker |

CONST-19 adds the fourth namespace, `/api/muse/*path` — identical single-door/masking/audit/
degradation semantics to the other three, with one difference: `/api/muse/art/*` (poster/art
images) passes through as raw bytes with the upstream's own content-type rather than JSON —
fetch those by URL (e.g. an `<img src>`), not through `client.request<T>()`.

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
  metric row, last-activity line, Open/Configure + Hide, an expandable body), plus a fixed
  **`ActivityFeedCard`** (see below) that is not part of the drag/hide layout system. Operators
  can drag-reorder, hide, and re-add module cards ("+ Add widget"); a card focused with the
  keyboard reorders via `⌘/Ctrl+arrow`. Layout + density persist **only** via `client.prefs`.

## Notifications & activity feed (CONST-26, §3.3)

One shell-level hook, `useActivityFeed` (`src/hooks/useActivityFeed.ts`), merges three sources
into a single, deduplicated, most-recent-first `FeedItem[]` — the pure merge/dedupe/severity
logic lives in `src/lib/activityFeed.ts` (unit-testable independent of React/network):

1. **Activity** — polls `GET /api/terminus/activity` every 30s (same cadence as the health
   poll).
2. **Health transitions** — diffs consecutive `/api/health` snapshots (e.g. `chord ->
   unavailable`); `App.tsx`'s `Shell` is the one place already doing this diffing, so the hook
   takes the shell's health state as input rather than polling a second time.
3. **`/ws` events** — subscribes via `client.ws.connect()` (CONST-18); when no live event
   stream is configured this contributes nothing, silently — no special-casing needed by
   callers.

This one feed backs two surfaces, both pure renderers with no polling/subscriptions of their
own:

- **`ActivityFeedCard`** (`src/panels/overview/ActivityFeedCard.tsx`) — an Overview canvas
  widget rendering the feed in the brand's log-line voice (§2.2, `[ok] ...` / `[warn] ...` /
  `[error] ...`).
- **`NotificationBell`** (`src/components/NotificationBell.tsx`) — a bell menu in `GlobalBar`
  retaining the **last 50 items, in memory only** — never `localStorage`/`sessionStorage` (the
  CONST-16 `prefs` seam is layout/density state, not a notification history; this is
  deliberately NOT routed through it).

**Toasts** (`src/components/Toast.tsx`, `ToastProvider`/`useToastContext`, mounted once at the
app root) fire for exactly two things, per spec — never anything else:

- **Mutation results** — observed centrally via `aggregationClient`'s `onMutationResult`
  seam, which every mutating (`POST`/`PUT`/`PATCH`/`DELETE`) `client.request<T>()` call already
  emits on completion. No individual panel needs to opt in.
- **Health transitions** — `App.tsx`'s `Shell` passes a callback into `useActivityFeed` that
  also pushes a toast for each detected transition.

Toasts auto-dismiss after 6s and render in a fixed `aria-live="polite"` region so a screen
reader announces one without interrupting the current task.

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


## Brand system (CONST-17)

The app renders the **Terminus GUI Brand Guide** ("deep space violet" portal, v1.0) — see
`docs/constellation/CONST-GUI-SPEC.md` §2. `src/styles/globals.css` is the canonical token
sheet (surfaces, violet accent ramp, semantic "flux" hues, type, spacing, radius, glow,
motion). Two rules that are grep-enforced in review:

- **No raw hex where a token exists.** New code reaches for a `--token`, never a literal
  hex. The `StatusColor` union (`Card.tsx`) stays the only sanctioned status-color API.
- **Color is always semantic (§2.4).** The five flux hues carry fixed meanings — violet =
  core/brand; blue = inbound/source/cold; green = outbound/endpoint/free; amber =
  cloud/gated/paid/warm; rose = alert/error/hot. A chart series that IS one of these
  semantics wears that token; only nominal identity (models, languages, providers, tiers
  without a fixed meaning) gets a categorical slot (`src/viz/palette.ts`).

**Legacy aliases** (`--bg-surface`, `--accent-primary`, `--text-primary/secondary/tertiary`,
the old `--text-xs..metric` scale, `--h-*`, …) are kept in `globals.css` for ONE release so
the panels ported from harmony-web restyle without a full rename — every alias is dated
"LEGACY (CONST-17)" and scheduled for removal at CONST-29. Do not add new call-sites against
the legacy names.

**Fonts** are self-hosted: Inter 400/500/600/700 + JetBrains Mono 400/500/700 (latin subset
woff2, ~172KB total) live in `public/fonts/` and are declared in `src/styles/fonts.css`
(`font-display: swap` + system fallbacks in `--font-sans`/`--font-mono`). The brand guide's
hosted-fonts `@import` is NOT used — the built dist makes zero external requests (same-origin
model, audit §3). If you ever need to re-fetch/update a font file, pull the real `.woff2`
binary and commit it; never point `@font-face` at a remote URL.

### Dataviz palette validation

The 6 categorical slots (`--series-1..6`) were run through the dataviz skill's
`validate_palette.js` against `--mode dark --surface "#161130"` (the card surface), plus
`--pairs all` for slots 1-4 (the scatter/radar/swarm all-pairs cap). Three slots failed the
brand-faithful starting point from spec §4.2 and were **snapped within their own brand ramp**
(hue held, lightness moved only):

| Slot | Role | Spec §4.2 value | Snapped value | Reason |
|---|---|---|---|---|
| `--series-2` | flux-green family | `#10B981` | `#059669` | outside the dark-mode lightness band |
| `--series-3` | flux-amber family | `#F59E0B` | `#D97706` | outside the dark-mode lightness band |
| `--series-4` | flux-blue family | `#3B82F6` | `#1D4ED8` | ΔE 0.9 vs violet-400 under deutan sim (all-pairs) |
| `--series-6` | violet-200 family | `#DDC9FD` | `#9D6FE0` | outside lightness band + below chroma floor |
| `--series-1` | violet-400 | `#A855F7` | unchanged | — |
| `--series-5` | flux-rose | `#F43F5E` | unchanged | — |

Final report (`node validate_palette.js "#A855F7,#059669,#D97706,#1D4ED8,#F43F5E,#9D6FE0"
--mode dark --surface "#161130"`): **ALL CHECKS PASS** (lightness band, chroma floor, normal-
vision floor, contrast vs surface all PASS; CVD separation reports a WARN in the 6-8 ΔE band
on the adjacent amber/green pair and on the all-pairs violet/blue pair — legal per the skill's
rule *"CVD in the 6-8 floor band is legal ONLY with secondary encoding: direct labels, gaps,
or texture"*, satisfied here because every chart ships a `ChartLegend` + `TableViewToggle`,
§4.2/§4.4). Status/semantic tokens (`--flux-*`, `--status-*`) were left at their spec values —
only the categorical chart-slot copies were snapped, since those are the ones the validator
scopes to.

### The viz kit (`src/viz/`)

**Panels never import `recharts`/`@nivo/*` directly — always import from `src/viz/`.**
`theme.ts` bridges the CSS tokens into a nivo theme + Recharts style constants (memoized
`getComputedStyle` read); `palette.ts` holds the categorical/sequential/diverging accessors
plus `SlotAssigner` (first-seen-order categorical slot assignment, stable across filtering —
instantiate one per chart instance, not per render). `ChartCard`/`ChartTooltip`/
`ChartLegend`/`ChartEmpty`/`ChartSkeleton`/`TableViewToggle` are the shared chart chrome
every chart composes (loading/refetch/empty/degraded states, table-view twin, textContent-
only tooltip label insertion since series/point labels can be untrusted upstream data). For
the advanced chart forms (radar/boxplot/heatmap/parallel-coordinates/swarmplot/scatterplot),
CONST-17 ships the FOUNDATION only: pinned `@nivo/*` 0.99.0 packages, the shared nivo theme
bridge (`theme.ts`), and a dedicated `viz` Vite chunk (`vite.config.ts` `manualChunks`) so
the shell/panels' initial bundle doesn't pay for nivo. The chart-form wrapper components
themselves land with the routes that use them (MINT/Models, CONST-22..24), which lazy-import
their panels.

Grid lines are **solid 1px hairlines** (`--chart-grid`/`--chart-axis`) — the dashed
`strokeDasharray:'3 3'` pattern from harmony-web is retired everywhere (audit §1.4). Every
chart ships a table-view twin (`TableViewToggle`) — this is both the WCAG relief channel for
sub-3:1 fills and a hard rule (§4.4).
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

## Terminus module panels (CONST-28)

The `terminus` module's own self-observability surface, built on the CONST-04 `Config` panel's
pattern, in `src/panels/terminus/`:

- **`FleetPanel.tsx`** ("Fleet") — a health board with one card per fleet system
  (harmony/chord/lumina/terminus). Each card polls `client.health.list()` on its own 5s
  interval and accumulates into a **client-held ring buffer of the last 120 polls per system**
  (`fleetRingBuffer.ts` — pure, framework-free, unit-tested in `fleetRingBuffer.test.ts` via
  `npm run test`: capacity cap, transition/flap detection, uptime ratio). Each card renders an
  uptime `Sparkline` (`src/viz/Sparkline.tsx`, the viz kit's minimal chrome-free line chart) plus
  the mesh/broker summary (module/worker counts) from `/api/terminus/config`. Edge cases: an
  empty broker-routes table reads as "0 (in-process)", not an error; a failing health poll
  leaves every system's ring buffer at its last-known content (see the pure function's own
  "leaves a system untouched" test) rather than clearing it.
- **`ToolsPanel.tsx`** ("Tools") — the full tool catalog, grouped by module prefix, from the
  CONST-28-extended `/api/terminus/config` (`modules[].tools`/`toolCount`). Searchable (text +
  per-module filter chips) and paged (`DataTable`, 25 rows/page) — the mock fixture pads `plane`
  out to 34 tools specifically to exercise paging. A `TODO(CONST-25 seam)` comment marks where
  the command-palette entity-source registration wires in once that item lands (CONST-25 isn't
  on this branch's base yet — deliberately not imported ahead of time so this typechecks/builds
  clean against `origin/main`).
- **`ActivityPanel.tsx`** ("Activity") — a paged, filterable (system/method/principal) view
  against the §8 contract `GET /api/terminus/activity?limit=` → `{entries:[{ts,method,path,
  principal,system}]}`. That Rust endpoint is CONST-26's, landing in parallel with this item —
  this panel only *consumes* `client.terminus.activity()` (`aggregationClient.ts`), which
  already degrades to `{available:false}` on a 404/501/any failure; the panel then renders an
  explanatory "not live yet" empty state instead of an error.

All three are registered under the existing `terminus` module in `registerPanels.ts` alongside
the pre-existing `Config` panel (`terminus.fleet` / `terminus.tools` / `terminus.activity`).

## Dev / build

```sh
npm install
npm run dev        # vite dev server, :5174, proxies /api and /ws to :3100 by default
npm run typecheck  # tsc --noEmit
npm run test       # vitest run — currently: fleetRingBuffer.test.ts (CONST-28)
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
