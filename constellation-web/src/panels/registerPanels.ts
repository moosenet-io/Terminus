// CONST-04: Central import point that registers every panel module with the module registry.
// Imported once, for side effects only, from src/main.tsx before the app renders. Each future
// panel adds one line here — the shell never needs to change.
//
// CONST-16: also registers the ModuleDescriptor for every module that has a real presence
// today (harmony/chord/lumina/muse/terminus). `models` is a valid `ModuleId` (see
// moduleRegistry.ts) but is NOT registered here yet — it lands with CONST-22; until then it
// simply doesn't exist in the registry, so it never shows up as a global-bar tab (no module
// descriptor to match `getAvailableModules` against).
//
// CONST-23 registers `mint` (module + its one sectioned `/mint` panel, `mint.overview` —
// see §7.1: "one page, sectioned... sticky in-page section nav", not one panel per section).
//
// CONST-19 registers the `muse` module descriptor only — no panels yet (CONST-20's job); a
// module with zero registered panels is a valid, if empty, tab (`getPanelsByModule('muse')`
// returns `[]` until CONST-20 lands `muse.dashboard`/`muse.taste`/`muse.channels`).
//
// Panel `system` values are now lowercase ModuleIds (not the old capitalized SystemGroup) —
// the legacy Status/Providers groups have dissolved: Analytics/Engine Diagram re-home under
// `harmony` (spec §5.1 — "'Status' as a top-level group dissolves into Overview"); Chord's
// Providers panel stays under `chord` per §5.2 (only the *legacy label* 'Providers' remaps to
// `terminus`, via legacySystemGroupToModuleId — no current panel used that label).
//   Harmony:  Dashboard, Projects, Tasks, Agents, PRs, Prompts, Sessions, AuditLog,
//             Analytics (was status.analytics), Engine Diagram (was status.engine-diagram)
//   Chord:    Inference, Providers, Playground
//   Muse:     Dashboard, Taste, Channels (CONST-20)
//   Terminus: Config (existing example TerminusPanel), plus CONST-28's module-self build:
//             Fleet, Tools, Activity
//   Lumina:   module registered, no panels yet (LGUI-05) -- LUMINA-GUI-SPEC.md supersedes the
//             old CONST-07 config-surface placeholder; the CONST-04 stub (`available: false`,
//             a "not yet available" placeholder card) is removed here. Real panels land
//             LGUI-06..12 (overview/chat/memory/persona/routing/tools/access/setup); until the
//             first one registers, `lumina` is a module with zero panels -- same pattern
//             CONST-19 established for `muse` (a module tab can exist before it has any
//             panels, per `getPanelsByModule`'s doc in moduleRegistry.ts).
import { registerPanel, registerModule } from '../lib/moduleRegistry';
import { registerCommand } from '../lib/commandRegistry';
import { getCurrentPath, requestHealthRefresh } from '../lib/shellBridge';
import { TerminusPanel } from './terminus/TerminusPanel';
import { ChatPanel } from './lumina/ChatPanel';
import { FleetPanel } from './terminus/FleetPanel';
import { ToolsPanel } from './terminus/ToolsPanel';
import { ActivityPanel } from './terminus/ActivityPanel';
import { PersonaPanel } from './lumina/PersonaPanel';
import { MemoryPanel } from './lumina/MemoryPanel';
import { EngineDiagramPanel } from './status/EngineDiagramPanel';
import { DashboardPanel } from './harmony/DashboardPanel';
import { ProjectsPanel } from './harmony/ProjectsPanel';
import { HarmonyForestPanel } from './harmony/HarmonyForestPanel';
import { BackendsPanel } from './chord/BackendsPanel';
import { OverviewPanel as LuminaOverviewPanel } from './lumina/OverviewPanel';
import { RosterPanel as ModelsRosterPanel } from './models/RosterPanel';
import { DashboardPanel as MuseDashboardPanel } from './muse/DashboardPanel';
import { TastePanel as MuseTastePanel } from './muse/TastePanel';
import { ChannelsPanel as MuseChannelsPanel } from './muse/ChannelsPanel';
import { OverviewPanel as MintOverviewPanel } from './mint/OverviewPanel';
import { CategoryReportPanel as MintCategoryReportPanel } from './mint/CategoryReportPanel';
import { Tasks } from '../pages/Tasks';
import { Agents } from '../pages/Agents';
import { PRs } from '../pages/PRs';
import { Prompts } from '../pages/Prompts';
import { Sessions } from '../pages/Sessions';
import { AuditLog } from '../pages/AuditLog';
import { Inference } from '../pages/Inference';
import { Providers } from '../pages/Providers';
import { Playground } from '../pages/Playground';
import { Analytics } from '../pages/Analytics';

// ── Modules (order per LUMINA-GUI-SPEC §2: Overview · Harmony · Chord · Muse · Lumina ·
// Models · MINT · Terminus — Overview has no descriptor, it's the always-available default
// route. LGUI-05 review decision: the spec's "lumina orders after Muse" IS the directive
// (CONST-GUI-SPEC §1.4's earlier listing predates the Lumina spec superseding §5.3), so
// muse takes CONST-16's old lumina slot and lumina follows it.) ──────────────────────────

registerModule({ id: 'harmony', title: 'Harmony', icon: '⌂', healthSystem: 'harmony', order: 1 });
registerModule({ id: 'chord', title: 'Chord', icon: '◈', healthSystem: 'chord', order: 2 });
// CONST-19 registered the muse module; CONST-20 adds its three panels below.
registerModule({ id: 'muse', title: 'Muse', icon: '◎', healthSystem: 'muse', order: 3 });
// LGUI-05: lumina module registration only -- no panels yet (LGUI-06 adds lumina.overview
// first). Ordered AFTER Muse per LUMINA-GUI-SPEC §2.
registerModule({ id: 'lumina', title: 'Lumina', icon: '✦', healthSystem: 'lumina', order: 4 });
// CGUI-09 (TERM #532): the Models module. Per LUMINA-GUI-SPEC §2 order (…Muse · Lumina ·
// Models · MINT · Terminus), Models takes order 5. It is a terminus-backed module — its data
// source is the terminus models API (CONST-21), so it binds to the always-available terminus
// health entry (moduleRegistry: models/mint/terminus bind healthSystem 'terminus').
registerModule({ id: 'models', title: 'Models', icon: '◆', healthSystem: 'terminus', order: 5 });
// CGUI-10 (TERM #533): the MINT benchmark module — reserved-but-unbuilt until now. Ordered
// after Models and before Terminus per LUMINA-GUI-SPEC §2 (… Muse · Lumina · Models · MINT ·
// Terminus). Terminus-backed (its data source is the terminus namespace), so it binds to the
// always-available terminus health entry.
registerModule({ id: 'mint', title: 'MINT', icon: '◈', healthSystem: 'terminus', order: 6 });
registerModule({ id: 'terminus', title: 'Terminus', icon: '⚙', healthSystem: 'terminus', order: 7 });

// ── Harmony ──────────────────────────────────────────────────────────────────

registerPanel({
  id: 'harmony.dashboard',
  system: 'harmony',
  title: 'Dashboard',
  path: '/harmony/dashboard',
  icon: '⌂',
  available: true,
  component: DashboardPanel,
});

registerPanel({
  id: 'harmony.projects',
  system: 'harmony',
  title: 'Projects',
  path: '/harmony/projects',
  icon: '▸',
  available: true,
  component: ProjectsPanel,
});

registerPanel({
  id: 'harmony.tasks',
  system: 'harmony',
  title: 'Tasks',
  path: '/harmony/tasks',
  icon: '✓',
  available: true,
  component: Tasks,
});

registerPanel({
  id: 'harmony.agents',
  system: 'harmony',
  title: 'Agents',
  path: '/harmony/agents',
  icon: '◍',
  available: true,
  component: Agents,
});

registerPanel({
  id: 'harmony.prs',
  system: 'harmony',
  title: 'PRs',
  path: '/harmony/prs',
  icon: '⎇',
  available: true,
  component: PRs,
});

registerPanel({
  id: 'harmony.prompts',
  system: 'harmony',
  title: 'Prompts',
  path: '/harmony/prompts',
  icon: '▤',
  available: true,
  component: Prompts,
});

registerPanel({
  id: 'harmony.sessions',
  system: 'harmony',
  title: 'Sessions',
  path: '/harmony/sessions',
  icon: '◔',
  available: true,
  component: Sessions,
});

registerPanel({
  id: 'harmony.audit',
  system: 'harmony',
  title: 'Audit Log',
  path: '/harmony/audit',
  icon: '▣',
  available: true,
  component: AuditLog,
});

// CGUI-11 (TERM #534): the Harmony Forest Build orchestrator screen — a self-contained animated
// build visualization (spec "grows" as an SVG tree; leaves = issues; a persisted forest = shipped
// specs). Driven by a built-in simulation (forestEngine.ts), not live data in this item.
registerPanel({
  id: 'harmony.forest',
  system: 'harmony',
  title: 'Forest Build',
  path: '/harmony/forest',
  icon: '▲',
  available: true,
  component: HarmonyForestPanel,
});

// Re-homed from the legacy 'Status' group (spec §5.1/§10 CONST-16).
registerPanel({
  id: 'harmony.analytics',
  system: 'harmony',
  title: 'Analytics',
  path: '/harmony/analytics',
  icon: '▥',
  available: true,
  component: Analytics,
});

registerPanel({
  id: 'harmony.engine',
  system: 'harmony',
  title: 'Engine Diagram',
  path: '/harmony/engine',
  icon: '⚙',
  available: true,
  component: EngineDiagramPanel,
});

// ── Chord ────────────────────────────────────────────────────────────────────

registerPanel({
  id: 'chord.inference',
  system: 'chord',
  title: 'Inference',
  path: '/chord/inference',
  icon: '◈',
  available: true,
  component: Inference,
});

registerPanel({
  id: 'chord.providers',
  system: 'chord',
  title: 'Providers',
  path: '/chord/providers',
  icon: '◉',
  available: true,
  component: Providers,
});

registerPanel({
  id: 'chord.playground',
  system: 'chord',
  title: 'Playground',
  path: '/chord/playground',
  icon: '▶',
  available: true,
  component: Playground,
});

// CGUI-06 (TERM #529): Backends — Chord-managed backend roster + named-alias routing table.
// Deepens the (previously thin) Chord module beyond Inference/Providers/Playground.
registerPanel({
  id: 'chord.backends',
  system: 'chord',
  title: 'Backends',
  path: '/chord/backends',
  icon: '⬡',
  available: true,
  component: BackendsPanel,
});

// ── Muse (CONST-20) ──────────────────────────────────────────────────────────

registerPanel({
  id: 'muse.dashboard',
  system: 'muse',
  title: 'Dashboard',
  path: '/muse/dashboard',
  icon: '◎',
  available: true,
  component: MuseDashboardPanel,
});

registerPanel({
  id: 'muse.taste',
  system: 'muse',
  title: 'Taste',
  path: '/muse/taste',
  icon: '◹',
  available: true,
  component: MuseTastePanel,
});

registerPanel({
  id: 'muse.channels',
  system: 'muse',
  title: 'Channels',
  path: '/muse/channels',
  icon: '▭',
  available: true,
  component: MuseChannelsPanel,
});

// ── Models (CGUI-09) ─────────────────────────────────────────────────────────
// The Models module's primary panel — a master-detail roster (rich model cards + per-model
// dimension radar). Driven entirely by the CGUI-08 data client (client.models.*); the
// reserved `models` ModuleId now surfaces as a real global-bar tab with a panel.

registerPanel({
  id: 'models.roster',
  system: 'models',
  title: 'Roster',
  path: '/models/roster',
  icon: '◆',
  available: true,
  component: ModelsRosterPanel,
});

// ── MINT (CGUI-10, TERM #533) ────────────────────────────────────────────────
// The benchmark/profiling module. Two panels: a cross-category Overview and the per-category
// Report surface (radar / heatmap / distribution / ranking / failures+runs), both driven by the
// CGUI-08 `client.mint.*` data client. Health-bound to terminus (always available), so the tab
// shows whenever the shell can reach terminus.

registerPanel({
  id: 'mint.overview',
  system: 'mint',
  title: 'Overview',
  path: '/mint/overview',
  icon: '◈',
  available: true,
  component: MintOverviewPanel,
});

registerPanel({
  id: 'mint.categories',
  system: 'mint',
  title: 'Category Reports',
  path: '/mint/categories',
  icon: '▥',
  available: true,
  component: MintCategoryReportPanel,
});

// ── Terminus ─────────────────────────────────────────────────────────────────

registerPanel({
  id: 'terminus.config',
  system: 'terminus',
  title: 'Config',
  path: '/terminus/config',
  icon: '⚙',
  available: true,
  component: TerminusPanel,
});

// CONST-28: Terminus module self — fleet health board, tool catalog, activity feed.
registerPanel({
  id: 'terminus.fleet',
  system: 'terminus',
  title: 'Fleet',
  path: '/terminus/fleet',
  icon: '⬢',
  available: true,
  component: FleetPanel,
});

registerPanel({
  id: 'terminus.tools',
  system: 'terminus',
  title: 'Tools',
  path: '/terminus/tools',
  icon: '▧',
  available: true,
  component: ToolsPanel,
});

registerPanel({
  id: 'terminus.activity',
  system: 'terminus',
  title: 'Activity',
  path: '/terminus/activity',
  icon: '⌒',
  available: true,
  component: ActivityPanel,
});

// ── Lumina ───────────────────────────────────────────────────────────────────
// LGUI-06: the module's actual landing panel (§2 of LUMINA-GUI-SPEC.md — `lumina.overview`,
// route `/lumina`, min role viewer). Registered first so it's the module's first panel (the
// one `ModuleRail`/`ModuleCard`'s "Open" link points at, per moduleRegistry's registration-
// order convention — see getPanelsByModule). The CONST-04 stub (`lumina.config`,
// `available: false`) was removed by LGUI-05; LGUI-06 registers the first real panel here.
// SWAPPED IN over the simpler CGUI-06 placeholder (TERM #529, mock-fallback data, no
// first-run/degraded-state handling) per operator decision: LGUI-06 is spec-accurate (real
// useLumina §7 hook, registry-gated first-run redirect, honest whole-panel degrade) — same bar
// as the LGUI-07/08 panels already merged alongside it.
registerPanel({
  id: 'lumina.overview',
  system: 'lumina',
  title: 'Overview',
  path: '/lumina',
  icon: '✦',
  available: true,
  component: LuminaOverviewPanel,
});

// LGUI-07: Conversations panel (LUMINA-GUI-SPEC.md §3.2/§9, route `/lumina/chat`, min role
// operator per §2's panel table — enforced inside ChatPanel itself via useAuthRole, same
// convention as RoleGate; PanelDescriptor has no per-panel role field). Works end-to-end on
// the mock adapter today; the real `/api/lumina/v1/chat/completions` proxy route is LGUI-05's
// job (also added by LGUI-05 — keep one on merge, if that lands a lumina.chat registration too).
registerPanel({
  id: 'lumina.chat',
  system: 'lumina',
  title: 'Conversations',
  path: '/lumina/chat',
  icon: '💬',
  available: true,
  component: ChatPanel,
});

// LGUI-08 (§3.3): Memory (engram) browser — operator-gated in-component (RoleGate/useAuthRole
// convention, see ChatPanel.tsx); `registerPanel`'s `PanelDescriptor` has no `minRole` field as
// of this build, so the panel itself renders a viewer placeholder, same pattern as lumina.chat.
registerPanel({
  id: 'lumina.memory',
  system: 'lumina',
  title: 'Memory',
  path: '/lumina/memory',
  icon: '\u{1F9E0}',
  available: true,
  component: MemoryPanel,
});

// LGUI-09 (§2/§3.4): Persona & Behavior — traits, digest, active context, prompt layers.
registerPanel({
  id: 'lumina.persona',
  system: 'lumina',
  title: 'Persona',
  path: '/lumina/persona',
  icon: '🎭',
  available: true,
  component: PersonaPanel,
});

// ── Palette commands (CONST-25) ────────────────────────────────────────────────
// A couple of sensible starter actions, registered the same way panels are — one line each,
// no shell change needed. Every other panel adds its own `registerCommand` calls the same way.

registerCommand({
  id: 'shell.refresh-health',
  title: 'Refresh health',
  subtitle: 'Re-poll /api/health for every module now',
  icon: '⟳',
  run: () => requestHealthRefresh(),
});

registerCommand({
  id: 'shell.copy-current-path',
  title: 'Copy current path',
  subtitle: 'Copies the current route to the clipboard',
  icon: '⧉',
  run: () => {
    navigator.clipboard?.writeText(getCurrentPath()).catch(() => {
      // Clipboard permission denied/unavailable — the command just silently no-ops, same
      // convention as the rest of the shell's non-critical UI actions.
    });
  },
});
