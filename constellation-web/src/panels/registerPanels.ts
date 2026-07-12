// CONST-04: Central import point that registers every panel module with the module registry.
// Imported once, for side effects only, from src/main.tsx before the app renders. Each future
// panel adds one line here — the shell never needs to change.
//
// This now includes the full ported harmony-web surface (pages ported under src/pages/,
// registered here as panels, grouped by system per CONST-04 §3):
//   Harmony:  Dashboard, Projects, Tasks, Agents, PRs, Prompts, Sessions, AuditLog
//   Chord:    Inference, Providers, Playground
//   Status:   Analytics, Engine Diagram (dashboard/EnginePanel)
//   Terminus: existing example TerminusPanel
//   Lumina:   stub (config surface TBD in CONST-07)
import { registerPanel } from '../lib/moduleRegistry';
import { TerminusPanel } from './terminus/TerminusPanel';
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

// ── Harmony ──────────────────────────────────────────────────────────────────

registerPanel({
  id: 'harmony.dashboard',
  system: 'Harmony',
  title: 'Dashboard',
  path: '/harmony/dashboard',
  icon: '⌂',
  available: true,
  component: DashboardPanel,
});

registerPanel({
  id: 'harmony.projects',
  system: 'Harmony',
  title: 'Projects',
  path: '/harmony/projects',
  icon: '📁',
  available: true,
  component: ProjectsPanel,
});

registerPanel({
  id: 'harmony.tasks',
  system: 'Harmony',
  title: 'Tasks',
  path: '/harmony/tasks',
  icon: '✓',
  available: true,
  component: Tasks,
});

registerPanel({
  id: 'harmony.agents',
  system: 'Harmony',
  title: 'Agents',
  path: '/harmony/agents',
  icon: '🤖',
  available: true,
  component: Agents,
});

registerPanel({
  id: 'harmony.prs',
  system: 'Harmony',
  title: 'PRs',
  path: '/harmony/prs',
  icon: '⎇',
  available: true,
  component: PRs,
});

registerPanel({
  id: 'harmony.prompts',
  system: 'Harmony',
  title: 'Prompts',
  path: '/harmony/prompts',
  icon: '📝',
  available: true,
  component: Prompts,
});

registerPanel({
  id: 'harmony.sessions',
  system: 'Harmony',
  title: 'Sessions',
  path: '/harmony/sessions',
  icon: '⏱',
  available: true,
  component: Sessions,
});

registerPanel({
  id: 'harmony.audit',
  system: 'Harmony',
  title: 'Audit Log',
  path: '/harmony/audit',
  icon: '📋',
  available: true,
  component: AuditLog,
});

// ── Chord ────────────────────────────────────────────────────────────────────

registerPanel({
  id: 'chord.inference',
  system: 'Chord',
  title: 'Inference',
  path: '/chord/inference',
  icon: '⚡',
  available: true,
  component: Inference,
});

registerPanel({
  id: 'chord.providers',
  system: 'Chord',
  title: 'Providers',
  path: '/chord/providers',
  icon: '🔌',
  available: true,
  component: Providers,
});

registerPanel({
  id: 'chord.playground',
  system: 'Chord',
  title: 'Playground',
  path: '/chord/playground',
  icon: '▶',
  available: true,
  component: Playground,
});

// ── Status ───────────────────────────────────────────────────────────────────

registerPanel({
  id: 'status.analytics',
  system: 'Status',
  title: 'Analytics',
  path: '/status/analytics',
  icon: '📊',
  available: true,
  component: Analytics,
});

registerPanel({
  id: 'status.engine-diagram',
  system: 'Status',
  title: 'Engine Diagram',
  path: '/status/engine-diagram',
  icon: '⚙',
  available: true,
  component: EngineDiagramPanel,
});

// ── Terminus ─────────────────────────────────────────────────────────────────

registerPanel({
  id: 'terminus.config',
  system: 'Terminus',
  title: 'Config',
  path: '/terminus/config',
  icon: '⚙',
  available: true,
  component: TerminusPanel,
});

// ── Lumina ───────────────────────────────────────────────────────────────────
// Stub only — Lumina's own config surface is CONST-07's job, not this port.

registerPanel({
  id: 'lumina.config',
  system: 'Lumina',
  title: 'Lumina',
  path: '/lumina/config',
  icon: '✦',
  available: false,
  component: LuminaStubPanel,
});
