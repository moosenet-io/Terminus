// CONST-04: Central import point that registers every panel module with the module registry.
// Imported once, for side effects only, from src/main.tsx before the app renders. Each future
// panel adds one line here — the shell never needs to change.
//
// CONST-16: also registers the ModuleDescriptor for every module that has a real presence
// today (harmony/chord/lumina/muse/terminus). `models`/`mint` are valid `ModuleId`s (see
// moduleRegistry.ts) but are NOT registered here yet — their modules/panels land with
// CONST-21..24; until then they simply don't exist in the registry, so they never show up
// as a global-bar tab (no module descriptor to match `getAvailableModules` against).
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
import { FleetPanel } from './terminus/FleetPanel';
import { ToolsPanel } from './terminus/ToolsPanel';
import { ActivityPanel } from './terminus/ActivityPanel';
import { EngineDiagramPanel } from './status/EngineDiagramPanel';
import { DashboardPanel } from './harmony/DashboardPanel';
import { ProjectsPanel } from './harmony/ProjectsPanel';
import { DashboardPanel as MuseDashboardPanel } from './muse/DashboardPanel';
import { TastePanel as MuseTastePanel } from './muse/TastePanel';
import { ChannelsPanel as MuseChannelsPanel } from './muse/ChannelsPanel';
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
registerModule({ id: 'chord', title: 'Chord', icon: '⚡', healthSystem: 'chord', order: 2 });
// CONST-19 registered the muse module; CONST-20 adds its three panels below.
registerModule({ id: 'muse', title: 'Muse', icon: '🎬', healthSystem: 'muse', order: 3 });
// LGUI-05: lumina module registration only -- no panels yet (LGUI-06 adds lumina.overview
// first). Ordered AFTER Muse per LUMINA-GUI-SPEC §2.
registerModule({ id: 'lumina', title: 'Lumina', icon: '✦', healthSystem: 'lumina', order: 4 });
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
  icon: '📁',
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
  icon: '🤖',
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
  icon: '📝',
  available: true,
  component: Prompts,
});

registerPanel({
  id: 'harmony.sessions',
  system: 'harmony',
  title: 'Sessions',
  path: '/harmony/sessions',
  icon: '⏱',
  available: true,
  component: Sessions,
});

registerPanel({
  id: 'harmony.audit',
  system: 'harmony',
  title: 'Audit Log',
  path: '/harmony/audit',
  icon: '📋',
  available: true,
  component: AuditLog,
});

// Re-homed from the legacy 'Status' group (spec §5.1/§10 CONST-16).
registerPanel({
  id: 'harmony.analytics',
  system: 'harmony',
  title: 'Analytics',
  path: '/harmony/analytics',
  icon: '📊',
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
  icon: '⚡',
  available: true,
  component: Inference,
});

registerPanel({
  id: 'chord.providers',
  system: 'chord',
  title: 'Providers',
  path: '/chord/providers',
  icon: '🔌',
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

// ── Muse (CONST-20) ──────────────────────────────────────────────────────────

registerPanel({
  id: 'muse.dashboard',
  system: 'muse',
  title: 'Dashboard',
  path: '/muse/dashboard',
  icon: '🎬',
  available: true,
  component: MuseDashboardPanel,
});

registerPanel({
  id: 'muse.taste',
  system: 'muse',
  title: 'Taste',
  path: '/muse/taste',
  icon: '📈',
  available: true,
  component: MuseTastePanel,
});

registerPanel({
  id: 'muse.channels',
  system: 'muse',
  title: 'Channels',
  path: '/muse/channels',
  icon: '📺',
  available: true,
  component: MuseChannelsPanel,
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
  icon: '🛰',
  available: true,
  component: FleetPanel,
});

registerPanel({
  id: 'terminus.tools',
  system: 'terminus',
  title: 'Tools',
  path: '/terminus/tools',
  icon: '🧰',
  available: true,
  component: ToolsPanel,
});

registerPanel({
  id: 'terminus.activity',
  system: 'terminus',
  title: 'Activity',
  path: '/terminus/activity',
  icon: '📡',
  available: true,
  component: ActivityPanel,
});

// ── Lumina ───────────────────────────────────────────────────────────────────
// No panels registered yet -- see the module-registration comment above (LGUI-05); the
// CONST-04 stub (`lumina.config`, `available: false`) is removed. LGUI-06 registers the
// first real panel (`lumina.overview`).
