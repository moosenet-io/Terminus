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

- **`httpAdapter`** — real fetch against `/api/...`, the same origin the SPA is served from.
  **Default in any browser** (the SPA is served same-origin by the real terminus binary in
  production, so the backend is right there).
- **`mockAdapter`** — canned, in-memory data. Explicit opt-in only; lets the app build, run, and
  be reviewed with zero backend present.

S127 TGUI2 — the adapter default is **http**, and selection is runtime-selectable (see
`resolveMode()` in `src/lib/aggregationClient.ts`). Precedence: build-time `VITE_AGG_MODE`
(`http`|`mock`) › server-injected `window.__AGG_MODE__` › runtime mock opt-in (`?mock` URL param
or `localStorage['constellation.aggMode']='mock'`) › **http** in any browser › `mock` only when
there is no `window` (unit tests/SSR). This inverts the old build-time-only default (which was
`mock`, so a build that forgot `VITE_AGG_MODE=http` shipped the entire app as fixtures). A
mock-only bundle can no longer ship silently; `npm run build:verify` asserts the emitted bundle
can reach the http adapter (`scripts/assert-http-bundle.mjs`).

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

**LGUI-05 — Lumina proxy authentication.** `/api/lumina/*path` is the one namespace that
authenticates itself to its backend server-side: `proxy_lumina`
(`src/constellation/proxy.rs`) attaches `Authorization: Bearer <CONSTELLATION_LUMINA_TOKEN>`
(unset ⇒ unauthenticated passthrough, unchanged from before this item) and `X-Lumina-User:
<verified session principal>` on every outbound call. The browser never holds, sets, or reads
either header — `enforceHeaders` (above) strips a caller-supplied `Authorization`/
`X-Lumina-User` client-side as a defense-in-depth door, and the Rust proxy independently never
reads any inbound header but `content-type` to build its own outbound ones. A `401` from
Lumina (misconfigured/rejected token) degrades to the same `{available:false,
detail:"lumina auth failed"}` shape every other backend failure uses, never a raw `401`
forwarded to a browser session that has no way to react to it.

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
  (health dot + degraded indicator), a `⌘/Ctrl+K` command palette trigger (see §4 below), the
  density toggle, and the account chip.
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

### 4. The command palette (`src/components/CommandPalette.tsx`, CONST-25, §3.2 of the spec)

`⌘/Ctrl+K` anywhere in the shell opens the palette (`App.tsx`'s `Shell` owns the open state and
the global keydown listener — not `GlobalBar`, so the shortcut works regardless of what has DOM
focus). Zero new dependencies: its own subsequence fuzzy-matcher
(`src/lib/commandMatch.ts`), its own `role="dialog"`/`listbox` markup, CSS tokens only.

Three sources, always shown in this order, each degrading independently:

1. **Navigation** — every panel in the same health-filtered set `App.tsx` routes (never the raw
   registry), ranked by `src/lib/commandMatch.ts#fuzzyMatch` against the query.
2. **Actions** — `src/lib/commandRegistry.ts#registerCommand()`, a sibling of `registerPanel`/
   `registerModule` (same "register once, at import time" convention). Register a command
   anywhere a panel is registered:

   ```ts
   registerCommand({
     id: 'shell.refresh-health',       // must be globally unique — duplicates THROW at
     title: 'Refresh health',          // registration time (not silently overwritten, unlike
     subtitle: 'Re-poll /api/health',  // registerPanel/registerModule — see the file's doc
     icon: '⟳',                        // comment for why)
     minRole: 'viewer',                // default; 'operator' hides the command entirely for
     run: () => requestHealthRefresh(),// a viewer session (not merely disabled)
   });
   ```

   **Role gating (CONST-27, merged):** `App.tsx`'s Shell reads the real session role via
   `useAuthRole()` (from CONST-27's `AuthRoleProvider`) and passes it into `CommandPalette`;
   `getAvailableCommands(role)` HIDES operator-only commands from a `'viewer'` session. A
   `null` role (unauthenticated edge — the palette normally never renders there) resolves to
   `'operator'` purely as the documented backward-compat fallback, mirroring the server's own
   claim-absent-token rule; the UI gate remains cosmetic — the server's
   `enforce_viewer_role_gate` 403 is the real enforcement.

3. **Entity search** — `src/lib/entitySearch.ts#searchEntities()`, debounced 150ms, fans the
   query out (`Promise.allSettled`, never `Promise.all`) to a handful of cheap existing list
   reads (sessions, agent activity, providers, models, terminus modules), grouped by source. A
   dead/erroring backend shows one greyed-out "`<Group>` unavailable" row for its own group and
   changes nothing else — it can never suppress navigation, actions, or another source's hits.

**Keyboard contract:** `↑`/`↓` move the selection; `Tab`/`Shift+Tab` jump to the first row of
the next/previous non-empty group; `Enter` runs the selected row; `Esc` closes. The text input
keeps DOM focus for the palette's entire lifetime — the "selection" is virtual
(`aria-activedescendant` into a `role="listbox"`/`role="option"` tree), which both implements
the focus trap (nothing else on the page can steal focus while it's open) and keeps screen
readers on the standard combobox-listbox pattern.

**Adding an entity source:** add one entry to the `SOURCES` array in `entitySearch.ts` — a
`group` label and a `load(client)` function that calls `client.request(...)` (or a typed
aggregation-client method) and maps the response to `EntityHit[]`. It degrades automatically;
no palette code changes.

**Testing:** `src/lib/commandMatch.test.ts` is a small dependency-free assertion file
(`runCommandMatchTests()`) covering the fuzzy matcher and `rankItems` — this repo has no JS test
runner wired up yet (no vitest/jest in `package.json`), so it isn't invoked by any script today;
wire it into `npm test` the moment one is added.

## Lumina module (`lumina.*`, LGUI-05/06)

The `lumina` module (id `lumina`, `healthSystem: 'lumina'`) is the assistant's home in the
portal (LUMINA-GUI-SPEC.md §1/§2). Its first panel, `lumina.overview` (route `/lumina`, min
role viewer — `src/panels/lumina/OverviewPanel.tsx`), is the assistant dashboard:

- **Identity Card** (`src/panels/lumina/IdentityCard.tsx`) — StatusPill from `status.state`,
  uptime + version, one Badge per channel (green=connected, neutral=configured-off,
  amber=misconfigured), `glow`s when online.
- **Tile row** — memories (engram total + 24h delta when derivable), turns today, deep-turn
  share, active users, reminders. Tiles degrade to an em dash rather than fabricating a number
  when a section hasn't loaded or the source can't derive it.
- **Charts** (viz kit only, per CONST-GUI-SPEC.md §4) — memory growth (30-day area, single
  series), routing mix (14-day stacked bars, fast vs deep), top tools (7-day horizontal bar).
  Each chart is backed by its OWN windowed request/slice (review fix) — routing mix and top
  tools are two SEPARATE `useLumina` sections (`analyticsRouting` at `days=14`, `analyticsTools`
  at `days=7`), not one over-fetched request rendered into two differently-labeled charts,
  because the backend's `top_tools` ranking is itself scoped by the `days` param it's asked
  for. Memory growth's `growth_30d` is defensively `.slice(-30)`'d client-side.
- **Activity feed** — last 20 events in the log-line voice (`[ok] tool searxng_search 412ms`).
- **First-run**: when `GET /api/lumina/status` reports `onboarding_complete: false`, the intent
  is `/lumina/setup` (LGUI-12's wizard route). Review fix: the panel checks the registry
  dynamically (`isPanelAvailable('lumina.setup')`, `src/lib/moduleRegistry.ts`) before
  redirecting — while LGUI-12 is unmerged that check is false, so instead of an unconditional
  `Navigate` (which just bounces off App.tsx's wildcard Route back to `/overview`, making the
  "NEW · needs setup" card permanently unreachable), the panel renders that hero card here on
  `/lumina` with its "Begin setup" action disabled and annotated "setup wizard lands with
  LGUI-12". The moment LGUI-12 registers `lumina.setup`, the redirect self-activates with zero
  code change on either side.
- **Degraded/empty states**: a whole-panel degraded card when `/api/health`'s `lumina` entry
  reports `available: false`; per-section `ChartEmpty` (e.g. "No memories yet — they'll appear
  as you talk") when a store has no data yet. `status.display_name` and `engram.growth_30d` are
  OPTIONAL additive extensions not in the §7 sketch (`src/types/lumina.ts`) — `undefined`
  (field absent) and empty-but-present are distinct states with distinct copy: the identity
  card falls back to "Lumina" + version/uptime with no name, and the memory-growth chart shows
  "backend does not expose a memory-inserts series yet" (field absent) vs "No memories yet"
  (field present, store just has no history). Each of the five backing reads (status, engram
  stats, analytics-routing, analytics-tools, analytics events) is its own independent
  `useLumina` section state, so a slow/failing one degrades on its own without blanking the
  rest of the panel.

Data comes from `src/hooks/useLumina.ts`, which polls the §7 endpoints
(`/api/lumina/status`, `/api/lumina/engram/stats`,
`/api/lumina/analytics?view=summary&days=14`, `/api/lumina/analytics?view=summary&days=7`,
`/api/lumina/analytics?view=events&days=7`) through `client.request('lumina', ...)` — see
`src/types/lumina.ts` for the exact response shapes (REQUIRED surface is §7 exactly, plus the
two documented OPTIONAL additive fields above) and `lib/aggregationClient.ts`'s mock data for
the canned fixtures those hooks build against with no backend present.

**Seam note**: the shared Overview card canvas (`panels/overview/ModuleCard.tsx`) has a
4-state `CardState` union (`online`/`idle`/`error`/`disabled`) with no per-module state-
injection seam yet — adding a 5th "needs setup" canvas state is a canvas refactor out of
LGUI-06's scope. The "NEW · needs setup" badge + "Begin setup" button instead render on the
Lumina module's own `/lumina` route today; wire it through `ModuleCard` once that seam exists.

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

## Lumina module — Persona & Behavior panel (LGUI-09)

`lumina.persona` (`/lumina/persona`, operator, `src/panels/lumina/PersonaPanel.tsx`) is the
first real `lumina` panel wired against the `LUMINA-GUI-SPEC.md` §7 data contracts (LGUI-01..04,
the API items, are `moosenet/lumina-constellation` PRs and may not be merged yet — this panel
consumes `GET/PUT /api/persona*` and the `onboarding_complete`/`dynamic_prompt` slice of
`GET /api/status` through the generic `client.request('lumina', path)` escape hatch, same
convention as `useMuse.ts`; see `src/types/lumina.ts` for the exact §7-shaped types and
`src/lib/aggregationClient.ts`'s `MOCK_LUMINA_PERSONA*` fixtures for the mock-mode contract a
real backend must satisfy).

- **Trait quartet** (`TraitSlider.tsx`) — one row per `TraitVector` axis (flair/spontaneity/
  humor/focus, §0.1.1), each showing the shared base marker, the per-user modifier delta, and
  the clamped effective value; rails render the soft bounds (0.15–0.85, client-side clamped via
  `clampToPersonaBounds` in `useLuminaPersona.ts`, mirroring the server's own
  `effective = clamp(base + modifier)`).
- **Trait radar** — a 4-axis radar thumbnail (`src/viz/RadarChart.tsx`, this item's addition to
  the viz kit — no radar wrapper existed on `main` yet; see that file's own doc for why it's
  Recharts-based like the Muse scatter/area charts rather than nivo) mirroring the quartet. The
  sliders and radar are fed from the exact same `useLuminaPersona`/`draftBase`/`draftModifier`
  state in `PersonaPanel.tsx` — there is no second copy of the trait values anywhere, which is
  what makes "radar and sliders never disagree" (the spec's explicit AC) structurally true
  rather than merely tested.
- **Knowledge digest** (read-only) + **active context** (editable textarea, `RoleGate`d,
  `PUT /api/persona/context`) + **layer inspector** (the 11 `PromptAssembler` layers in their
  fixed order, `LUMINA_PROMPT_LAYER_ORDER` in `src/types/lumina.ts`, with per-layer byte bars +
  enabled state; a `LUMINA_DYNAMIC_PROMPT=false` status flag renders a "legacy prompt mode"
  warning card).
- **Trait save** — `PUT /api/persona/traits` behind a diff-preview `ConfirmDialog` (old→new per
  changed trait); admin edits the shared base by default, with a "per-user modifier
  (admin-on-behalf)" toggle for the v1 modifier-editing path (§3.4). Every mutating control is
  wrapped in main's canonical `RoleGate` — cosmetic only, the server's `enforce_viewer_role_gate`
  is the real enforcement (see "Roles" below).
- **Ceremony card** — onboarding marker status (from the `/api/status` slice) plus a "Re-run
  naming ceremony" button that navigates to `/lumina/setup` (the LGUI-12 wizard route; this
  item only links to it, it does not implement the wizard).

## Muse module (CONST-19 backend, CONST-20 UI)

`muse` is the fourth namespaced proxy arm (`/api/muse/*path` in `src/constellation/proxy.rs`,
CONST-19) with three panels (CONST-20, `src/panels/muse/`) against it:

- **`muse.dashboard`** — a Library Overview MetricCards row (library size, active channels,
  pending items, last ingest) plus On Deck (poster rail), Premieres (sorted, past-dated
  entries dimmed not hidden), and Gaps summary.
- **`muse.taste`** — a taste-cluster scatter (first 4 clusters keep a categorical slot, the
  rest fold into one "Other" series — the §4.2 all-pairs cap), a watch-history stacked area,
  and a group-dynamics table. All read-only.
- **`muse.channels`** — channels list, per-channel lineup, and a guide grid rendered as a
  `DataTable` timeline (deliberately **not** an EPG widget, per spec §5.4). Compose/maintenance
  actions are operator-gated and confirmed.

All data comes from `src/hooks/useMuse.ts`, which wraps every Muse read in its own
`useMuseSection` call — this is the mechanism behind the module's **per-endpoint degradation**
requirement: a single unwired/erroring route (the MUSEX-WIRE reality — most Muse features
exist unwired in production) collapses only its own `ChartCard` to `ChartEmpty("not yet
wired")`, never the whole panel. Degradation is keyed on two equivalent signals: the
httpAdapter throwing `HTTP 404`/`HTTP 501`, or the mockAdapter resolving `null` for a pathname
with no `MOCK_GET` entry (aggregationClient.ts's own "not mocked" sentinel). Manually verified
by deleting a `MOCK_GET` key and confirming only that section degrades (see `useMuse.ts`'s and
`DashboardPanel.tsx`'s top comments for how).

Role gating comes from **merged CONST-27**: Muse's compose/maintenance controls are wrapped in
main's canonical `components/RoleGate.tsx` (a viewer session sees them disabled with an
"operator role required" tooltip; the real session role flows via `AuthRoleContext`'s
`useAuthRole()`), and enforcement is always server-side regardless (spec §3.4 —
`enforce_viewer_role_gate` 403s a viewer's mutating request). This build's earlier pre-merge
role seam (a local `hooks/useAuthRole.ts` + its own RoleGate variant) was DELETED when the
branch reconciled onto merged main. One local stand-in remains, clearly marked in its file
header for its real item to replace without touching call sites:

- **`components/ConfirmDialog.tsx`** — no shared modal/dialog kit on main yet (CONST-25's is
  unmerged). Minimal, brand-token, `role="dialog"` + Esc-to-cancel stand-in for Muse's
  compose/maintenance confirmations.

Two mock/route additions beyond the original §5.4 endpoint list (both plain GET/POST passthrough
under the existing `proxy_muse` arm, no `proxy.rs` change needed, both degrade the same way as
every spec'd route): `GET /stats` (the dashboard's MetricCards row has no dedicated endpoint in
the original list) and `POST /api/channels/{id}/{compose,maintenance}` (the channel mutation
routes spec §5.4 names but doesn't give an exact path for).

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
CONST-17 shipped the FOUNDATION only: pinned `@nivo/*` 0.99.0 packages, the shared nivo theme
bridge (`theme.ts`), and a dedicated `viz` Vite chunk (`vite.config.ts` `manualChunks`) so
the shell/panels' initial bundle doesn't pay for nivo. CONST-23 landed the first three
chart-form wrappers on top of that foundation: `RadarChart.tsx` (C1), `HeatmapChart.tsx` (C2),
`ScatterChart.tsx` (C4). CONST-24 lands the remaining four: `BoxPlotChart.tsx` (C3),
`SwarmPlotChart.tsx` (C5), `FailureBarsChart.tsx` (C6, Recharts), and
`ParallelCoordinatesChart.tsx` (C9) -- all four chart forms in §4.1's decision are now real.
NOTE: the `vite.config.ts` comment's "lazy-loaded" framing for MINT/Models routes is
aspirational -- this app has no `React.lazy`/route-level code-splitting anywhere yet
(`registerPanels.ts` imports every panel eagerly, MINT included); the `viz` manualChunks split
alone keeps both the initial (~155 KB gz) and viz (~150 KB gz) bundles under the §9 budget
(350/250 KB gz) even without it, but true lazy-loading is still a real gap if the bundle grows.

**`@nivo/parallel-coordinates`'s shipped types are broken** (CONST-24 finding): the installed
0.99.0 package declares `"types": "./dist/types/index.d.ts"` in its own `package.json` but does
not ship that directory -- only the `.cjs.js`/`.mjs` runtime bundles are present. Every other
pinned nivo package in this kit ships real types; this one doesn't. `viz/nivo-parallel-
coordinates.d.ts` is an ambient module shim declaring just the runtime-verified export surface
(`ResponsiveParallelCoordinates`, confirmed via `node -e "require('@nivo/parallel-coordinates')"`)
so `ParallelCoordinatesChart.tsx` can typecheck -- the one sanctioned "the library's types are
broken" escape hatch in this kit, not a precedent for under-typing wrappers in general.

**The exact-quantile / exact-value tricks** (CONST-24): both `BoxPlotChart.tsx` (C3) and
`ParallelCoordinatesChart.tsx` (C9) need nivo to reproduce SERVER-computed values (box
quartiles; a fixed 0..1 domain per axis) rather than deriving statistics from raw per-point
data itself, and neither chart form exposes a "the stats are already computed" mode or a scale
accessor to custom layers. Both wrappers work around this by feeding nivo a small synthetic
reference dataset whose exact values make nivo's OWN interpolation reproduce the desired
result with zero error (5 sorted points for boxplot's [min,q1,median,q3,max]; two rows pinned
to 0 and 1 on every axis for parallel-coordinates' pixel<->value mapping) -- see the file-header
comments in each for the exact math. This is a deliberate, documented technique, not
incidental test-fixture noise.

Grid lines are **solid 1px hairlines** (`--chart-grid`/`--chart-axis`) — the dashed
`strokeDasharray:'3 3'` pattern from harmony-web is retired everywhere (audit §1.4). Every
chart ships a table-view twin (`TableViewToggle`) — this is both the WCAG relief channel for
sub-3:1 fills and a hard rule (§4.4).

## MINT module (`src/panels/mint/`, CGUI-10 base + CONST-23/24 chart-type reconciliation)

MINT is TWO registered panels, both terminus-backed (`mint` registers as a `ModuleId` gated on
the `terminus` health entry, `registerPanels.ts` -- it has no independent proxy namespace; like
`models`, its data is server-side aggregation inside Terminus itself, `GET /api/terminus/mint/*`)
and both driven ENTIRELY by the real, live `client.mint.*` data client (`aggregationClient.ts`,
CGUI-08):
- **`mint.overview`** (`/mint/overview`, `OverviewPanel.tsx`) -- fleet-wide headline metrics,
  profiling activity over time, a category coverage roll-up, and (see below) fleet-wide
  trade-off analysis.
- **`mint.categories`** (`/mint/categories`, `CategoryReportPanel.tsx`) -- the per-category
  deep-dive: a grouped category picker over all 12 MINT categories (8 task-categories, 3 legacy
  suites, the persona radar), each rendering capability radar / coverage heatmap / distribution
  box / ranking / failure-class bars / recent runs, plus (see below) a context-degradation view
  for the `context` legacy category.

This pair superseded an independently-built, two-phase MINT UI (`CONST-23-mint-phase1` /
`CONST-24-mint-phase2`, a single sectioned `/mint` page with its own filter bar and a bespoke
mock-only data layer, built before CGUI-10's real backend/client landed on `main`). The two were
reconciled by keeping CGUI-10's two-panel structure as the base (already live, already wired to
the real backend) and porting only the CONST-23/24 chart TYPES that panel pair genuinely lacked,
rewired to the real client:
- **Low-n distribution honesty** (`BoxPlotChart.tsx`): a group with `n < 5` no longer renders a
  5-number-summary box (statistically misleading from that few samples) -- it renders the
  individual observed values (summary points + outliers) as jittered dots instead.
- **Trade-off parallel coordinates** (`TradeoffsSection.tsx`, folded into `mint.overview`;
  `viz/ParallelCoordinatesChart.tsx` + `viz/nivo-parallel-coordinates.d.ts`) -- a 6-dimension
  per-model comparison (mean score, pass^3, throughput, p95 latency, VRAM, max safe context),
  assembled CLIENT-SIDE from the real `languageStats()` and `contextProfiles()` methods (there is
  no dedicated `/mint/tradeoffs` backend endpoint -- CONST-24's original version read one, but it
  was a mock-only fixture with no real contract behind it).
- **Context degradation** (`ContextDegradationSection.tsx`, folded into `mint.categories`,
  rendered only for the `context` legacy category) -- throughput/recall over context-token
  tiers, OOM markers, and a `max_context_safe` hairline, wired to the real `contextProfiles()`
  method. Sibling charts, never a dual-axis chart.

What was reviewed and NOT ported, because CGUI-10's existing pair already covers the same ground
live: the CONST-23/24 filter bar (`MintFilterBar.tsx`/`mintFilters.ts` -- its model/category
facets were backed by a mock fixture, and its language filter was explicitly a documented
stopgap "swap for a real facet list once CONST-21 lands"; CGUI-10 instead uses a per-panel
category picker + epoch selector against the real client), its own Overview/Coverage sections
(redundant with `OverviewPanel.tsx`'s headline metrics + coverage roll-up), its Capability
radar (redundant with the persona radar already in `CategoryReportPanel.tsx`), and its
failure-class bar chart / score-beeswarm Coder section (redundant with `CategoryReportPanel.tsx`'s
existing Failures + Distribution sections for the `code` category). `MintPage.tsx` and every
CONST-23/24 section file were deleted once their useful chart types were folded in or explicitly
rejected, so the module carries no dead duplicate UI.

## Model Library module (`models`, CONST-21 API + CGUI-09 roster/detail + CONST-22 compare, spec §6)

A `terminus`-backed module (its `ModuleDescriptor.healthSystem` is `'terminus'` — there's no
separate `/api/health` entry for it, same convention as the `terminus` module itself), 100%
wired to the real Terminus models API (`GET /api/terminus/models*`, `GET /api/terminus/mint/
dimensions`, CONST-21 + CGUI-07) via the CGUI-08 data client (`getAggregationClient().models.*`
/ `.mint.*`) and its checked response types in `src/types/mint.ts` (mirrors `models_api.rs`'s
`json!({…})` shapes 1:1). There is exactly one registered panel each for the roster, detail, and
compare surfaces — see below for how the module's two build lineages (CGUI-09's roster/detail,
CONST-22's compare) were reconciled into that single set.

- **`models.roster`** (`/models/roster`, `src/panels/models/RosterPanel.tsx`) — the module's
  primary surface: header `MetricCard` row (models / serving now / in current fleet), a
  `Toolbar` (search, All/Fleet/Brochure scope segments, serving-only toggle, table/card view
  toggle + result count), server-side `limit`/`offset` pagination over the full roster, and a
  dense sortable `SortableTable` (default view) or a rich card grid — both driven by
  `client.models.list()`. Clicking a model name/card swaps the panel to `ModelDetailView` inline
  (a master-detail swap within the same route, not a separate `/models/:name` route — there is
  no `models.detail` panel registration; `ModelDetailView` is rendered directly by `RosterPanel`
  when a row is selected). Review fix: a leading checkbox per row (table view) / per card (card
  view) selects up to 4 models for comparison; a "Compare (N)" button appears in the toolbar
  once ≥1 is selected (enabled at ≥2) and navigates to `models.compare` with the selected names
  as `?m=` params — Compare was originally reachable only by hand-constructing the URL.
- **`ModelDetailView`** (`src/panels/models/ModelDetailView.tsx`, no route of its own) —
  per-model detail via `client.models.model(name)`: a per-category pass-rate radar
  (`src/viz/RadarChart.tsx`, lazy-loaded so the roster never pays for the `@nivo/radar` chunk),
  Identity/Brochure/Serving/Operational fact cards, and a full per-category metrics table. Fails
  open throughout — a 404/network error degrades to an inline "unavailable" notice, never a
  thrown/blank panel.
- **`models.compare`** (`/models/compare?m=a&m=b…`, `src/panels/models/ComparePanel.tsx`, 2–4
  models) — URL state ONLY, no `client.prefs` entry. A capability the roster/detail surface
  doesn't have at all, added as a pure addition rather than replacing anything: a side-by-side
  `DataTable` (best value per row outline-ringed, never color-alone), a MINT dimension radar
  overlay (≤4 series via `SlotAssigner`, `src/viz/CompareRadarChart.tsx` — a new lazy nivo
  wrapper distinct from `RadarChart`/`RadarChartKit`/`MintRadarChart`, since none of those three
  fit a generic up-to-4-model overlay driven by the caller's own per-model colors), and a Pareto
  scatter (VRAM vs. best pass-rate) with the compared models emphasized and the rest of the
  fleet rendered in `--chart-deemphasis`. `low_confidence`/`n<=1` MINT scores always render the
  ⚠ affordance + a variance tooltip (`src/lib/mintCaveat.ts`) — never silently hidden, INCLUDING
  a null-`norm` (no-data) score (review fix: the table cell's early-return for a null score
  used to skip the caveat check entirely). The radar can't honestly plot a missing per-vertex
  value (it renders as 0, matching a real low score) — a caveat line beneath the chart names
  every low-confidence/no-data `(model, dimension)` point actually plotted, disclosing rather
  than hiding the substitution. VRAM/best-pass-rate fallback for the COMPARED models themselves
  comes from a small targeted `models.list({q:name})` lookup per compared name (review fix: the
  original single `limit:500/offset:0` "rest of fleet" fetch could silently miss a compared
  model past the first page on a roster larger than 500 — that broad fetch now backfills only
  non-compared models for the Pareto background). Compare
  was originally built against a bespoke mock data layer (`hooks/useModels.ts` +
  `types/models.ts`) that never wired to the real backend; it has since been ported onto the
  same real data client the roster/detail use (`getAggregationClient().models.*` /
  `.mint.*`, typed via `types/mint.ts`) — the bespoke hook/types files are retired. One field the
  panel originally read doesn't exist on the real `ModelDetailResponse` (`catalog.card` has no
  `best_pass_rate`); Compare's "Best pass-rate" row sources that value from the roster's
  `ModelListEntry.best_pass_rate` instead of fabricating the field.

Two small additive changes landed alongside this module (both backward-compatible, every
existing caller unaffected): `DataTable` gained an optional `onRowClick` prop, and
`PanelDescriptor` gained an optional `hideInRail` flag — set on `models.compare` since it's only
reachable via a Compare action + its own URL-state selection, never a bare rail link, and
`ModuleRail` filters it out of the nav list.

`src/viz/recharts.ts`'s barrel also carries `ScatterChart`/`Scatter`/`ZAxis` (for the Pareto
chart) — same "panels never import recharts directly" rule as every other chart in this app.

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

### Maestro Activity panel cadence (MACT-08, MUSE-128)

`src/hooks/useActivityFeedLive.ts` drives the Maestro Activity panel's data on a fixed per-tier
polling cadence -- live pane every 5s, stat tiles every 10s, history every 60s -- and stops
ENTIRELY (no timer of any kind) while the tab/route is not visible (`document.visibilityState`).
On repeated failures the live and tiles tiers double their interval up to a 30s cap; `history`
does not back off at all, because its cap is the larger of 30s and its own base, and its base is
already 60s. Polling does NOT fire an extra request on mount: each pane's `useMuseSection`
already fetches from its own mount effect, so the first poll is scheduled rather than immediate
(becoming visible again after a hide DOES poll immediately, since that data is stale). `ActivityPanel.tsx`'s `LivePane`/
`HistoryPane` and `ActivityTiles.tsx` all use it and render the resulting cadence as a
plain-text "polling every Ns" label next to their existing source label -- never a colour/
title-only signal.

**This item originally planned to ride the `/ws` relay** -- a periodic Muse `activity_tick`
change-signal, coalesced client-side, promoted to a "live" state when ticks were arriving. That
was rejected in review: the tick would have been a clock inside `ws.rs`'s `pipe()` loop, never
actually observing Muse, so receiving it would have proven only that the relay's own socket was
alive -- not that Muse was reachable or had changed. Its ~3s cadence was also tighter than every
one of the panel's own polling numbers above, so wiring it would have INCREASED backend load
(five Muse requests per tick for the stat-tile row alone) for zero additional information, on
top of a spurious dependency on Harmony's upstream WS leg for a feature that has nothing to do
with Harmony. `ws.rs` carries no functional change for MACT-08 -- only a doc comment recording
this finding (its "MACT-08 evaluated..." section) so the next person inherits it rather than
rediscovering it. The `source` envelope seam described in the "Real-time relay" section above
is untouched and remains the right extension point if Muse ever grows a real outbound event
source to fan in.

## Lumina Conversations panel (LGUI-07)

`lumina.chat` (`/lumina/chat`, `src/panels/lumina/ChatPanel.tsx` +
`ChatBubble.tsx` + `src/hooks/useLuminaChat.ts`) is a **single-conversation, v1** chat surface
per `docs/constellation/LUMINA-GUI-SPEC.md` §3.2 — there is no history-list API yet, so this
panel only ever holds the in-memory thread for the current tab session; refreshing the page
starts a new one.

- **Wire call**: the pre-existing, non-streaming lumina endpoint (spec §0.2), reached as
  `POST /api/lumina/v1/chat/completions` through the Terminus proxy (server-side bearer
  injection is LGUI-05's job — this panel already calls the right path/shape and works fully
  against the mock adapter today). Request body is the OpenAI-shaped `{messages:[{role,content}]}`;
  success response `{choices:[{message:{role,content}}]}`; errors reuse the constellation-wide
  `{error:{message,type}}` envelope.
- **No fake streaming.** The composer disables and shows a `StatusPill state="idle" label="thinking"`
  for the one round trip; there is no token-by-token animation anywhere in this code path.
- **`/deep` / `/quick` chips** are REAL router overrides (spec §0.1.4) — toggling one just
  prefixes the outgoing message with `/deep ` or `/quick `, exactly like typing it yourself;
  there is no client-side routing logic.
- **Error mapping** (`useLuminaChat`'s `ChatErrorKind`): `rate_limit_error` → inline amber
  "Daily turn budget reached"; `upstream_error` (or any thrown transport failure) → "Chord
  unreachable"; anything else → inline error text + a retry button that resends the last
  attempted message.
- **Session-idle divider**: a "session resumes · 30 min idle" divider renders between two
  consecutive messages whose client-side timestamps are more than `SESSION_IDLE_MS` (30 min)
  apart — cosmetic only, never gates the request.
- **Role gating**: `ChatPanel` reads `useAuthRole()` directly (same convention `RoleGate` uses)
  and renders a read-only placeholder card for a `'viewer'` session instead of the composer —
  the panel is registered `available: true` for everyone the module rail shows, per spec §2's
  "min role operator" being a UI-courtesy gate here, not a registry field (`PanelDescriptor` has
  no per-panel role).
- **Injection-safe rendering (XSS proof)**: `ChatBubble.tsx` never uses
  `dangerouslySetInnerHTML`. Message content is parsed by `src/lib/chatMarkdown.ts` — a tiny,
  dependency-free parser (bold/inline-code/fenced-code/http(s)-only links; no dependency was
  added, per spec) that only ever produces a plain-data token list, which `ChatBubble` renders
  entirely as React text content. A literal `<script>...</script>` in a reply (see the mock's
  `trigger:xss` fixture below) can only ever reach the DOM as the visible, inert characters
  `<script>...</script>` — there is no code path that turns untrusted content into markup.
  `src/lib/chatMarkdown.test.ts` is the dependency-free self-check for this (same convention as
  `commandMatch.test.ts` — no JS test runner is wired up in this repo yet; run directly via
  `npx tsx src/lib/chatMarkdown.test.ts`), including two assertions specifically proving the
  `<script>` tag round-trips as inert text tokens only.
- **Long replies** (4000+ chars) render in a bubble with its own `overflow-y: auto` and a fixed
  max height, so the transcript panel itself never grows unbounded.

**Mock fixtures** (`mockLuminaChatReply` in `src/lib/aggregationClient.ts`) key off substrings
in the composer text (case-insensitive) so every one of the above is reviewable with zero
backend — type one of these as (or within) your message:

| Trigger substring | What comes back |
|---|---|
| `trigger:ratelimit` | `{error:{type:'rate_limit_error', ...}}` |
| `trigger:upstream` | `{error:{type:'upstream_error', ...}}` |
| `trigger:other` | `{error:{type:'internal_error', ...}}` (the generic inline+retry path) |
| `trigger:xss` | assistant reply containing a literal `<script>alert(1)</script>` |
| `trigger:long` | a 4200+ char assistant reply |
| anything else | a short canned reply exercising **bold**, `` `inline code` ``, a link, and a fenced code block |

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

## Connectors page (`terminus.connectors`, RMCP-13 / TERM-624)

`/terminus/connectors` (`src/pages/Connectors.tsx` + `src/panels/connectors/`) is the operator
surface for the S132 OAuth/MCP connector door: who may connect, what each connector is scoped to
reach, and which live grants to cut off. Three tabs — **Connectors**, **Tool groups**,
**Sessions**.

### The resolved preview is the point

The most valuable element on the page is the per-client **resolved preview**: the concrete list
of tools that connector can reach right now, each row carrying the group and the pattern that
put it there. It turns "2 groups × 1 server" — an abstraction a human cannot verify — into a
list they can read before handing out credentials.

It is **the server's answer, always**. `ResolvedToolPreview` calls `rmcp_client_resolve`
(RMCP-07's single `effective(principal, client, catalog)`, the same function that gates
`tools/list` and `tools/call`), and the group editor's live preview calls `rmcp_group_preview`
(RMCP-06's matcher). **There is no pattern matching anywhere in `src/` outside the mock server**,
and a unit test in `src/lib/rmcpClient.test.ts` asserts that nothing under `src/panels/` or
`src/pages/` imports the fixture module. A TypeScript matcher that agrees with the server today
drifts tomorrow, and a preview that can disagree with enforcement is worse than none — an
operator would trust it and be wrong.

### One door, server-side authorization

All reads and writes go through `src/lib/rmcpClient.ts`, which calls the same `rmcp_*` Terminus
tools the CLI does, over ONE endpoint — `POST /api/terminus/rmcp/call` `{tool, args}`, reached
via the app's single backend seam (`getAggregationClient().request('terminus', …)`). No second
REST surface, no direct DB access from the web layer.

Authorization is entirely server-side. Records carry `editable` / `ownedByMe` flags (RMCP-12
ownership) and the UI uses them to avoid offering a control that would 403 — a **courtesy, never
an enforcement point**. Reads are already scoped server-side, so a delegated owner never receives
another owner's objects to hide in the first place, and a write is refused whether or not a
button was rendered. Mutating controls additionally sit behind `RoleGate` (viewer sessions), the
same cosmetic-plus-server-enforced pattern as every other panel.

### Behaviours worth knowing

- **Empty state guides creation.** No connectors ⇒ an explanation of what a connector is plus a
  "create the first connector" action, not a blank table.
- **The client secret is shown exactly once.** It lives only in `ClientCreateDialog`'s component
  state, for the life of the created step, with a copy control, an explicit "this is the only
  time you will see this", and an acknowledgement checkbox gating the Done button. No read tool
  returns it (the store holds only an argon2id hash), and it never touches the `prefs` seam —
  see the no-secrets-in-browser rule above.
- **Absence is denial, and the UI says so in those words.** A client with no groups, no servers,
  or neither reads as "reaches nothing"; `scopeSummary` never renders an unscoped client as
  "all" or "unrestricted" (unit-tested).
- **A down upstream is a STATE, not an error.** A namespace whose upstream is not answering
  renders as `unavailable` (amber), with its tools still listed as in-scope. The config is
  correct and the mesh is not; painting that red sends the operator to fix the wrong thing.
- **Concurrent edits surface as a conflict.** Every write carries the `version` the form was
  loaded at. On `version_conflict` the editor shows the conflict and offers a reload — it does
  **not** retry with a fresh version, because that is exactly how one operator silently reverts
  another.
- **Large catalogs are bounded twice.** The resolve call takes `limit`/`offset` (bounded on the
  wire) and the table pages 25 rows at a time (bounded in the DOM); a server-capped result is
  labelled as capped rather than implying the connector reaches only that many tools.
- **Revocation is per-row and bulk**, from the Sessions tab or from inside a connector, with a
  confirmation that names what stops working and states that it takes effect at the next
  dispatch (not at token expiry). Revoked rows stay visible, marked — the list is an audit
  surface as much as a control.

### Backend readiness (`src/lib/rmcpFixtures.ts` — the one mocked boundary)

The backing `rmcp_*` tools land in RMCP-05/06/07/08/11/12, in parallel with this page. Until
they are deployed, every call answers `tool_unavailable` and the page renders an explanatory
"not live on this server yet" state — the posture `ActivityPanel` took toward CONST-26's
endpoint — never an error page and never invented data.

`rmcpFixtures.ts` is a **fixture SERVER**, reached only from `rmcpClient.ts`'s single dispatch.
It exists so the page could be built and tested before its tools; its pattern matcher is the mock
server's, which is why it is allowed to exist at all. Swapping to the live tools requires no code
change.

**It cannot ship.** Not by convention — structurally. The only reference to it is a dynamic
`import()` behind a literal `!import.meta.env.PROD` guard, which Vite folds to `false` at build
time, so the module never enters a production bundle's graph. `scripts/assert-http-bundle.mjs`
(the last step of `npm run build`, so there is no unguarded build path) then asserts the fixture's
marker string is absent from every emitted JS asset and **fails the build** if it reappears —
verified by deliberately re-introducing a top-level import and confirming the build fails.

The consequence is deliberate: in a production bundle a runtime `?mock` opt-in gives the rest of
the app fixtures but gives this page nothing — its calls go to the real endpoint and report
`tool_unavailable` if it is not there. Connector scoping is the one surface where plausible-looking
fake data is worse than an empty page, because the data *is* the authorization answer.

**The fixture is at least as strict as the real server, never laxer.** Its principal is a
*delegated owner* (it owns the `media`/`home`/`workshop`/`notes` namespaces and the objects built
on them, and does not own `studio` or the client, group, and session behind it), and it enforces
RMCP-12 ownership on every read and write: another owner's clients/groups/sessions are not
enumerated at all, a direct call naming one is refused, and scoping a client to an unowned
namespace is refused at write. That is what makes the delegated-owner tests meaningful — they
assert the *fixture* refuses, not that the UI hid something. An early version that only hid would
have let a UI-only "enforcement" pass its own tests.

## Lumina Memory browser (LGUI-08)

`lumina.memory` (route `/lumina/memory`, spec §3.3 "Engram browser") — the operator-facing
browser over the assistant's engram store. **v1 is read-only end to end: no delete/edit
affordance exists anywhere in `src/panels/lumina/{MemoryPanel,MemoryDrawer}.tsx`.**

- **Own type/data seam, deliberately not sharing LGUI-06/07's files** — `src/types/
  luminaMemory.ts`, `src/hooks/useLuminaMemory.ts`, `src/panels/lumina/memorySearch.ts` are new
  files rather than extensions of the unmerged sibling branches' `types/lumina.ts` /
  `useLumina.ts` (filename-collision avoidance per this item's brief); reconciling the two
  `EngramStats`-shaped types happens at merge time, not here.
- **Filter row** (query, `memory_type`, `sensitivity`, `visibility`, admin-only user scope,
  limit) is **server-side only** — `useLuminaMemory` always re-issues `GET /api/lumina/engram/
  search?...` on a filter change; it never fetches an unfiltered dump and slices it client-side.
  The mock adapter's own simulation of that server-side filtering (`mockEngramSearch` in
  `aggregationClient.ts`) reuses the exact same `applyMemorySearchParams` helper
  `memorySearch.ts` exports — one filtering implementation, exercised by both the mock route and
  `memorySearch.test.ts`.
- **Badges** (§5): `MemoryTypeBadge` — fixed tone map violet=Principle, blue=Semantic,
  green=Preference, neutral=Episodic (`MEMORY_TYPE_TONE`, `memorySearch.ts`), with a
  `MemoryTypeLegend` in the panel header so the mapping is always visible, never memorized.
  `SensitivityBadge` — `Health`/`Finance`/`Personal` (`isAlwaysPrivate`,
  `types/luminaMemory.ts`) ALWAYS render a 🔒 lock glyph, independent of the record's actual
  `visibility` value.
- **Results `DataTable`** → row click opens `MemoryDrawer` with the full `Memory` record
  (embedding is never present in the type at all — not even as an optional field — so
  rendering one is a type error, not a runtime slip), provenance (conversation/turn/source),
  and a `superseded_by` link that re-points the drawer at the replacing record
  (`supersededChain` in `memorySearch.ts` is cycle-safe for malformed/mock data).
- **Stats strip**: total, by-type mini bars, DB size (`formatBytes`), embedding coverage %, and
  store health. A `store_ok: false` (or a `SecurityViolation` on open) renders an error card
  naming only the offending key's **ENV NAME** (e.g. `ENGRAM_DB_KEY`) — S7 secrets discipline,
  never a value, never a GUI write path.
- **Mock fixtures** (`aggregationClient.ts`): 18 seeded `Memory` records covering all 4 types,
  6 of the 7 sensitivity categories (incl. the always-private set), a superseded chain
  (`mem-006 → mem-002`), and one deliberately huge-content record (`mem-014`) to exercise
  `clampPreview`'s 2-line/240-char preview clamp independent of CSS `line-clamp` alone.
- **`DataTable` gained one additive, opt-in prop** (`onRowClick?: (row: T) => void`,
  `src/components/DataTable.tsx`) to support the row → Drawer interaction — every existing
  caller that doesn't pass it renders exactly as before.
- Empty store → onboarding pointer copy (links to `/lumina/setup` in prose, no route change).
- Gating follows the `ChatPanel.tsx`/`RoleGate` convention: `PanelDescriptor` has no `minRole`
  field, so `MemoryPanel` itself checks `useAuthRole()` and renders a read-only placeholder for
  a viewer session — cosmetic only, same as everywhere else in this app; the server enforces
  the real 403.

## Dev / build

```sh
npm install
npm run dev        # vite dev server, :5174, proxies /api and /ws to :3100 by default
npm run typecheck  # tsc --noEmit
npm run test       # vitest run — fleetRingBuffer.test.ts (CONST-28), memorySearch.test.ts (LGUI-08)
npm run build       # tsc --noEmit && vite build -> dist/
```

The app talks to the real backend (http adapter) by default in any browser. To force offline
fixtures during dev, opt into mock explicitly: `VITE_AGG_MODE=mock npm run dev`, or append
`?mock` to the URL, or set `localStorage['constellation.aggMode']='mock'`.

## Embedded build (CONST-15)

`dist/` is **committed** into the repo (not gitignored) and embedded directly into the
`terminus_primary` binary via `include_dir` (`src/constellation/assets.rs`,
`include_dir!("$CARGO_MANIFEST_DIR/constellation-web/dist")`). This is deliberate: the fleet's build-on-dest pipeline
(`constellation-updater`, moosenet-spec v3.23) runs a **cargo-only** build on the deploy
host with no npm/node toolchain — the committed dist is what makes that possible. The
embedded UI is served same-origin by the binary in production, so it talks to the real backend
(the http adapter is the default — S127 TGUI2; no `VITE_AGG_MODE` flag is required, and a
mock-only bundle can no longer ship silently).

**Whenever the UI changes, rebuild and recommit `dist/`:**

```sh
npm run build:verify   # tsc + vite build, then asserts the bundle can reach the http adapter
git add -f constellation-web/dist
```

`CONSTELLATION_WEB_DIST_DIR` remains available as an optional filesystem override for local
dev against a live-reloading build — when set, the binary serves from that directory
instead of the embedded assets (see `src/constellation/mod.rs`).
