// MUSE-124 (MACT-04 review, round 2): proves `registerModuleIfAbsent`'s actual guarantee —
// first registration wins, a later call for the same id is a genuine no-op — rather than
// leaving that property as a convention a call site has to remember to enforce. Per the
// project's revert-and-confirm-failure discipline: this test was checked by hand to FAIL if
// `registerModuleIfAbsent`'s `has()` guard is removed (i.e. if it behaved like plain
// `registerModule` and just overwrote).
import { describe, it, expect, afterEach } from 'vitest';
import { clearModuleRegistry, getAllModules, registerModule, registerModuleIfAbsent } from './moduleRegistry';
import type { ModuleDescriptor } from './moduleRegistry';

afterEach(() => {
  clearModuleRegistry();
});

function maestroDescriptor(title: string): ModuleDescriptor {
  return { id: 'maestro', title, icon: '▶', healthSystem: 'muse', order: 8 };
}

describe('registerModuleIfAbsent', () => {
  it('registers and returns true when nothing is registered for that id yet', () => {
    const registered = registerModuleIfAbsent(maestroDescriptor('Maestro (MACT-04)'));
    expect(registered).toBe(true);
    expect(getAllModules().find(m => m.id === 'maestro')?.title).toBe('Maestro (MACT-04)');
  });

  // The load-bearing case: two independent spec items (MACT-04 and spec G) both register
  // `maestro` with DIFFERENT descriptors and no ordering guarantee between them. Whichever
  // runs first must survive; the second must be a real no-op, not a silent overwrite.
  it('first registration wins — a second call with a DIFFERENT descriptor does not overwrite it', () => {
    const first = registerModuleIfAbsent(maestroDescriptor('Maestro (MACT-04)'));
    const second = registerModuleIfAbsent(maestroDescriptor('Maestro (spec G)'));

    expect(first).toBe(true);
    expect(second).toBe(false); // the second call reports that it did NOT register
    expect(getAllModules().find(m => m.id === 'maestro')?.title).toBe('Maestro (MACT-04)');
  });

  it('is order-independent — whichever call runs first still wins', () => {
    registerModuleIfAbsent(maestroDescriptor('Maestro (spec G)'));
    registerModuleIfAbsent(maestroDescriptor('Maestro (MACT-04)'));

    expect(getAllModules().find(m => m.id === 'maestro')?.title).toBe('Maestro (spec G)');
  });

  it('does not affect other already-registered modules', () => {
    registerModule({ id: 'muse', title: 'Muse', icon: '◎', healthSystem: 'muse', order: 3 });
    registerModuleIfAbsent(maestroDescriptor('Maestro (MACT-04)'));
    registerModuleIfAbsent(maestroDescriptor('Maestro (spec G)'));

    expect(getAllModules().find(m => m.id === 'muse')?.title).toBe('Muse');
    expect(getAllModules().find(m => m.id === 'maestro')?.title).toBe('Maestro (MACT-04)');
  });
});
