// CONST-04: Central import point that registers every panel module with the module registry.
// Imported once, for side effects only, from src/main.tsx before the app renders. Each future
// panel adds one line here — the shell never needs to change.
//
// CONST-16: also registers the ModuleDescriptor for every module that has a real presence
// today (harmony/chord/lumina/terminus). `muse`/`models`/`mint` are valid `ModuleId`s (see
// moduleRegistry.ts) but are NOT registered here yet — their modules/panels land with
// CONST-19..24; until then they simply don't exist in the registry, so they never show up
// as a global-bar tab (no module descriptor to match `getAvailableModules` against).
//
// Panel `system` values are now lowercase ModuleIds (not the old capitalized SystemGroup) —
// the legacy Status/Providers groups have dissolved: Analytics/Engine Diagram re-home under
// `harmony` (spec §5.1 — "'Status' as a top-level group dissolves into Overview"); Chord's
// Providers panel stays under `chord` per §5.2 (only the *legacy label* 'Providers' remaps to
// `terminus`, via legacySystemGroupToModuleId — no current panel used that label).
//   Harmony:  Dashboard, Projects, Tasks, Agents, PRs, Prompts, Sessions, AuditLog,
//             Analytics (was status.analytics), Engine Diagram (was status.engine-diagram)
//   Chord:    Inference, Providers, Playground
//   Terminus: Config (existing example TerminusPanel), plus CONST-28's module-self build:
//             Fleet, Tools, Activity
//   Lumina:   stub (config surface TBD in CONST-07)
import { registerPanel, registerModule } from '../lib/moduleRegistry';
import { TerminusPanel } from './terminus/TerminusPanel';
import { FleetPanel } from './terminus/FleetPanel';
import { ToolsPanel } from './terminus/ToolsPanel';
import { ActivityPanel } from './terminus/ActivityPanel';
import { LuminaStubPanel } from './lumina/LuminaStubPanel';
import { EngineDiagramPanel } from './status/EngineDiagramPanel';
import { DashboardPanel } from './harmony/DashboardPanel';
import { ProjectsPanel } from './harmony/ProjectsPanel';
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

// ── Modules (CONST-16, §1.4 order: Overview · Harmony · Chord · Lumina · Muse · Models ·
// MINT · Terminus — Overview has no descriptor, it's the always-available default route;
// order 4/5/6 are reserved for muse/models/mint when their own items register them) ────────

registerModule({ id: 'harmony', title: 'Harmony', icon: '⌂', healthSystem: 'harmony', order: 1 });
registerModule({ id: 'chord', title: 'Chord', icon: '⚡', healthSystem: 'chord', order: 2 });
registerModule({ id: 'lumina', title: 'Lumina', icon: '✦', healthSystem: 'lumina', order: 3 });
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
// Stub only — Lumina's own config surface is CONST-07's job, not this port.

registerPanel({
  id: 'lumina.config',
  system: 'lumina',
  title: 'Lumina',
  path: '/lumina/config',
  icon: '✦',
  available: false,
  component: LuminaStubPanel,
});
