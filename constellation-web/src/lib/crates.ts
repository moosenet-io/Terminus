// CGUI-12 (TERM #535): the crate grouping model for the Overview shell (guide-spec §3.1).
//
// The brand guide's shell is CRATE-centric: the global bar carries three crate tabs
// (`lumina-core` / `chord-proxy` / `terminus-rs`) and the left rail lists the *active crate's*
// modules grouped by flow role. The running app, however, models only MODULES (moduleRegistry's
// `ModuleId` — harmony/chord/lumina/muse/terminus/models/mint); there is no backend "crate"
// entity and no machine-readable module→crate binding yet.
//
// PLACEHOLDER NOTE: the CRATE_MEMBERS map below is therefore a curated, CLIENT-SIDE grouping —
// a "sensible filter" (per the CGUI-12 task) that buckets the live modules under the three guide
// crates so the crate tabs actually do something (they filter the rail + card canvas to their
// members). It is NOT live data. Swap it for the real crate manifest once the aggregation client
// exposes one (same posture as moduleMeta.ts's flow-role placeholders).
import type { ModuleDescriptor, ModuleId } from './moduleRegistry';
import { MODULE_META } from '../panels/overview/moduleMeta';
import type { NodeKind } from '../components/NodeBadge';

/** The three guide-spec crates (§3.1 / §00 hero: "lumina-core, chord-proxy, terminus-rs"). */
export type CrateId = 'lumina-core' | 'chord-proxy' | 'terminus-rs';

export interface CrateDescriptor {
  id: CrateId;
  /** Shown as the crate tab label and the Overview breadcrumb/title. Lowercase crate name,
   *  per the DS content voice (§9 "lowercase crate/command names"). */
  title: CrateId;
  /** The modules this crate owns, in display order. */
  moduleIds: ModuleId[];
}

/** Fixed crate order — matches the guide hero string "lumina-core, chord-proxy, terminus-rs". */
export const CRATE_ORDER: readonly CrateId[] = ['lumina-core', 'chord-proxy', 'terminus-rs'];

/** Curated module→crate buckets (see PLACEHOLDER NOTE above):
 *  - lumina-core  → the assistant surfaces (lumina + muse)
 *  - chord-proxy  → the inference proxy and the model catalog/benchmarks it routes to
 *  - terminus-rs  → the tool-hub + build-orchestrator infra */
const CRATE_MEMBERS: Record<CrateId, ModuleId[]> = {
  'lumina-core': ['lumina', 'muse'],
  'chord-proxy': ['chord', 'models', 'mint'],
  'terminus-rs': ['terminus', 'harmony'],
};

export const CRATES: readonly CrateDescriptor[] = CRATE_ORDER.map(id => ({
  id,
  title: id,
  moduleIds: CRATE_MEMBERS[id],
}));

const MODULE_TO_CRATE: Record<ModuleId, CrateId> = (() => {
  const out = {} as Record<ModuleId, CrateId>;
  for (const crate of CRATES) {
    for (const m of crate.moduleIds) out[m] = crate.id;
  }
  return out;
})();

/** The crate a module belongs to; falls back to the first crate for any id not in the map
 *  (defensive — every current ModuleId is mapped above). */
export function crateForModule(id: ModuleId): CrateId {
  return MODULE_TO_CRATE[id] ?? CRATE_ORDER[0];
}

export function getCrate(id: CrateId): CrateDescriptor {
  return CRATES.find(c => c.id === id) ?? CRATES[0];
}

/** Filter a live (available) module list down to one crate's members, preserving the incoming
 *  order. Used by the Overview to scope both the rail and the card canvas to the active crate. */
export function modulesInCrate(crateId: CrateId, modules: ModuleDescriptor[]): ModuleDescriptor[] {
  const members = new Set(getCrate(crateId).moduleIds as string[]);
  return modules.filter(m => members.has(m.id));
}

// ── Flow-role grouping (guide-spec §3.1 rail: SOURCES / CORES / ENDPOINTS) ────────────────────

export interface FlowGroup {
  /** Tracked-mono uppercase group label (SOURCES / CORES / ENDPOINTS / CLOUD). */
  label: string;
  /** The NodeKind whose accent colour dots this group's module rows (source blue / core violet /
   *  endpoint green / cloud amber). */
  kind: NodeKind;
  modules: ModuleDescriptor[];
}

/** Group definitions in fixed source→core→endpoint→cloud flow order. Only non-empty groups are
 *  returned by `groupByFlowRole`. */
const FLOW_GROUPS: readonly { label: string; kind: NodeKind }[] = [
  { label: 'SOURCES', kind: 'source' },
  { label: 'CORES', kind: 'core' },
  { label: 'ENDPOINTS', kind: 'endpoint' },
  { label: 'CLOUD', kind: 'cloud' },
];

/** Bucket a module list by each module's flow role (from MODULE_META[id].kind) into the guide's
 *  SOURCES / CORES / ENDPOINTS (/ CLOUD) groups, dropping any group with no members. Preserves
 *  incoming module order within each group. */
export function groupByFlowRole(modules: ModuleDescriptor[]): FlowGroup[] {
  return FLOW_GROUPS.map(g => ({
    label: g.label,
    kind: g.kind,
    modules: modules.filter(m => MODULE_META[m.id].kind === g.kind),
  })).filter(g => g.modules.length > 0);
}
