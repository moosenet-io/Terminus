// S127 TGUI2 (Part A): the CONSTELLATION CORE model — the real top-level nav.
//
// This replaces CGUI-12's fictional 3-"crate" strip (`lumina-core`/`chord-proxy`/`terminus-rs`),
// which was a curated client-side grouping with no backend reality that mis-bucketed the fleet
// (Muse buried under lumina-core, Harmony buried under terminus-rs, Models/MINT under chord-proxy).
//
// The REAL constellation members (see <path>/CLAUDE.md) are five cores:
//     Lumina · Chord · Terminus · Harmony · Muse
// Every registered module maps to exactly one core. The ONLY re-grouping vs "each module is its
// own core" is that Terminus OWNS its intake subsystems — Models and MINT are `src/intake/`
// subsystems of Terminus, so they render as sub-sections under the Terminus core, NOT as their
// own top-level tabs (they stay registered modules with panels/routes/health; only the tab strip
// and the rail grouping change).
import type { ModuleDescriptor, ModuleId } from './moduleRegistry';
import { MODULE_META } from '../panels/overview/moduleMeta';

/** The five real constellation cores — the top-level GlobalBar tabs. */
export type CoreId = 'lumina' | 'chord' | 'terminus' | 'harmony' | 'muse';

/** Fixed core order across the top bar (the real constellation members). */
export const CORE_ORDER: readonly CoreId[] = ['lumina', 'chord', 'terminus', 'harmony', 'muse'];

export interface CoreDescriptor {
  id: CoreId;
  /** Shown as the core tab label and the Overview breadcrumb/title. */
  title: string;
  /** The ModuleIds whose panels render under this core, in display order. */
  moduleIds: ModuleId[];
}

/** Core → member modules. Terminus owns its intake subsystems (Models + MINT); MACT-04
 *  (MUSE-124) adds Muse's second subsystem, Maestro (live activity / playback control), the same
 *  "own module, nested under the parent core's rail" relationship. Every other core is exactly
 *  itself. */
const CORE_MEMBERS: Record<CoreId, ModuleId[]> = {
  lumina: ['lumina'],
  chord: ['chord'],
  terminus: ['terminus', 'models', 'mint'],
  harmony: ['harmony'],
  muse: ['muse', 'maestro'],
};

/** Display titles for the core tabs (proper-case constellation names). */
const CORE_TITLE: Record<CoreId, string> = {
  lumina: 'Lumina',
  chord: 'Chord',
  terminus: 'Terminus',
  harmony: 'Harmony',
  muse: 'Muse',
};

/** Human-readable sub-group labels for a core's member modules (used by the CoreRail when a core
 *  has more than one member — i.e. Terminus). Keyed by ModuleId. */
export const MEMBER_LABEL: Record<ModuleId, string> = {
  lumina: 'LUMINA',
  chord: 'CHORD',
  terminus: 'TERMINUS',
  harmony: 'HARMONY',
  muse: 'MUSE',
  models: 'MODELS',
  mint: 'MINT',
  maestro: 'MAESTRO',
};

export const CORES: readonly CoreDescriptor[] = CORE_ORDER.map(id => ({
  id,
  title: CORE_TITLE[id],
  moduleIds: CORE_MEMBERS[id],
}));

const MODULE_TO_CORE: Record<ModuleId, CoreId> = (() => {
  const out = {} as Record<ModuleId, CoreId>;
  for (const core of CORES) {
    for (const m of core.moduleIds) out[m] = core.id;
  }
  return out;
})();

/** The core a module belongs to; falls back to the first core for any id not in the map
 *  (defensive — every current ModuleId is mapped above). */
export function coreForModule(id: ModuleId): CoreId {
  return MODULE_TO_CORE[id] ?? CORE_ORDER[0];
}

export function getCore(id: CoreId): CoreDescriptor {
  return CORES.find(c => c.id === id) ?? CORES[0];
}

/** Filter a live (available) module list down to one core's members, preserving CORE_MEMBERS
 *  order (not the incoming order) so Terminus always reads Terminus → Models → MINT. */
export function modulesInCore(coreId: CoreId, modules: ModuleDescriptor[]): ModuleDescriptor[] {
  const order = getCore(coreId).moduleIds;
  const byId = new Map(modules.map(m => [m.id, m] as const));
  return order.map(id => byId.get(id)).filter((m): m is ModuleDescriptor => m != null);
}

/** The node-dot colour for a core tab — the member module's kind colour (violet core / green
 *  endpoint), so the row still reads semantically. Uses the core's own id (every CoreId is a
 *  ModuleId with a MODULE_META entry). */
export function coreKind(id: CoreId): (typeof MODULE_META)[ModuleId]['kind'] {
  return MODULE_META[id].kind;
}
