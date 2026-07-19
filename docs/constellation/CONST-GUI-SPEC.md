# Constellation Web GUI — Full Product & Build Spec
plane_project: TERM
module: Terminus
prefix: CONST
spec_id: S119-constellation-gui-v2

> RECONSTRUCTION NOTE (2026-07-19, S121 takeover orchestrator): this file was authored
> 2026-07-18 but only ever existed UNTRACKED in the dev-box main checkout; a concurrent
> session's cleanup deleted it before it was committed. This is a faithful reconstruction
> from the takeover orchestrator's full read of the original (same session it was deleted
> in). The companion `CONST-GUI-audit.md` was deleted before ever being read by this
> session and could NOT be reconstructed — re-export it from the workspaces share if
> needed. Item ACs and section content below match the original v1.1; minor whitespace
> drift from the original is possible.

## Metadata
- **Author:** Fable (spec agent), for <operator> (Moose)
- **Session:** S119
- **Date:** 2026-07-18 · **Spec version:** v1.1 (reconstructed 2026-07-19)
- **Changelog:** v1.1 (2026-07-18) — reconciled with the authoritative **Terminus GUI Brand
  Guide** ("Lumina Constellation Web GUI System · Terminus. · v1.0 portal", Claude Design;
  lives on the workspaces share as `Terminus GUI Brand Guide.html`, NOT in this repo). The
  brand guide WINS over the code-extracted harmony-web tokens for all visual identity (color,
  type, wordmark, spacing, radius, elevation, motion, tone); §2 is rewritten to the brand
  system, §3.1 adopts its two-tier border nav + card canvas, §4/§7 viz palettes re-derived
  from the brand hues with a new validation target, and CONST-16/17 scopes/ACs updated so
  builders produce the brand look, not the old harmony-web look. Architecture, module
  contract, data contracts, and decomposition structure unchanged. v1.0 (2026-07-18) —
  initial spec.
- **Module version:** Terminus main (constellation foundation CONST-01/02/03/04/15 merged + deployed)
- **Estimated total:** ~82h autonomous agent work across 14 items (CONST-16..29)
- **Context:** Constellation is the primary Terminus-served web front-end for the whole fleet.
  The foundation (shell, aggregation layer, auth, embedded dist) is LIVE. This spec finishes the
  product: a module-registry shell hosting Harmony (the ported reference app), Chord, Lumina,
  Muse, Terminus-self, plus two new flagship modules — the **Model Library** and **MINT Test
  Results** (the charts showcase) — with a codified design system, real-time layer, command
  palette, and role gating. Companion ground-truth audit: `docs/constellation/CONST-GUI-audit.md`
  (LOST — see reconstruction note).
- **Prefix note:** `CONST` is already registered (S97-constellation-gui, Plane TERM CONST-01..15 =
  issues #318–#331, #336). This spec **continues that claim** — items number from CONST-16.
  CONST-05..14 (config-write panels, provider panels) remain open under S97 and are NOT
  duplicated here; §11.4 states the reconciliation.

## Pre-flight
- Repository: `moosenet/Terminus` on the internal forge (existing; `constellation-web/` +
  `src/constellation/` present on main)
- Working directory: the Terminus checkout on the dev box; UI builds (`npm ci && npm run build`)
  run on the dev box (small, no cargo); **Rust builds/test-gates go through the compiler tool**
  (`compiler_build(module=terminus, ref=<branch>, mode=test)`) per skill v4.2 — never ad-hoc
  cargo on a shared host
- Vault/<secret-manager> secrets required (already provisioned unless noted):
  `CONSTELLATION_OPERATOR_SECRET`, `TERMINUS_JWT_SIGNING_KEY`; NEW (operator adds when CONST-27
  lands): `CONSTELLATION_VIEWER_SECRET`
- Non-secret env (crate::config helpers): `CONSTELLATION_{HARMONY,CHORD,LUMINA}_URL` (Harmony/
  Lumina still unset — panels degrade until the operator provides reachable URLs); NEW:
  `CONSTELLATION_MUSE_URL`, `CONSTELLATION_ACTIVITY_TAIL_LIMIT`
- Infrastructure: internal forge + Plane reachable via the sanctioned Terminus tools; the intake
  Postgres (`INTAKE_DATABASE_URL`) reachable from the terminus-primary host for CONST-21
- Baseline tests: current `cargo test --workspace` green modulo the known environmental-failure
  set (compiler `mode=test` runs unfiltered; pre-existing env failures are expected in-sandbox
  and are NOT regressions)
- Baseline verify: `npm run typecheck && npm run build` green in `constellation-web/`

---

# §1 Product vision & information architecture

## 1.1 What Constellation is

**Constellation is the fleet's single pane of glass** — the primary web GUI, served by the
terminus-primary binary (embedded SPA, CONST-15), through which the operator observes and
controls every constellation service. It is a **control plane, not a dashboard** (S97 charter):
panels carry active configuration and controls, not just read views. The browser talks ONLY to
Terminus (`/api/*` aggregation, one auth boundary, secrets masked server-side).

## 1.2 The three-level IA: Shell → Module → Panel

- **Shell** — auth gate, sidebar nav, status strip, command palette, notifications, theming.
  Owns zero domain knowledge; renders whatever the registry reports.
- **Module** — one fleet system's presence in the GUI: `harmony`, `chord`, `lumina`, `muse`,
  `terminus` (self), `models` (Model Library), `mint` (MINT Test Results) — plus future ones.
  A module = a **ModuleDescriptor** (§1.3) grouping panels, binding availability to a health
  source, and declaring its data namespace.
- **Panel** — one routed screen inside a module. The existing `PanelDescriptor` contract
  (`constellation-web/src/lib/moduleRegistry.ts`) is kept **unchanged** — CONST-16 layers
  modules above panels; every already-registered panel keeps working.

Navigation IA follows the brand guide's **two-tier border nav**: the **global bar** (top)
picks the module — **Overview** · **Harmony** · **Chord** · **Lumina** · **Muse** · **Models**
· **MINT** · **Terminus** — and the **left module rail** picks the panel within it. "The
frame never moves; only the canvas does." Module tabs render only when their module reports
available (health-driven, §1.3); an absent backend removes the tab rather than showing dead
links.

## 1.3 The module contract (normative)

```ts
// constellation-web/src/lib/moduleRegistry.ts — CONST-16 addition (PanelDescriptor unchanged)
export interface ModuleDescriptor {
  /** Stable id and data namespace: 'harmony' | 'chord' | 'lumina' | 'muse' | 'terminus'
   *  | 'models' | 'mint' | future ids. For proxied systems this doubles as the
   *  aggregation SystemId; for terminus-backed modules (models/mint/terminus) the data
   *  source is the terminus namespace. */
  id: string;
  /** Sidebar group title, e.g. "Model Library". */
  title: string;
  icon: string;
  /** Which /api/health system-entry gates this module's availability; 'terminus' modules
   *  (models/mint/terminus) bind to the always-available terminus self entry. */
  healthSystem: 'harmony' | 'chord' | 'lumina' | 'muse' | 'terminus';
  /** Fixed sidebar order (stable across health flaps — modules never reorder at runtime). */
  order: number;
  /** Minimum role that may see this module at all (default 'viewer'); mutating controls
   *  inside panels additionally gate on 'operator' (§3.4). */
  minRole?: 'viewer' | 'operator';
}
export function registerModule(m: ModuleDescriptor): void;
export function getAvailableModules(health: HealthStatus[]): ModuleDescriptor[];
```

Rules: (1) a panel's `system` field maps to a module id — the legacy `SystemGroup` union widens
to the module-id set and 'Providers'/'Status' panels re-home under `terminus` and `harmony`
respectively; (2) module availability = registered AND its `healthSystem` entry reports
`available:true` (from the existing 30s `/api/health` poll in `App.tsx`) — stale-while-degrading:
a module that flaps down keeps its nav entry but renders panels in their degraded state for 2
poll cycles before hiding; (3) registration stays import-time side-effect in `registerPanels.ts`
— no dynamic plugin loading (the SPA is one embedded bundle; module federation rejected §8.6).

## 1.4 Module list (this spec's scope)

| Module | Source of data | State today | This spec |
|---|---|---|---|
| Overview | `/api/health` + per-module summary endpoints | StatusStrip only | Fleet dashboard panel (CONST-16) |
| Harmony | `/api/harmony/*` proxy | 8 panels ported, live | Keep; re-home under module registry; ws relay makes it fully live (CONST-17/18) |
| Chord | `/api/chord/*` proxy | 3 panels ported, live | Keep; re-home |
| Lumina | `/api/lumina/*` proxy | stub, `available:false` | Health-gated module; read dashboard rides S97 CONST-07 (superseded by LUMINA-GUI-SPEC) |
| Muse | `/api/muse/*` proxy — **new namespace** | none | CONST-19 backend + CONST-20 UI |
| Models | `/api/terminus/models*` — **new read API** | none | CONST-21 API + CONST-22 UI |
| MINT | `/api/terminus/mint/*` — **new read API** | none | CONST-21 API + CONST-23/24 UI |
| Terminus | `/api/terminus/config` (+ new activity/tools endpoints) | 1 config panel | CONST-26 feed + CONST-28 fleet/tools panels |

---

# §2 Design system — the Terminus brand ("deep space" portal)

**Authority:** the Terminus GUI Brand Guide (Claude Design, v1.0 "portal") is the visual source
of truth. Where it differs from the code-extracted harmony-web tokens currently in
`constellation-web/src/styles/globals.css`, **the brand guide wins**; the audited harmony-web
system remains the reference for interaction *patterns* (skeletons, degradation, table skins)
but NOT for color/type/spacing/motion. Engineering approach unchanged: **CSS custom properties
+ inline style objects — no Tailwind utilities** (builders MUST NOT introduce Tailwind classes).
CONST-17 REPLACES the token sheet wholesale; §2.5 lists every changed token.

## 2.1 Canonical token sheet (brand-derived; `constellation-web/src/styles/globals.css`)

```css
/* ── Surfaces — "deep space" (page → chip, darkest to lightest) */
--space-900:#0D0B1A; --space-800:#110E22; --space-700:#161130;
--space-600:#1A1333; --space-500:#221A40; --space-400:#2C2350;
--bg-page:var(--space-900); --bg-panel:var(--space-700); --bg-elevated:var(--space-600);
--bg-hover:var(--space-500); --surface-card:var(--space-700); --surface-chip:var(--space-400);
--grad-space:linear-gradient(135deg,#0D0B1A 0%,#1A1333 100%);
--grad-card:linear-gradient(180deg,var(--space-700),var(--space-800)); /* card fill */

/* ── Brand accent — violet ramp */
--violet-700:#5B21B6; --violet-600:#6D28D9; --violet-500:#7C3AED;
--violet-400:#A855F7; --violet-300:#C4A5FB; --violet-200:#DDC9FD;
--accent:var(--violet-500); --accent-bright:var(--violet-400); --accent-on:#FFFFFF;
--grad-accent:linear-gradient(135deg,#7C3AED 0%,rgba(124,58,237,0.6) 100%);

/* ── Functional ("flux") hues — SEMANTIC, never decorative (§2.4) */
--flux-blue:#3B82F6;  --flux-blue-soft:#60A5FA;   /* inbound · source · cold tier */
--flux-green:#10B981; --flux-green-soft:#34D399;  /* outbound · endpoint · free cost */
--flux-amber:#F59E0B;                             /* cloud · gated · paid cost · warm */
--flux-rose:#F43F5E;                              /* alert · error · hot tier */
--node-source:var(--flux-blue); --node-core:var(--violet-500);
--node-endpoint:var(--flux-green); --node-cloud:var(--flux-amber);
--tier-hot:var(--flux-rose); --tier-warm:var(--flux-amber); --tier-cold:var(--flux-blue);
--cost-free:var(--flux-green); --cost-paid:var(--flux-amber);
--status-success:var(--flux-green); --status-warning:var(--flux-amber);
--status-error:var(--flux-rose);   --status-info:var(--flux-blue);

/* ── Ink */
--text-100:#F4F2FB; --text-200:#C7C3D6; --text-300:#9CA3AF;
--text-400:#6B7280; --text-500:#4B5563; --text-600:#374151;
--text-heading:var(--text-100); --text-body:var(--text-200);
--text-muted:var(--text-300);   --text-faint:var(--text-500);

/* ── Lines — violet-alpha hairlines */
--line-soft:rgba(168,85,247,0.14); --line-default:rgba(168,85,247,0.22);
--line-strong:rgba(168,85,247,0.40);
--border:var(--line-default); --border-strong:var(--line-strong); --border-width:1px;

/* ── Type — Inter UI + JetBrains Mono for code/labels/telemetry (both SELF-HOSTED woff2) */
--font-sans:'Inter',-apple-system,BlinkMacSystemFont,'Segoe UI',sans-serif;
--font-mono:'JetBrains Mono',ui-monospace,'SF Mono',Menlo,monospace;
--fs-display:68px; --fs-h1:44px; --fs-h2:32px; --fs-h3:24px; --fs-h4:19px;
--fs-body-lg:17px; --fs-body:15px; --fs-sm:13px; --fs-xs:11px;
--fs-mono:13px; --fs-mono-sm:11px; --fs-label:11px;
--fw-regular:400; --fw-medium:500; --fw-semibold:600; --fw-bold:700;
--lh-tight:1.1; --lh-heading:1.2; --lh-body:1.6;
--ls-display:0.04em; --ls-label:0.18em; --ls-mono:0.02em;

/* ── Space / radius / width */
--space-1:4px; --space-2:8px; --space-3:12px; --space-4:16px; --space-5:24px;
--space-6:32px; --space-7:48px; --space-8:64px; --space-9:96px;
--radius-xs:4px; --radius-sm:6px; --radius-md:10px; --radius-lg:14px;
--radius-xl:20px; --radius-pill:999px;
--maxw-prose:68ch; --maxw-content:1120px;

/* ── Elevation + glow (glow = brand emphasis, reserved: see §2.4) */
--shadow-sm:0 1px 2px rgba(0,0,0,0.4); --shadow-md:0 8px 24px rgba(0,0,0,0.45);
--shadow-lg:0 20px 60px rgba(0,0,0,0.55);
--glow-violet:0 0 24px rgba(124,58,237,0.45); --glow-violet-soft:0 0 12px rgba(168,85,247,0.30);
--glow-blue:0 0 18px rgba(59,130,246,0.45); --glow-green:0 0 18px rgba(16,185,129,0.45);
--glow-amber:0 0 18px rgba(245,158,11,0.45);
--inset-hi:inset 0 1px 0 rgba(255,255,255,0.04);

/* ── Motion */
--ease-out:cubic-bezier(0.16,1,0.3,1); --ease-in-out:cubic-bezier(0.65,0,0.35,1);
--dur-fast:140ms; --dur-base:240ms; --dur-slow:600ms; --dur-flow:3.2s;

/* ── Focus */
--focus-ring:2px solid var(--accent-bright);
```

Rules: fonts are **self-hosted woff2 in the repo** (Inter 400/500/600/700 + JetBrains Mono
400/500/700, latin subsets minimum) served from the embedded dist — no hosted-fonts `@import`
(the app must load with zero external requests; same-origin model). Legacy `--h-*` aliases and
old-name tokens are kept ONE release as aliases onto the new values, then removed in CONST-29.
Theming is **dark-only** — deep space IS the brand. No component may hardcode a hex that has a
token (grep-gated).

## 2.2 Identity: wordmark, imagery, voice

- **Wordmark:** "Terminus" in Inter 700, letter-spacing −0.02em, with the terminal period in
  `--accent-bright` — "Terminus**.**". Product eyebrow: two JetBrains Mono 11px uppercase
  labels tracked `--ls-label` ("LUMINA CONSTELLATION" `--text-300` · "WEB GUI SYSTEM"
  `--violet-300`) separated by a 4px glowing violet dot.
- **Imagery:** the starfield — sparse 1px radial-gradient "stars" over `--grad-space`, with a
  ≤1-opacity twinkle (`--dur-flow` scale) — is reserved for the login screen and Overview
  header band only; never behind data panels. No illustrations, no photography.
- **Voice/tone:** operator-terminal register. Section/eyebrow labels are tracked-mono
  uppercase ("CALLS/H", "P50"); numbers and telemetry always in `--font-mono`; log-line motif
  `[ok] … cost=0.00` for live feeds; errors state facts + next action, no apologies-theater.

## 2.3 Component kit (brand-normative styling; existing files restyled, APIs kept)

| Primitive | Lands in | Brand styling (normative) |
|---|---|---|
| **Card** | `src/components/Card.tsx` (same 4 variants + new `glow`/`accent` props) | fill `--grad-card`; 1px `--border` (accent: `--line-strong` + violet gradient border-mask); `--radius-lg`; `--shadow-md` + `--inset-hi`; interactive hover = translateY(−2px) + `--glow-violet` + border → `--violet-400`, `--dur-base --ease-out` |
| **Button** | `Button.tsx` (variants primary/secondary/ghost/danger; sm/md/lg) | primary = `--grad-accent` + `--accent-on` ink + `--glow-violet-soft`; secondary = space-600→700 gradient + `--border-strong`; ghost = transparent (hover `--bg-hover`); danger = rose-tint gradient + rose 45% border; active = translateY(1px) scale(.99); disabled 0.45 opacity |
| **Badge** | `Badge.tsx` (tones violet/blue/green/amber/rose/neutral, optional glow dot, `mono` flag) | pill radius; tone at 14% bg-tint + ~32% border + soft ink; mono variant for cost/tier badges |
| **StatusPill** | `StatusPill.tsx` (states online/idle/error + hot/warm/cold) | mono 11px uppercase pill on `--space-700`; 7px state-color dot with 8px glow + `lumina-ping` expanding ring (1.8s); idle = muted dot, NO ping |
| **NodeBadge** | new `src/components/NodeBadge.tsx` | kind-colored glowing 9px dot (source/core/endpoint/cloud per §2.4) + bold mono name + muted role line, kind-tinted gradient chip, `--radius-md`; optional `lumina-corepulse` (3.2s) on the active core |

Shared primitives restyled to the brand: `MetricCard`, `DataTable`, `Drawer`/`CommandPalette`/
`Toast` (surfaces `--bg-elevated`, `--shadow-lg`), `ConfirmDialog`, `RoleGate`, `ChartCard`.

## 2.4 The semantic-color law (enforced in review)

**"Color is always semantic — the node dot and cost never decorate."** violet = core/brand/
primary action; blue = inbound/source/cold; green = outbound/endpoint/free; amber =
cloud/gated/paid/warm; rose = alert/error/hot. (1) a UI element may use a flux hue ONLY when it
means that semantic; (2) glows are emphasis for live/primary elements, never ambient decoration;
(3) charts: a series that IS a semantic entity wears that semantic token; only non-semantic
identity uses the categorical slots (§4.2); (4) status tokens are never series colors and vice
versa. A PR that colors decoratively is CHANGES_REQUESTED on §2.4 alone.

## 2.5 Delta table — every token the rebrand changed (CONST-17, MERGED)

| Token (role) | Old (harmony-web) | New (brand) |
|---|---|---|
| Page bg | `#0f1117` | `#0D0B1A` (`--space-900`) |
| Card/panel bg | `#161b22` flat | `#161130` (+ `--grad-card` fill) |
| Raised / hover bg | `#1c2129` / — | `#1A1333` / `#221A40` |
| Overlay/chip bg | `#242b35` | `#2C2350` |
| Accent | teal `#5ce0d8` | violet `#7C3AED` (bright `#A855F7`) |
| Borders | white-alpha .06/.10/.16 | violet-alpha .14/.22/.40 |
| Text 1/2/3 | `#e6edf3`/`#8b949e`/`#6e7681` | `#F4F2FB`/`#C7C3D6`/`#9CA3AF` (+ faint `#4B5563`) |
| Status s/w/e/i | `#3fb950`/`#d29922`/`#f85149`/`#58a6ff` | `#10B981`/`#F59E0B`/`#F43F5E`/`#3B82F6` |
| UI sans | system stack | **Inter** (self-hosted) |
| Type scale | 11..24 | 11/13/15/17/19/24/32/44/68 (body 15; labels mono 11 tracked) |
| Spacing scale | 4..24 (6 steps) | 4..96 (9 steps; `--space-5` now 24px) |
| Radius | 4/6/8/12 | 4/6/10/14/20/pill (cards → 14) |
| Shadows | 2 soft | sm/md/lg + violet/flux glows + `--inset-hi` |
| Motion | 120/200ms ease | 140/240/600ms + `--ease-out`; flow 3.2s |
| Wordmark | harmony SVG | "Terminus." Inter 700 + violet period |
| Focus ring | teal | `--accent-bright` violet |

## 2.6 States, motion, accessibility (normative for every panel)

- **Loading:** skeletons sized to final layout; no spinner pages. **Refetch keeps the frame**
  (previous render at 0.6 opacity — never re-skeleton, never layout-jump).
- **Empty:** centered `--text-muted` message + one-line "how data appears here" hint.
  **Degraded:** `{available:false, detail}` renders the module-standard degraded card (icon +
  detail + retry) — never a crash. **Card states:** online = full opacity; idle = muted dot no
  ping; error = rose border tint + `[!!]` motif; disabled = 50% opacity inert.
- **Error (non-degraded):** inline `--status-error` + retry; toasts only for async mutations.
- **Motion:** `--dur-fast` hover / `--dur-base` structural, always `--ease-out`; sanctioned
  animated effects = ping, corepulse, twinkle (login/hero only), skeleton shimmer, flow
  streaks. ALL disable under `prefers-reduced-motion`; glows degrade to static shadow.
- **Accessibility:** `--focus-ring` on every interactive element; full keyboard reachability;
  ARIA nav landmarks, `aria-current`, `role="dialog"` + focus trap on palette/drawer/confirm,
  live-region toasts. Ink floors on `--bg-panel`: body ≥ `--text-200` (AA); `--text-300`
  smallest at 11px labels; `--text-400/500` decorative only. Every chart ships a table-view
  twin (§4.4); sub-3:1 fills get the relief rule. Ping/pulse never carry meaning alone.

---

# §3 Shell & cross-cutting features

## 3.1 The shell: two-tier border nav + card canvas (CONST-16, MERGED)

- **Global bar** (top): "Terminus." wordmark, module tabs from `getAvailableModules(health)`
  (active = `--accent-bright` underline; health dot per tab), palette trigger ("search… ⌘K"),
  density toggle (Comfortable | Compact), account chip.
- **Module rail** (left): active module's panels, grouped, live status dots; collapses to
  icons <1100px; drawer nav <760px.
- **Card canvas Overview** (`/overview`, default route): customizable canvas of module cards,
  seven-region card anatomy in fixed order: (1) drag handle + semantic node dot + module name,
  (2) StatusPill, (3) tracked-mono kind/role line, (4) metric row + cost/tier Badge,
  (5) last activity line (hidden in Compact), (6) enable/hide toggle + quick actions,
  (7) card body widget when expanded. Drag-reorder/remove/add ("+ Add widget"); layout +
  density persist per operator via the `client.prefs` seam — allowlisted localStorage,
  `layout` + `density` keys ONLY. Keyboard reorder (focus card → ⌘/Ctrl+arrows).
- The activity feed (§3.3) renders as a canvas widget; cards deep-link into modules.

## 3.2 Command palette & global search (CONST-25)

`Ctrl/Cmd+K` opens `CommandPalette` (in-house, zero new deps: `role="dialog"` overlay +
listbox; ~200 LOC). Sources in rank order: (1) navigation — every available panel; (2) actions
— palette-registered commands with role gating (operator-only mutations open ConfirmDialog);
(3) entity search — async, debounced 150ms, fans out through the aggregation client to cheap
existing list endpoints, grouped hits. Fuzzy match = subsequence with word-boundary bonus (own
util, no dependency). Keyboard: ↑/↓/Enter/Esc, Tab cycles groups. Panels register palette
entries via `registerCommand()` sibling of `registerPanel()`.

## 3.3 Notifications & activity feed (CONST-26)

Server: `GET /api/terminus/activity?limit=N` (protected) returns the masked tail of the
constellation audit log (JSONL, S6-sanitized at write; response passes `mask_response`) as
`{ts, method, path, principal, system}` entries — **no body content**. Client: the Overview
feed merges (a) activity entries, (b) health transitions from the 30s poll, (c) ws events
(CONST-18). Toasts: mutation results and health transitions only; auto-dismiss 6s;
`aria-live="polite"`; bell menu retains last 50 in memory (no browser storage).

## 3.4 Auth & roles (CONST-27, extends `src/constellation/auth.rs`)

CONST-27 adds a **`role` claim** to the same session JWT: login checks
`CONSTELLATION_OPERATOR_SECRET` (role `operator`) then `CONSTELLATION_VIEWER_SECRET` (role
`viewer`), both constant-time, both fail-closed when unset. Enforcement server-side and
structural: viewer gets `403 {"error":"forbidden","required_role":"operator"}` on every
mutating method under `/api/{harmony,chord,lumina,muse}/*` and operator-only terminus
endpoints — UI `RoleGate` is a courtesy, never the enforcement. `GET /api/auth/me` gains
`role`. No third role; no per-module ACLs.

## 3.5 Real-time (CONST-18, MERGED)

`/ws` relay: session-cookie-authenticated axum WebSocket endpoint, upstream to
`CONSTELLATION_HARMONY_WS_URL` (unset ⇒ typed close, UI stays on polling), pipes events with
`mask_response` on every JSON text frame. Envelope `{source:'harmony', event:...}`. Everything
else stays 30s polling.

---

# §4 Chart & data-viz standards (normative for every chart)

## 4.1 Library decision

**Recharts 3.8 stays** for what it renders well. **Advanced forms on nivo** (`@nivo/radar`,
`@nivo/boxplot`, `@nivo/heatmap`, `@nivo/parallel-coordinates`, `@nivo/swarmplot`,
`@nivo/scatterplot` — pinned). Rejected: ECharts, visx, observable-plot. **Panels never import
nivo/recharts directly** — they import from `src/viz/` (CONST-17's kit).

## 4.2 The viz kit (`constellation-web/src/viz/`, MERGED) — brand-derived palettes

```css
/* Categorical — 6 slots from the brand hues, fixed order, assigned in sequence,
   never cycled. Series CEILING IS 6 (fold to "Other" beyond); all-pairs forms cap at 4. */
--series-1:#A855F7; --series-2:#10B981; --series-3:#F59E0B;
--series-4:#3B82F6; --series-5:#F43F5E; --series-6:#DDC9FD;
/* Sequential (magnitude) — violet ramp, HIGH = LIGHT */
--seq-1:#5B21B6; --seq-2:#6D28D9; --seq-3:#7C3AED; --seq-4:#A855F7;
--seq-5:#C4A5FB; --seq-6:#DDC9FD;
/* Diverging — cold↔hot, neutral gray midpoint */
--div-cold:var(--flux-blue); --div-mid:#4B5563; --div-hot:var(--flux-rose);
/* Chart chrome */
--chart-grid:#221A40; --chart-axis:#2C2350; --chart-deemphasis:#4B5563;
```

**Semantic-series rule:** tier/cost/flow-role/health series wear their semantic token, NOT a
categorical slot; nominal identities (models, languages, providers) use `--series-1..6`.
`theme.ts` reads CSS vars at mount (memoized getComputedStyle) → nivo theme + Recharts
constants: 11px ticks `--text-muted` (`--font-mono` numeric), solid 1px grid `--chart-grid`
(never dashed), tooltip chrome `--bg-elevated` + `--border` + `--shadow-lg`. `palette.ts`:
slots per first-seen entity, KEPT across filtering. Shared: ChartCard, ChartTooltip
(textContent-only), ChartLegend (≥2 series), TableViewToggle, ChartEmpty, ChartSkeleton.

## 4.3 ChartCard contract

Card (content variant) + header (title 13px/600, subtitle, controls slot) + body (chart height
includes axis band) + footer (table toggle, caveats). Loading = ChartSkeleton at final height;
refetch = previous render 0.6 opacity; empty = ChartEmpty with provenance hint. Filters NEVER
inside a ChartCard — one filter row above the grid it scopes.

## 4.4 Interaction & accessibility floor (all charts)

Hover layer mandatory: crosshair+snap on time-series (one tooltip, all series); per-mark
tooltips ≥24px hit targets (nearest-point mesh on scatter/swarm); hovered mark lifts. Keyboard
focus reaches every mark group. Every chart has a **table view** (TableViewToggle → DataTable
of the same slice — the WCAG relief channel). Bars ≤24px, 4px rounded data-end, 2px gaps;
lines 2px round-capped; markers ≥8px with 2px rings; area fills ≈10%. Direct labels selective.
**No dual-axis charts anywhere.**

---

# §5 Per-module specs

## 5.1 Harmony module (ported — re-home + live wiring; CONST-16/17/18 MERGED)
Panels: Dashboard, Projects, Tasks, Agents, PRs, Prompts, Sessions, AuditLog + the Status pair
re-homed as `harmony.analytics` / `harmony.engine`. Re-skinned to the brand in CONST-17 (dashed
grids, raw-hex fills, teal accents retired; pass-rate donut → horizontal stacked bar).

## 5.2 Chord module (ported — keep)
Panels: Inference (VRAMGauge, ModelInventory, ModelDownload, LifecycleControls, StorageManager,
ProviderHealthCard), Providers (ProviderCard/Analytics/Summary, RoutingDiagram,
InferenceMixSlider), Playground. This spec adds: viz-kit re-theme, RoleGate on mutating
controls (playground run stays viewer-allowed), "Serving now" MetricCard row from
`/api/chord/health`. Deep config-write panels remain S97 CONST-05.

## 5.3 Lumina module
Health-gated stub registration only (`healthSystem:'lumina'`, CONST-16). Superseded by
`docs/constellation/LUMINA-GUI-SPEC.md` (S119-lumina-gui, Plane LUM).

## 5.4 Muse module (CONST-19 backend + CONST-20 UI)

Backend (CONST-19): fourth proxy namespace `/api/muse/*path` in `src/constellation/proxy.rs`
(+ `CONSTELLATION_MUSE_URL` config helper, + `muse` in `/api/health` probes, + `'muse'` in the
client `SystemId` union and mock adapter). Identical single-door/masking/audit/degradation
semantics.

UI (CONST-20) — three panels against Muse's verified routes:
- **`muse.dashboard`** — MetricCards (library size, active channels, pending items, last
  ingest) + On Deck rail (`GET /on_deck`) + Premieres list (`GET /premiere`) + Gaps summary
  (`GET /gaps`). Poster art via `GET /art/:kind/:id` (proxied — same-origin).
- **`muse.taste`** — cluster map from `GET /api/graph/taste-clusters` (scatter, first-4 series
  slots by cluster, >4 clusters fold to "Other"), watch-history stacked area
  (`GET /api/graph/watch-history`), group dynamics table (`GET /api/graph/group-dynamics`).
  All read-only.
- **`muse.channels`** — channels list + per-channel lineup (`GET /api/channels`,
  `GET /api/channels/:id/lineup`) with the guide grid (`GET /guide` data as a DataTable
  timeline, not an EPG widget); compose/maintenance actions operator-RoleGated + ConfirmDialog.
Degradation: MUSEX features exist unwired in production — every panel section degrades
per-endpoint (404/501 from one endpoint collapses that section to ChartEmpty "not yet wired",
never the whole panel).

## 5.5 Terminus module (self; CONST-28)
- **`terminus.fleet`** — per-system cards from `/api/health` history (client ring buffer of
  last 120 polls → uptime sparkline per system), worker/broker count, mesh/federation summary
  from `/api/terminus/config`.
- **`terminus.tools`** — tool catalog: module-prefix groups from an extended
  `/api/terminus/config` (per-module tool names + counts — additive, read-only introspection),
  searchable DataTable + palette entity source.
- **`terminus.activity`** — the full activity/audit tail view (CONST-26's endpoint, paged),
  filterable by system/method/principal.

# §6 Model Library module (`models`) — CONST-21 API + CONST-22 UI

Catalog of every model the fleet knows: the profiled fleet models (`model_fleet_catalog`) plus
the HF brochure (`model_discovery_candidate`), joined on `model_name`. Read-only in v1 —
curation actions stay in the MCP tools.

## 6.1 Panels

### `models.browse` (`/models`)
- **Header stat row** (MetricCards): fleet models · brochure candidates · serving-now count
  (`keep_warm`) · catalog `refreshed_at` (amber if > 7 days).
- **Filter row**: scope Fleet | Brochure | All; search `q`; category (8-value enum); brochure
  status; size bucket (<4B / 4–10B / 10–35B / >35B); coverage; "serving now" toggle.
- **Body**: DataTable (default) — Model · Family/Params · Quant · Category · Status badge ·
  Coverage strip (4 mini-cells) · Best pass-rate · VRAM · Last run · discovery_score sparkbar.
  Card-grid toggle. Row click → detail. Checkbox per row (max 4) → Compare. Server-side
  pagination (`limit/offset`, default 50). Empty state per scope.

### `models.detail` (`/models/:name` — URL-encoded full registry key)
Sections, each degrading independently: 1. **Identity** (advisor fields, quants table,
best_for/avoid_for chips); 2. **Provenance** (hf_repo link-out, brochure status + state
timeline, discovery_score, rationale); 3. **Deployment** (per serving_profile row: backend_tag,
tok_s, vram peak, cold_load_s, keep_warm, exclusion_reason; operational profile: max_context
safe/absolute, degradation point, throughput strip, tier); 4. **MINT profile** (radar thumbnail
vs fleet median, coverage matrix row expanded, link into MINT pre-filtered).

### `models.compare` (`/models/compare?m=a&m=b…`, 2–4 models — URL state only)
Side-by-side DataTable (best value ringed, never color-alone); radar overlay (≤4 series);
Pareto scatter with compared models emphasized. Persistent collections deferred.

## 6.2 States & rules
Series colors follow model identity (slot per first-selection order). `tabular-nums` in
tables. `low_confidence` and `n_samples ≤ 1` always render the ⚠ affordance + tooltip —
never silently hidden.

---

# §7 MINT Test Results module (`mint`) — CONST-21 API + CONST-23/24 UI

## 7.1 Layout & global filters
`/mint` = one page, sectioned (Overview → Coverage → Capability → Coder deep-dive → Context →
Runs), sticky in-page section nav. **One filter row on top scopes everything**: epoch (default
current), task_category, backend_tag, model multi-select (≤4, drives emphasis/series),
language (Coder section only — the one documented scoping exception). Deep links encode
filters in the query string.

## 7.2 The charts
- **C0 — Overview stat tiles**: models profiled · runs this epoch · fleet-best by pass_hat_3 ·
  GPU-hours · current epoch. Tiles deep-link.
- **C1 — Capability radar** (@nivo/radar): 8 assistant dimensions, ≤4 models + fleet median in
  `--chart-deemphasis`; vertex tooltips (value, raw, ±std_dev, n, ⚠); axis click → Drawer
  breakdown; missing dims at 0 with hollow vertex + caveat.
- **C2 — Score/coverage heatmap** (@nivo/heatmap): rows = models, cols = test_type ×
  task_category; fill = pass_rate on `--seq-1..6` (high = light); not_run = surface cell "—";
  stale = 55% + clock glyph; non_viable = de-emphasis + ✕ (glyph+tooltip+table, never color
  alone); cell click → drill-down.
- **C3 — Latency box plots** (@nivo/boxplot): horizontal, per model, single hue `--series-1`,
  server-side quartiles + outliers; log-scale toggle default on; n<5 → beeswarm strip + ⚠.
- **C4 — Quality × latency Pareto scatter** (@nivo/scatterplot): x = mean_latency (log), y =
  mean_score, size = vram (√, 8–24px); Pareto front = 2px step line `--accent-bright` +
  selective direct labels; selection → slots 1..4, rest de-emphasized; nearest-point mesh.
- **C5 — Score beeswarm** (@nivo/swarmplot): per-run judge scores per model (≤4), 8px dots,
  lane median tick; failure_class≠none → hollow dots; >400/lane decimates with caption.
- **C6 — Failure-class bars** (Recharts h-stacked): top-4 classes → slots 2/3/4/5, "Other" →
  de-emphasis; `none` excluded; segment click filters C5.
- **C7 — Context degradation lines** (Recharts): x = context_tokens (log2 ticks), y =
  throughput; sibling chart for recall (never a second axis); max_context_safe hairline;
  OOM ✕ markers; shared crosshair.
- **C8 — Sweep activity** (Recharts stacked area 10%): runs/day by suite + epoch marker
  hairlines; range presets 30d/90d/all.
- **C9 — Parallel coordinates** (@nivo/parallel-coordinates): 6 dims (mean_score, pass_hat_3,
  mean_throughput, p95_latency inv, vram inv, max_context_safe), normalized server-side, real
  units on ticks; selected ≤4 in series colors 2px, rest 1px de-emphasis; axis brush filters;
  <2 complete models → empty with counted caveat.

## 7.3 Section composition
Overview = C0+C8 · Coverage = C2 · Capability = C1+C9 · Coder = C4+C3+C5+C6 (language control
here) · Context = C7. ChartCards in 2-col grid (1-col <900px), full-width C2/C9.

---

# §8 New data contracts (Terminus-side, CONST-21)

All endpoints: protected (session; viewer-readable), masked (`mask_response`), read-only GETs,
JSON, served by `src/constellation/models_api.rs` in `protected_router`. Reuses
`src/intake/{storage,catalog,discovery}` read paths — no new external DB client, no MCP
self-calls. List endpoints: `limit` (default 50, max 500) + `offset` + `total`.

| Endpoint | Source | Response sketch |
|---|---|---|
| `GET /api/terminus/models?scope=&q=&category=&status=&serving=&limit=&offset=` | catalog ⋈ brochure ⋈ serving ⋈ advisor | `{total, refreshed_at, models:[{model_name, family?, params_b?, quant, category?, brochure_status?, in_current_fleet, discovery_score?, vram_gb?, size_b?, serving_now, coverage:{coder,assistant,serving,agent}, best_pass_rate?, last_run_at?}]}` |
| `GET /api/terminus/models/{name}` | all sources for one model | `{identity, brochure (incl. timeline), serving:[...], operational, catalog:{card, cells}}` — absent sources `null`; 404 only when unknown everywhere |
| `GET /api/terminus/mint/summary?epoch=` | counts + epoch + best-model | C0 payload |
| `GET /api/terminus/mint/dimensions?models=&epoch=` | assistant_dimension_score | `{dimensions:[8], models:[{model_id, scores:[{dimension, norm, raw, metric, std_dev, n, low_confidence}]}], fleet_median:[...]}` |
| `GET /api/terminus/mint/matrix?epoch=` | catalog cells | `{models, columns:[{test_type,task_category}], cells:[{model, col, status, pass_rate, n_samples, score_stddev, low_confidence, last_run_at, harness_version}]}` |
| `GET /api/terminus/mint/runs?suite=&model=&task_category=&language=&failure_class=&epoch=&limit=&offset=` | run tables | paged raw rows |
| `GET /api/terminus/mint/box?metric=total_time_ms\|code_quality_score&…` | server-side quartiles | `{groups:[{model, min,q1,median,q3,max,n, outliers:[{run_id,value,case_id,failure_class}]}]}` |
| `GET /api/terminus/mint/language-stats?language=&epoch=` | matview ⋈ profiles | rows + per-model rollup incl. vram_gb |
| `GET /api/terminus/mint/failures?epoch=&task_category=` | GROUP BY failure_class | `{classes:[top5+other], models:[{model, counts, total_runs}]}` |
| `GET /api/terminus/mint/context-profiles?models=` | context runs ⋈ operational | per-model tier arrays + max_context_safe |
| `GET /api/terminus/mint/activity?range=` | created_at histograms + epoch markers | `{days:[{date, code, context, agent}], epochs:[...]}` |
| `GET /api/terminus/activity?limit=` (CONST-26) | audit JSONL tail | `{entries:[{ts, method, path, principal, system}]}` — no bodies |

**Contracts-to-confirm (resolved at CONST-21 build):** (1) `model_profiles.profile_date` live
but not in repo CREATE — order by `COALESCE(profile_date, created_at)`; (2) catalog `quant`
live-nullable — treat as `Option`; (3) code/agent cells all `not_run` until
`INTAKE_CORPUS_V2_DIR` provisioned — UI copy truthful; (4) brochure category enum uses full
8-value set; (5) epoch semantics per `EpochSelector` — absent ⇒ Current, `all` ⇒ All, else Only.

---

# §9 Technical architecture

- **Build tooling:** Vite 5 / TS 5.4 / React 18.3 / react-router 6.23; `npm run build` =
  `tsc --noEmit && vite build`. `manualChunks` `viz` chunk (nivo); MINT/Models routes lazy.
  Budget: initial ≤ 350 KB gz, viz ≤ 250 KB gz.
- **Serving:** CONST-15 embedded dist (`include_dir!`, `src/constellation/assets.rs`), SPA
  fallback, `CONSTELLATION_WEB_DIST_DIR` dev override.
- **Harmony embedding:** in-tree port (done). harmony-web = maintenance mode.
- **State/data:** React local state + per-domain hooks through the singleton
  `aggregationClient` — no Redux/query lib. Grep-enforced: no fetch/WebSocket/window.location/
  browser storage outside `aggregationClient.ts`; no raw hex where a token exists; no direct
  nivo/recharts imports outside `src/viz/`.
- **Backend layout:** new Rust inside `src/constellation/` (`models_api.rs`, `ws.rs`,
  `activity.rs`) + the muse arm in `proxy.rs`; routes in `mod.rs`; every handler ends in
  `mask_response`; every mutating path audited.
- **Testing:** UI = tsc + vite build + grep gates; Rust = compiler-tool `mode=test` with env
  allowlist caveat; every new endpoint gets axum oneshot tests (shape + auth-guard +
  degradation + masking).

---

# §10 Build decomposition — Plane items CONST-16..29 (project TERM)

Pipeline: full moosenet-spec v4.2+ per item. Phases: **A** = CONST-16/17/18 (platform, MERGED)
· **B** = 19/20 (Muse) · **C** = 21/22/23/24 (Models+MINT) · **D** = 25/26/27/28/29.

### CONST-16: Module registry v2 + brand shell — MERGED (PR #211)
### CONST-17: Brand token sheet + component kit + viz kit — MERGED (PR #214)
### CONST-18: /ws relay — MERGED (PR #212)

### CONST-19: Muse proxy namespace (backend + client)
- **Priority:** High · **Agent:** claude · **Estimate:** 3h
- Fourth namespace `/api/muse/*path` (§5.4): proxy arm + `CONSTELLATION_MUSE_URL` helper +
  `muse` health probe + client `SystemId` union + mock-adapter canned muse data + module
  registration (`healthSystem:'muse'`).
- FILES: src/constellation/proxy.rs, src/constellation/mod.rs, src/config.rs, .env.example,
  constellation-web/src/lib/aggregationClient.ts, constellation-web/src/panels/registerPanels.ts
- EDGE CASES: muse-down vs unconfigured distinct detail strings; art binary passthrough with
  upstream content-type, skips JSON masking, no panic.
- **Acceptance criteria:**
  - [ ] /api/muse/* behaves identically to the other namespaces (tests mirror existing suite)
  - [ ] Binary/art passthrough works (non-JSON body not corrupted)
  - [ ] Health lists 5 systems; muse module appears when reachable
  - [ ] README + .env.example updated; no hardcoded infra values; existing tests pass

### CONST-20: Muse module UI (dashboard, taste graph, channels)
- **Priority:** Medium · **Agent:** claude · **Estimate:** 6h
- The three §5.4 panels bound to the verified Muse routes, per-endpoint degradation
  (MUSEX-WIRE reality), RoleGate + ConfirmDialog on compose/maintenance actions, taste-cluster
  scatter under the 4-series cap.
- FILES: constellation-web/src/panels/muse/{DashboardPanel,TastePanel,ChannelsPanel}.tsx,
  src/hooks/useMuse.ts
- APPROACH: build against CONST-19 mocks; every section wraps its fetch in a
  degrade-to-ChartEmpty boundary keyed on 404/501/available:false; charts via viz kit only;
  cluster fold >4 → "Other".
- EDGE CASES: empty library onboarding states; zero channels; past-dated premieres (sorted,
  dimmed).
- **Acceptance criteria:**
  - [ ] Three panels render on mocks; per-endpoint degradation proven
  - [ ] Mutations operator-gated + confirmed + audited (via proxy)
  - [ ] Charts pass §4 floor (tooltips, table twins, legends, empty/loading)
  - [ ] README updated; no hardcoded infra; existing tests pass

### CONST-21: Models/MINT read API (models_api.rs)
- **Priority:** High · **Agent:** claude · **Estimate:** 8h
- §8's endpoint table over the intake read layer; epoch scoping via EpochSelector; server-side
  quartiles/normalization/folding; the five contracts-to-confirm resolved live + documented.
- FILES: src/constellation/models_api.rs, src/constellation/mod.rs, src/intake/storage.rs,
  src/intake/catalog.rs (additive read fns)
- EDGE CASES: empty epoch → zeroed summary; quant NULL joins; profile_date COALESCE; limit>500
  clamped; offset past end → empty page with correct total.
- **Acceptance criteria:**
  - [ ] All §8 endpoints implemented + tested (shape/auth/degradation/masking each)
  - [ ] Contracts-to-confirm #1–#5 verified live and noted in PR description
  - [ ] No second DB access path (reuses intake read layer; grep for new pool creation = 0)
  - [ ] README updated (constellation API section); no hardcoded infra; existing tests pass

### CONST-22: Model Library UI
- **Priority:** High · **Agent:** claude · **Estimate:** 8h
- §6 in full — browse (table/card, filter row, stat tiles), detail (4 degrading sections),
  compare (URL-state, ≤4, radar overlay + emphasized Pareto), palette entity source
  ("model:" prefix) for CONST-25.
- FILES: constellation-web/src/panels/models/{BrowsePanel,DetailPanel,ComparePanel}.tsx,
  src/hooks/useModels.ts, mock fixtures in aggregationClient.ts
- EDGE CASES: brochure-only model detail renders identity+provenance only; catalog-but-evicted;
  4-model cap enforced with toast on 5th.
- **Acceptance criteria:**
  - [ ] Browse/detail/compare functional on mocks + live shapes
  - [ ] Every §6 field bound (spot-check list in PR)
  - [ ] low_confidence/n≤1 affordances present; nothing silently hidden
  - [ ] README updated; no hardcoded infra; existing tests pass

### CONST-23: MINT module — phase 1 (tiles, heatmap, radar, Pareto, context, activity)
- **Priority:** High · **Agent:** claude · **Estimate:** 8h
- §7 layout + global filter row + C0, C2, C1, C4, C7, C8 exactly as specced. Deep-link
  query-string filters.
- FILES: constellation-web/src/panels/mint/{MintPage,OverviewSection,CoverageSection,
  CapabilitySection,ContextSection}.tsx, src/hooks/useMint.ts, mocks
- EDGE CASES: corpus-dir-unset truth copy on C2; <2 models for C1 default; epoch with zero
  assistant runs hides Capability with explanatory empty.
- **Acceptance criteria:**
  - [ ] C0/C1/C2/C4/C7/C8 match §7.2 (encoding + interactions + states)
  - [ ] Global filters scope every section; deep links restore state
  - [ ] All charts: legend rule, tooltip floor, table twins, no dual axes, palette tokens only
  - [ ] README updated; no hardcoded infra; existing tests pass

### CONST-24: MINT module — phase 2 (box, beeswarm, failure bars, parallel-coords)
- **Priority:** Medium · **Agent:** claude · **Estimate:** 6h
- C3, C5, C6, C9 + cross-chart drill-downs (heatmap→box/swarm, segment→swarm, swarm→run row)
  and the Coder section assembly with its language control.
- FILES: …/panels/mint/CoderSection.tsx, TradeoffsSection.tsx (C9), viz additions
- EDGE CASES: all-none failure_class (C6 empty: "no failures this epoch"); model with one
  giant outlier (log toggle default proves readable).
- **Acceptance criteria:**
  - [ ] C3/C5/C6/C9 match §7.2; drill-down paths work
  - [ ] Discrete-score honesty: no smoothed violins; swarm + box roles as specced
  - [ ] README updated; no hardcoded infra; existing tests pass

### CONST-25: Command palette + global search
- **Priority:** Medium · **Agent:** claude · **Estimate:** 4h
- §3.2 — in-house palette (no deps), nav + registered commands + async entity search through
  existing list endpoints; role-aware; full keyboard contract; focus trap + ARIA dialog.
- FILES: constellation-web/src/components/CommandPalette.tsx, src/lib/commandRegistry.ts,
  App wiring, panel registerCommand calls
- EDGE CASES: all backends degraded (nav still instant); duplicate command ids rejected.
- **Acceptance criteria:**
  - [ ] Ctrl/Cmd+K everywhere; keyboard-only operable; screen-reader labeled
  - [ ] Entity search degrades per-source; no direct fetches
  - [ ] Operator-only commands hidden/disabled for viewer
  - [ ] README updated; no hardcoded infra; existing tests pass

### CONST-26: Activity endpoint + notifications/feed
- **Priority:** Medium · **Agent:** claude · **Estimate:** 4h
- §3.3 — GET /api/terminus/activity (audit-tail read, masked, no bodies,
  CONSTELLATION_ACTIVITY_TAIL_LIMIT cap) + Overview feed merge (activity + health transitions
  + ws events when present) + toasts + bell menu (in-memory only).
- FILES: src/constellation/activity.rs (+mod.rs route), UI src/components/{Toast,
  ActivityFeed}.tsx, Overview integration
- EDGE CASES: corrupt JSONL line skipped with counter; log rotated mid-read; zero-length file.
- **Acceptance criteria:**
  - [ ] Endpoint returns masked entries, never bodies; missing log = empty 200
  - [ ] Feed + toasts + bell per §3.3; nothing persisted to browser storage
  - [ ] README updated; no hardcoded infra; existing tests pass

### CONST-27: Viewer role (auth extension + role gating)
- **Priority:** Medium · **Agent:** claude · **Estimate:** 4h
- §3.4 — role claim in the session JWT; CONSTELLATION_VIEWER_SECRET (<secret-manager>-provisioned,
  unset ⇒ tier disabled); server-side 403 on mutating methods for viewer; /api/auth/me role
  field; UI RoleGate.
- EDGE CASES: same value both secrets (operator wins, warn-log); role tampering (JWT signature
  covers it — re-signed-wrong-key token test).
- **Acceptance criteria:**
  - [ ] Enforcement is server-side (UI gate cosmetic; proven by direct POST as viewer)
  - [ ] Fail-closed on unset secrets both tiers; secrets read per crate convention, never
        hardcoded, never logged
  - [ ] README updated (roles section); no hardcoded infra; existing tests pass

### CONST-28: Terminus-self module (fleet, tools, activity panels)
- **Priority:** Medium · **Agent:** claude · **Estimate:** 5h
- §5.5 — terminus.fleet (health history ring buffer + sparklines), terminus.tools (catalog
  from extended /api/terminus/config — additive field), terminus.activity (paged CONST-26
  view with filters).
- EDGE CASES: huge tool catalog (paged table); empty broker routes (workerCount 0 copy).
- **Acceptance criteria:**
  - [ ] Config endpoint change is additive (existing client contract test still green)
  - [ ] Three panels functional; tools searchable from the palette
  - [ ] README updated; no hardcoded infra; existing tests pass

### CONST-29: Accessibility, polish & docs QA pass
- **Priority:** Medium · **Agent:** claude · **Estimate:** 4h
- Whole-app pass against §2.6/§4.4: keyboard reachability audit (incl. canvas keyboard
  reorder), focus-visible coverage, ARIA landmarks/labels, reduced-motion verification, chart
  table-twin completeness, contrast spot-checks, degraded/empty-state copy consistency,
  brand-conformance sweep (no old-palette hexes anywhere), and removal of the CONST-17 legacy
  token aliases (--h-* + old-name tokens) once no call-sites remain (grep-proven); fix
  findings ≤30min inline, file larger ones as Plane follow-ups; final README/docs sweep.
- **Acceptance criteria:**
  - [ ] Checklist for every panel committed in the PR; zero unfixed ≤30min findings
  - [ ] Legacy token aliases removed; grep for --h- and old-value hexes returns 0
  - [ ] Brand-conformance screenshots per section attached
  - [ ] Larger findings filed as Plane items (listed in PR)
  - [ ] README final state matches shipped behavior
  - [ ] No hardcoded infra; existing tests pass

---

# §11 Sequencing, dependencies, reconciliation

## 11.1 Dependency graph
CONST-16 → {17, 25, 26, 28} · CONST-17 → {20, 22, 23, 24} · CONST-19 → 20 · CONST-21 → {22,
23, 24} · CONST-23 → 24 · CONST-18 independent after 16 · CONST-27 independent after 16 ·
CONST-29 last.

## 11.2 Definition of done
Per item: the v4.2 pipeline through mirror + Plane Done. Program: all 14 items Done → Epic
Review capstone (`review_run` structure `epic`, royal panel; KG refresh fires; docgen only on
APPROVE) → findings triaged into Plane.

## 11.3 Deploy note (ops, not code)
Live rollout = constellation-updater path (`constellation-update.sh --force --skip-idle
terminus-primary` on the primary's host) per skill Stage 8c; build-on-dest remains the
deployed norm until Phase 2 (fetch-mode) lands. Operator-action prerequisites (surface, never
hardcode): `CONSTELLATION_MUSE_URL`, `CONSTELLATION_HARMONY_WS_URL`,
`CONSTELLATION_VIEWER_SECRET` (<secret-manager>), and the standing S97 pair
`CONSTELLATION_HARMONY_URL`/`CONSTELLATION_LUMINA_URL`.

## 11.4 Reconciliation with S97 CONST-01..15
CONST-01..04 + 15: foundation, merged/deployed. CONST-05..14 remain OPEN under S97 and are not
duplicated; CONST-12's read scope subsumed by Overview/fleet panels; CONST-13 partially lands
via terminus.activity; CONST-14 unchanged. CONST-07 (Lumina) superseded by LUMINA-GUI-SPEC.

## 11.5 Risks
1. Live intake-schema drift → CONST-21 verifies live before binding; UI binds to the API.
2. Chart scope creep → the §7.2 chart list is closed for this spec.
3. Reviewer quota volatility → default panels per current memory, never a hand-rolled CLI.
