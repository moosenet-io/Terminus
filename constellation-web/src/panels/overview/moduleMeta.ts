// CGUI-04 (TERM #527): shared per-module flow metadata + a curated configuration/flow
// reference, extracted from CGUI-03's ModuleCard so the reusable module DETAIL view
// (ModuleDetail.tsx) and the Overview card render the SAME semantic-direction facts (the
// kind node-dot colour, the UPPERCASE flow role, the one-line description, the cost tier).
//
// PLACEHOLDER NOTE: `kind`/`role`/`desc`/`free` here are a curated heuristic — the fleet
// aggregation client exposes no machine-readable flow role, per-module call/latency figures,
// or live configuration yet (that lands with the CGUI-08 data client). Everything below is a
// "sensible placeholder" so every region renders truthfully rather than empty; it is NOT live
// data. Swap these tables for CGUI-08 reads once that client exists.
import type { ModuleId } from '../../lib/moduleRegistry';
import type { NodeKind } from '../../components/NodeBadge';

export interface ModuleMeta {
  kind: NodeKind;
  /** UPPERCASE identity role shown in the kind's accent colour. S127: the five constellation
   *  members read `CORE`; Terminus's intake subsystems (Models/MINT) read `SUBSYSTEM`. The
   *  node-dot `kind` colour is kept for visual variety, but the role no longer implies a
   *  SOURCE/ENDPOINT/CLOUD flow taxonomy the operator never asked for. */
  role: string;
  desc: string;
  /** false → paid/opt-in (amber badge); true → free $0/day (green badge). */
  free: boolean;
}

/** All fleet modules run local-inference at $0.00/day, so every card is a free (green) tier.
 *  `role` is core identity (CORE / SUBSYSTEM); `kind` stays the node-dot colour source. */
export const MODULE_META: Record<ModuleId, ModuleMeta> = {
  harmony:  { kind: 'core',     role: 'CORE',      desc: 'autonomous build orchestrator', free: true },
  chord:    { kind: 'core',     role: 'CORE',      desc: 'llm proxy + inference router',  free: true },
  terminus: { kind: 'core',     role: 'CORE',      desc: 'mcp tool hub + fleet infra',    free: true },
  lumina:   { kind: 'endpoint', role: 'CORE',      desc: 'assistant surface',             free: true },
  muse:     { kind: 'endpoint', role: 'CORE',      desc: 'media library + acquisition',   free: true },
  models:   { kind: 'source',   role: 'SUBSYSTEM', desc: 'model library',                 free: true },
  mint:     { kind: 'source',   role: 'SUBSYSTEM', desc: 'model benchmarks',              free: true },
};

/** The canonical route for a module's detail view (§4). Kept here so the Overview card's
 *  drill-in and App.tsx's route definition never drift. First path segment is the module id,
 *  so the shell's `activeModuleId` derivation keeps the module rail mounted ("same shell,
 *  deeper zoom") — see App.tsx. */
export function moduleDetailPath(id: ModuleId): string {
  return `/${id}/detail`;
}

export const KIND_COLOR: Record<NodeKind, string> = {
  source:   'var(--node-source)',    // blue
  core:     'var(--node-core)',      // violet
  endpoint: 'var(--node-endpoint)',  // green
  cloud:    'var(--node-cloud)',     // amber
};

// ── Detail-view reference data (CGUI-04, guide-spec §4) ──────────────────────────────────────
// All of the following is placeholder/representative pending the CGUI-08 data client — the
// detail view renders a real region shape with a sensible sample rather than an empty panel.

export type BadgeToneName = 'green' | 'amber' | 'neutral' | 'blue' | 'violet' | 'rose';

/** One key/value row of the CONFIGURATION panel (§4). `badge` renders the value as a tonal
 *  Badge (green `encrypted`, amber `$N cap`); a plain `value` renders as a mono figure. */
export interface ConfigRow {
  key: string;
  value?: string;
  badge?: { tone: BadgeToneName; label: string };
}

/** The §4 CONFIGURATION quartet — rate limit / timeout / auth vault / circuit breaker. These
 *  are representative defaults (NOT live config): the aggregation client exposes no per-module
 *  runtime configuration yet (CGUI-08). Every module gets the same shape so all modules reach
 *  the guide's depth checklist. */
export function configForModule(id: ModuleId): ConfigRow[] {
  // Per-module rate/timeout are representative — cores run hotter (higher rate, shorter
  // timeout) than endpoints; a source sits in between. Circuit breaker cost cap is $0 for a
  // free local module (shown as a green "no cap") — the amber "$N cap" appears where a module
  // could reach a paid upstream. Since every fleet module is free today, we still surface the
  // amber circuit-breaker row (a representative $5.00 cap) so the guide's amber-badge region is
  // demonstrated; swap for the real cap when CGUI-08 exposes it.
  const meta = MODULE_META[id];
  const rate = meta.kind === 'core' ? '120 / min' : meta.kind === 'source' ? '90 / min' : '60 / min';
  const timeout = meta.kind === 'core' ? '8s' : meta.kind === 'source' ? '12s' : '30s';
  return [
    { key: 'Rate limit', value: rate },
    { key: 'Timeout', value: timeout },
    { key: 'Auth vault', badge: { tone: 'green', label: 'encrypted' } },
    { key: 'Circuit breaker', badge: { tone: 'amber', label: '$5.00 cap' } },
  ];
}

export interface FlowNode {
  name: string;
  role: string;
  kind: NodeKind;
}

/** The §4 "POSITION IN FLOW" mini node-graph: a source → this module (core) → two endpoints.
 *  The centre node is always the module being inspected, rendered with its own kind so the
 *  diagram reads as "where this client sits in the request path". Representative topology
 *  pending CGUI-08's real dependency graph. */
export interface FlowGraph {
  source: FlowNode;
  core: FlowNode;
  endpoints: FlowNode[];
}

export function flowForModule(id: ModuleId, title: string): FlowGraph {
  const meta = MODULE_META[id];
  return {
    source: { name: 'rate-limiter', role: 'ingress guard', kind: 'source' },
    core: { name: title, role: meta.desc, kind: meta.kind },
    endpoints: [
      { name: 'research', role: 'agent endpoint', kind: 'endpoint' },
      { name: 'briefing', role: 'agent endpoint', kind: 'endpoint' },
    ],
  };
}
