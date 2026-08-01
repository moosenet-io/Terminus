// S127 TGUI2 (Part A): the constellation-core grouping model — the real top-level nav. Locks in
// the invariants the two-tier shell relies on: five cores in fixed order, every registered module
// maps to exactly one core, and Terminus (and only Terminus) owns Models + MINT as sub-modules.
import { describe, it, expect } from 'vitest';
import {
  CORES,
  CORE_ORDER,
  coreForModule,
  getCore,
  modulesInCore,
  MEMBER_LABEL,
} from './cores';
import type { ModuleDescriptor, ModuleId } from './moduleRegistry';

const ALL_MODULE_IDS: ModuleId[] = ['harmony', 'chord', 'lumina', 'muse', 'terminus', 'models', 'mint', 'maestro'];

function fakeModule(id: ModuleId): ModuleDescriptor {
  const healthSystem: ModuleDescriptor['healthSystem'] =
    (['harmony', 'chord', 'lumina', 'muse'] as ModuleId[]).includes(id)
      ? (id as 'harmony' | 'chord' | 'lumina' | 'muse')
      : id === 'maestro'
        ? 'muse' // Maestro is a Muse subsystem — see moduleRegistry.ts's ModuleId doc.
        : 'terminus'; // models/mint
  return { id, title: id, icon: '', healthSystem, order: 0 };
}

describe('core model', () => {
  it('exposes the five real constellation cores in fixed order', () => {
    expect(CORE_ORDER).toEqual(['lumina', 'chord', 'terminus', 'harmony', 'muse']);
    expect(CORES.map(c => c.id)).toEqual(['lumina', 'chord', 'terminus', 'harmony', 'muse']);
    expect(CORES.map(c => c.title)).toEqual(['Lumina', 'Chord', 'Terminus', 'Harmony', 'Muse']);
  });

  it('partitions every ModuleId into exactly one core', () => {
    const seen = new Set<ModuleId>();
    for (const core of CORES) {
      for (const m of core.moduleIds) {
        expect(seen.has(m)).toBe(false); // no module belongs to two cores
        seen.add(m);
      }
    }
    for (const id of ALL_MODULE_IDS) expect(seen.has(id)).toBe(true); // every live module is covered
  });

  it('maps each module to the operator-correct core', () => {
    // The three moves that fix the operator's complaints:
    expect(coreForModule('muse')).toBe('muse'); // Muse is its OWN core (was buried under lumina-core)
    expect(coreForModule('harmony')).toBe('harmony'); // Harmony is a top-level core (was under terminus-rs)
    expect(coreForModule('models')).toBe('terminus'); // Models is a Terminus subsystem (was under chord-proxy)
    expect(coreForModule('mint')).toBe('terminus'); // MINT is a Terminus subsystem
    // The rest are themselves:
    expect(coreForModule('lumina')).toBe('lumina');
    expect(coreForModule('chord')).toBe('chord');
    expect(coreForModule('terminus')).toBe('terminus');
  });

  it('coreForModule agrees with the CORES membership tables', () => {
    for (const core of CORES) {
      for (const m of core.moduleIds) {
        expect(coreForModule(m)).toBe(core.id);
      }
    }
  });

  it('Terminus and Muse are the only multi-member cores, each owning its subsystems in order', () => {
    // Terminus owns its intake subsystems (Models, MINT).
    expect(getCore('terminus').moduleIds).toEqual(['terminus', 'models', 'mint']);
    // MACT-04 (MUSE-124): Muse owns Maestro (live activity / playback control) the same way.
    expect(getCore('muse').moduleIds).toEqual(['muse', 'maestro']);
    for (const c of CORES) {
      if (c.id !== 'terminus' && c.id !== 'muse') expect(c.moduleIds).toEqual([c.id]);
    }
  });

  it('modulesInCore returns only that core members, in core order', () => {
    const all = ALL_MODULE_IDS.map(fakeModule);
    const terminus = modulesInCore('terminus', all);
    expect(terminus.map(m => m.id)).toEqual(['terminus', 'models', 'mint']);
    // preserves core order even if the input list is shuffled
    const shuffled = ['mint', 'models', 'terminus'].map(id => fakeModule(id as ModuleId));
    expect(modulesInCore('terminus', shuffled).map(m => m.id)).toEqual(['terminus', 'models', 'mint']);
  });

  it('modulesInCore drops members that are not currently available', () => {
    // Only terminus itself available (models/mint filtered out upstream) → rail shows just Terminus.
    const onlyTerminus = [fakeModule('terminus')];
    expect(modulesInCore('terminus', onlyTerminus).map(m => m.id)).toEqual(['terminus']);
  });

  it('provides an uppercase sub-group label for every module', () => {
    for (const id of ALL_MODULE_IDS) {
      expect(MEMBER_LABEL[id]).toBe(id.toUpperCase());
    }
  });
});
