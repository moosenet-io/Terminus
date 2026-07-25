// CGUI-12 (TERM #535): the crate grouping model — a curated client-side filter over the live
// module set (there is no backend crate entity). These lock in the guide-§3.1 invariants the
// Overview shell relies on: every module maps to exactly one crate, crate filtering is a subset,
// and flow-role grouping yields SOURCES → CORES → ENDPOINTS order with empty groups dropped.
import { describe, it, expect } from 'vitest';
import {
  CRATES,
  CRATE_ORDER,
  crateForModule,
  getCrate,
  modulesInCrate,
  groupByFlowRole,
} from './crates';
import type { ModuleDescriptor, ModuleId } from './moduleRegistry';

const ALL_MODULE_IDS: ModuleId[] = ['harmony', 'chord', 'lumina', 'muse', 'terminus', 'models', 'mint'];

function fakeModule(id: ModuleId): ModuleDescriptor {
  return {
    id,
    title: id,
    icon: '',
    healthSystem: (['harmony', 'chord', 'lumina', 'muse'].includes(id) ? id : 'terminus') as
      ModuleDescriptor['healthSystem'],
    order: 0,
  };
}

describe('crate model', () => {
  it('partitions every ModuleId into exactly one crate', () => {
    const seen = new Set<ModuleId>();
    for (const crate of CRATES) {
      for (const m of crate.moduleIds) {
        expect(seen.has(m)).toBe(false); // no module belongs to two crates
        seen.add(m);
      }
    }
    // every live module is covered
    for (const id of ALL_MODULE_IDS) expect(seen.has(id)).toBe(true);
  });

  it('crateForModule agrees with the crate membership tables', () => {
    for (const crate of CRATES) {
      for (const m of crate.moduleIds) {
        expect(crateForModule(m)).toBe(crate.id);
      }
    }
  });

  it('exposes the three guide crates in hero order', () => {
    expect(CRATE_ORDER).toEqual(['lumina-core', 'chord-proxy', 'terminus-rs']);
    expect(getCrate('chord-proxy').title).toBe('chord-proxy');
  });

  it('modulesInCrate returns only that crate members, preserving order', () => {
    const all = ALL_MODULE_IDS.map(fakeModule);
    const chord = modulesInCrate('chord-proxy', all);
    expect(chord.map(m => m.id)).toEqual(['chord', 'models', 'mint']);
    // subset of the input, never invents modules
    expect(chord.every(m => all.includes(m))).toBe(true);
  });

  it('groupByFlowRole buckets by kind in SOURCES→CORES→ENDPOINTS order, dropping empty groups', () => {
    // chord-proxy holds a core (chord) + two sources (models, mint) — no endpoints.
    const chordMods = modulesInCrate('chord-proxy', ALL_MODULE_IDS.map(fakeModule));
    const groups = groupByFlowRole(chordMods);
    expect(groups.map(g => g.label)).toEqual(['SOURCES', 'CORES']); // ENDPOINTS/CLOUD dropped (empty)
    expect(groups[0].modules.map(m => m.id)).toEqual(['models', 'mint']);
    expect(groups[1].modules.map(m => m.id)).toEqual(['chord']);
  });

  it('groupByFlowRole on the full fleet keeps flow order and every module', () => {
    const groups = groupByFlowRole(ALL_MODULE_IDS.map(fakeModule));
    expect(groups.map(g => g.label)).toEqual(['SOURCES', 'CORES', 'ENDPOINTS']);
    const total = groups.reduce((n, g) => n + g.modules.length, 0);
    expect(total).toBe(ALL_MODULE_IDS.length);
  });
});
