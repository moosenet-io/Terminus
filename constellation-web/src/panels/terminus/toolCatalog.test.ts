// CGUI-05 (TERM #528): unit coverage for the pure per-tool depth derivation. Component/DOM
// rendering isn't covered (no jsdom in this project — same convention as the other logic-only
// suites); these lock the shapes the Tools catalog renders from.
import { describe, it, expect } from 'vitest';
import {
  deriveToolDetail, paramsFor, lastInvocationFor, CATEGORY_BADGE, CATEGORY_DOT_COLOR,
  fmtLatency, FIXTURE_NOW,
} from './toolCatalog';

describe('deriveToolDetail — real vs derived facts', () => {
  it('keeps name/module/enabled as the real facts', () => {
    const d = deriveToolDetail('plane', 'plane_create_work_item', true);
    expect(d.name).toBe('plane_create_work_item');
    expect(d.module).toBe('plane');
    expect(d.enabled).toBe(true);
  });

  it('buckets the verb into a category', () => {
    expect(deriveToolDetail('plane', 'plane_list_work_items', true).category).toBe('read');
    expect(deriveToolDetail('plane', 'plane_create_work_item', true).category).toBe('write');
    expect(deriveToolDetail('plane', 'plane_delete_work_item', true).category).toBe('admin');
    expect(deriveToolDetail('plane', 'plane_search_work_items', true).category).toBe('search');
  });

  it('humanises the description from the action tokens', () => {
    expect(deriveToolDetail('gitea', 'gitea_merge_pr', true).description).toBe('Merge pr');
    expect(deriveToolDetail('plane', 'plane_create_work_item', true).description).toBe('Create work item');
  });

  it('sources rate-limit + auth identity from the owning module policy', () => {
    expect(deriveToolDetail('plane', 'plane_get_project', true).auth).toBe('PLANE_API_KEY');
    expect(deriveToolDetail('gitea', 'gitea_list_repos', true).rateLimit).toBe('60 / min');
    // unknown module falls back to the vault-managed default rather than throwing
    expect(deriveToolDetail('mystery', 'mystery_do_thing', true).auth).toBe('vault-managed');
  });

  it('suppresses telemetry for a disabled tool', () => {
    expect(deriveToolDetail('nexus', 'nexus_list_items', false).lastInvocation).toBeNull();
  });

  it('is fully deterministic with default args (no wall-clock) — same input, same output', () => {
    // The codex-flagged bug: a Date.now() default made identical fixture inputs produce
    // different lastInvocation.ts across calls. It must now be stable across calls.
    const a = deriveToolDetail('plane', 'plane_get_project', true);
    const b = deriveToolDetail('plane', 'plane_get_project', true);
    expect(a).toEqual(b);
    expect(a.lastInvocation).toEqual(b.lastInvocation);
    // and the default epoch is the fixed FIXTURE_NOW constant, not the wall clock
    expect(deriveToolDetail('plane', 'plane_get_project', true))
      .toEqual(deriveToolDetail('plane', 'plane_get_project', true, FIXTURE_NOW));
  });
});

describe('paramsFor — derived schema is never empty', () => {
  it('gives list a paginated read schema', () => {
    const p = paramsFor('list', ['work', 'items']);
    expect(p.map(x => x.name)).toEqual(['limit', 'cursor']);
    expect(p.every(x => !x.required)).toBe(true);
  });

  it('requires a query for search and an id for get', () => {
    expect(paramsFor('search', ['work', 'items'])[0]).toMatchObject({ name: 'query', required: true });
    expect(paramsFor('get', ['project'])[0]).toMatchObject({ name: 'project_id', required: true });
  });

  it('update carries both an id and a fields patch', () => {
    const p = paramsFor('update', ['work', 'item']);
    expect(p.map(x => x.name)).toContain('fields');
    expect(p.some(x => x.name.endsWith('_id') && x.required)).toBe(true);
  });

  it('always returns at least one representative row for an unknown verb', () => {
    expect(paramsFor('frobnicate', ['thing']).length).toBeGreaterThan(0);
  });
});

describe('lastInvocationFor — deterministic synthetic telemetry', () => {
  it('is stable for the same name (no render flicker)', () => {
    const now = Date.parse('2026-07-25T04:00:00Z');
    expect(lastInvocationFor('plane_list_projects', now)).toEqual(lastInvocationFor('plane_list_projects', now));
  });

  it('produces ok/error results and a relative ago label when present', () => {
    const now = Date.parse('2026-07-25T04:00:00Z');
    const inv = lastInvocationFor('gitea_list_repos', now);
    if (inv) {
      expect(['ok', 'error']).toContain(inv.result);
      expect(inv.ago).toMatch(/(s|m|h|d) ago$/);
    }
  });

  it('marks a realistic share of tools as never-invoked (null)', () => {
    const now = Date.parse('2026-07-25T04:00:00Z');
    // Scan a broad synthetic namespace — the ~1-in-6 null branch must fire for some, and NOT
    // for all (so the panel still shows live telemetry on most rows).
    const names = Array.from({ length: 60 }, (_, i) => `mod_action_${i}`);
    const results = names.map(n => lastInvocationFor(n, now));
    expect(results.some(r => r === null)).toBe(true);
    expect(results.some(r => r !== null)).toBe(true);
  });
});

describe('CATEGORY_BADGE', () => {
  it('maps every category to a Badge tone', () => {
    expect(CATEGORY_BADGE.read).toBe('blue');
    expect(CATEGORY_BADGE.write).toBe('green');
    expect(CATEGORY_BADGE.search).toBe('amber');
    expect(CATEGORY_BADGE.admin).toBe('rose');
  });
});

// S127 TGUI2 POL-06/M6: the Latency column + the neutral-chip semantic dot colors.
describe('latency telemetry (POL-06)', () => {
  it('attaches a deterministic latency to an invoked tool', () => {
    const a = lastInvocationFor('gitea_list_repos', FIXTURE_NOW);
    const b = lastInvocationFor('gitea_list_repos', FIXTURE_NOW);
    if (a && b) {
      expect(a.latencyMs).toBe(b.latencyMs);
      expect(a.latencyMs).toBeGreaterThan(0);
    }
  });

  it('threads latency onto the derived detail and drops it for a disabled tool', () => {
    const d = deriveToolDetail('plane', 'plane_get_project', true);
    expect(d.lastInvocation === null || typeof d.lastInvocation.latencyMs === 'number').toBe(true);
    expect(deriveToolDetail('plane', 'plane_get_project', false).lastInvocation).toBeNull();
  });

  it('formats ms under a second and seconds above', () => {
    expect(fmtLatency(240)).toBe('240ms');
    expect(fmtLatency(1500)).toBe('1.50s');
  });
});

describe('CATEGORY_DOT_COLOR (M6)', () => {
  it('gives every kind a CSS-var semantic dot ink for the neutral chip', () => {
    for (const cat of ['read', 'write', 'search', 'admin'] as const) {
      expect(CATEGORY_DOT_COLOR[cat]).toMatch(/^var\(--/);
    }
  });
});
