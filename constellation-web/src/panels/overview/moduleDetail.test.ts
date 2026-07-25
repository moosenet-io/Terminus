// CGUI-04 (TERM #527): unit coverage for the reusable module-detail view's pure helpers —
// the configuration/flow reference tables (moduleMeta) and the synthetic telemetry line
// builder (ModuleDetail). Component/DOM rendering isn't covered here (no jsdom/testing-library
// in this project — same convention as the other logic-only test suites); these lock the data
// shapes every detail region is built from.
import { describe, it, expect } from 'vitest';
import { configForModule, flowForModule, MODULE_META } from './moduleMeta';
import { makeLogLine } from './ModuleDetail';
import type { ModuleId } from '../../lib/moduleRegistry';

const ALL_IDS = Object.keys(MODULE_META) as ModuleId[];

describe('configForModule (§4 CONFIGURATION quartet)', () => {
  it('returns the four guide-spec rows for every module', () => {
    for (const id of ALL_IDS) {
      const rows = configForModule(id);
      expect(rows.map(r => r.key)).toEqual(['Rate limit', 'Timeout', 'Auth vault', 'Circuit breaker']);
    }
  });

  it('renders auth vault as a green badge and circuit breaker as an amber badge', () => {
    const rows = configForModule('terminus');
    const vault = rows.find(r => r.key === 'Auth vault')!;
    const breaker = rows.find(r => r.key === 'Circuit breaker')!;
    expect(vault.badge).toEqual({ tone: 'green', label: 'encrypted' });
    expect(breaker.badge?.tone).toBe('amber');
    expect(breaker.badge?.label).toContain('cap');
  });

  it('gives rate limit / timeout plain mono values (no badge)', () => {
    const rows = configForModule('chord');
    const rate = rows.find(r => r.key === 'Rate limit')!;
    expect(rate.value).toMatch(/\/ min$/);
    expect(rate.badge).toBeUndefined();
  });
});

describe('flowForModule (§4 POSITION IN FLOW)', () => {
  it('centres the inspected module as the core node with its own kind', () => {
    const g = flowForModule('lumina', 'Lumina');
    expect(g.core.name).toBe('Lumina');
    expect(g.core.kind).toBe(MODULE_META.lumina.kind);
    expect(g.source.kind).toBe('source');
    expect(g.endpoints).toHaveLength(2);
    expect(g.endpoints.every(e => e.kind === 'endpoint')).toBe(true);
  });
});

describe('makeLogLine (§4 LIVE LOG)', () => {
  it('is deterministic per (module, seq) and carries a zero cost', () => {
    const at = new Date('2026-07-25T04:03:09');
    const a = makeLogLine('terminus', 3, at);
    const b = makeLogLine('terminus', 3, at);
    expect(a).toEqual(b);
    expect(a.time).toBe('04:03:09');
    expect(a.cost).toBe('0.00');
    expect(a.event).toContain('terminus.');
  });

  it('tags every fifth line in-flight ([..]) and the rest settled ([ok])', () => {
    expect(makeLogLine('chord', 0).tag).toBe('[..]');
    expect(makeLogLine('chord', 5).tag).toBe('[..]');
    expect(makeLogLine('chord', 1).tag).toBe('[ok]');
    expect(makeLogLine('chord', 4).tag).toBe('[ok]');
  });
});
