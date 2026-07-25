// CGUI-11 (TERM #534): unit coverage for the Harmony Forest Build simulation engine — the pure,
// DOM-free core. Uses an in-memory storage adapter and a synchronous scheduler so the fully
// timer-driven finish/join transitions run without real timers.
import { describe, it, expect } from 'vitest';
import { ForestEngine, SPECS, mulberry32, hashStr, clamp, type EngineOptions } from './forestEngine';

function memStorage() {
  const map = new Map<string, string>();
  return { get: (k: string) => map.get(k) ?? null, set: (k: string, v: string) => { map.set(k, v); }, map };
}

/** Options that never touch real localStorage/timers. `schedule` runs synchronously so the
 *  finish→joining→shipped→reload chain completes within the driving loop (fine for these tests;
 *  the panel uses real setTimeout). */
function testOpts(extra: Partial<EngineOptions> = {}): EngineOptions {
  return {
    storage: memStorage(),
    schedule: (fn: () => void) => { fn(); return 0 as unknown as ReturnType<typeof setTimeout>; },
    clearScheduled: () => {},
    ...extra,
  };
}

describe('mulberry32 / hashStr', () => {
  it('is deterministic for a fixed seed', () => {
    const a = mulberry32(12345);
    const b = mulberry32(12345);
    expect([a(), a(), a()]).toEqual([b(), b(), b()]);
  });
  it('hashStr is stable and unsigned', () => {
    expect(hashStr('auth-service')).toBe(hashStr('auth-service'));
    expect(hashStr('auth-service')).toBeGreaterThanOrEqual(0);
  });
});

describe('clamp', () => {
  it('bounds within range', () => {
    expect(clamp(5, 3, 7)).toBe(5);
    expect(clamp(1, 3, 7)).toBe(3);
    expect(clamp(9, 3, 7)).toBe(7);
  });
});

describe('buildTree', () => {
  it('is deterministic from spec name + size (same tree every time)', () => {
    const e = new ForestEngine(testOpts());
    const a = e.buildTree('auth-service', 12);
    const b = e.buildTree('auth-service', 12);
    expect(a.leaves.length).toBe(b.leaves.length);
    expect(a.trunk.d).toBe(b.trunk.d);
    expect(a.boughs.map(x => x.d)).toEqual(b.boughs.map(x => x.d));
  });
  it('produces exactly `size` leaves and clamps bough count to 3..7', () => {
    const e = new ForestEngine(testOpts());
    for (const size of [6, 12, 24, 40, 48]) {
      const t = e.buildTree('spec', size);
      expect(t.leaves.length).toBe(size);
      expect(t.boughs.length).toBeGreaterThanOrEqual(3);
      expect(t.boughs.length).toBeLessThanOrEqual(7);
    }
  });
});

describe('loadSpec', () => {
  it('plans one issue per leaf, all queued, staged at plan', () => {
    const e = new ForestEngine(testOpts());
    e.loadSpec('auth-service', 12);
    expect(e.issues.length).toBe(12);
    expect(e.issues.every(i => i.status === 'queued')).toBe(true);
    expect(e.issues.every(i => i.stage === 0)).toBe(true);
    expect(e.phase).toBe('idle');
    // seeded first log line
    expect(e.logsArr[0].text).toContain('spec auth-service loaded');
  });
});

describe('pipeline run', () => {
  it('drives every issue to done and ships the spec into the forest', () => {
    const storage = memStorage();
    // Peak `done` is captured via onChange because the synchronous scheduler runs the post-ship
    // reload (which resets `done` to 0) inside the same tick that finishes the branch.
    let peakDone = 0;
    const e = new ForestEngine(testOpts({ storage, onChange: undefined }));
    e.setOnChange(() => { peakDone = Math.max(peakDone, e.done); });
    e.loadSpec('auth-service', 12);
    e.speed = 2.5; // fast-forward
    e.start();
    expect(e.phase).toBe('building');

    // Bounded loop — each issue fails at most once (retried flag), so completion is guaranteed.
    let guard = 0;
    while (e.phase === 'building' && guard < 100000) { e.tickIfRunning(); guard++; }

    expect(guard).toBeLessThan(100000); // actually converged, didn't hit the guard ceiling
    expect(peakDone).toBe(12);          // every issue committed before the branch merged
    expect(e.shipped).toBe(1);          // one tree joined the forest (survives the reload)
    expect(storage.map.get('harmony.forest.v1')).toBeTruthy(); // forest persisted
  });

  it('respects the CONC=4 concurrency window', () => {
    const e = new ForestEngine(testOpts());
    e.loadSpec('platform-core', 40);
    e.speed = 0.001; // so nothing completes within a couple of ticks
    e.start();
    e.tickIfRunning();
    const building = e.issues.filter(i => i.status === 'building').length;
    expect(building).toBeLessThanOrEqual(4);
    expect(building).toBeGreaterThan(0);
  });
});

describe('controls', () => {
  it('setSize renames the spec to custom and rebuilds', () => {
    const e = new ForestEngine(testOpts());
    e.setSize(20);
    expect(e.specName).toBe('custom');
    expect(e.issues.length).toBe(20);
  });
  it('selectSpec loads a preset', () => {
    const e = new ForestEngine(testOpts());
    const preset = SPECS[2];
    e.selectSpec(preset.name, preset.issues);
    expect(e.specName).toBe(preset.name);
    expect(e.issues.length).toBe(preset.issues);
  });
  it('togglePause flips running', () => {
    const e = new ForestEngine(testOpts());
    e.loadSpec('auth-service', 12);
    e.start();
    expect(e.running).toBe(true);
    e.togglePause();
    expect(e.running).toBe(false);
  });
});
