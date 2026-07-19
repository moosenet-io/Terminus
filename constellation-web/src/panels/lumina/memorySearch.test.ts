// LGUI-08: unit coverage for the Memory browser's pure logic — server-side filtering
// simulation, query-string building, preview clamping, and the superseded-by chain walk.
import { describe, expect, it } from 'vitest';
import {
  applyMemorySearchParams,
  buildMemorySearchQuery,
  clampPreview,
  supersededChain,
} from './memorySearch';
import type { Memory } from '../../types/luminaMemory';

function memory(overrides: Partial<Memory> & { id: string }): Memory {
  return {
    memory_type: 'Episodic',
    sensitivity: 'None',
    visibility: 'Private',
    content: 'placeholder content',
    confidence: 0.5,
    created_at: '2026-01-01T00:00:00Z',
    access_count: 0,
    user_id: 'admin',
    provenance: { conversation_id: null, turn_index: null, source: 'chat' },
    superseded_by: null,
    ...overrides,
  };
}

describe('buildMemorySearchQuery', () => {
  it('omits empty/undefined fields entirely', () => {
    expect(buildMemorySearchQuery({})).toBe('/search');
    expect(buildMemorySearchQuery({ q: '  ' })).toBe('/search');
  });

  it('includes only the params actually set', () => {
    const qs = buildMemorySearchQuery({ q: 'weather', type: 'Preference', limit: 25 });
    expect(qs).toBe('/search?q=weather&type=Preference&limit=25');
  });

  it('trims whitespace from q', () => {
    expect(buildMemorySearchQuery({ q: '  hello  ' })).toBe('/search?q=hello');
  });
});

describe('applyMemorySearchParams', () => {
  const all: Memory[] = [
    memory({ id: 'a', memory_type: 'Principle', sensitivity: 'Health', content: 'physical therapy plan' }),
    memory({ id: 'b', memory_type: 'Preference', sensitivity: 'None', content: 'likes celsius' }),
    memory({ id: 'c', memory_type: 'Preference', sensitivity: 'Finance', content: 'budget review monthly', user_id: 'member-1' }),
    memory({ id: 'd', memory_type: 'Episodic', sensitivity: 'None', content: 'asked about celsius twice' }),
  ];

  it('filters server-side by type', () => {
    const rows = applyMemorySearchParams(all, { type: 'Preference' });
    expect(rows.map(r => r.id)).toEqual(['b', 'c']);
  });

  it('filters by sensitivity', () => {
    const rows = applyMemorySearchParams(all, { sensitivity: 'Health' });
    expect(rows.map(r => r.id)).toEqual(['a']);
  });

  it('filters by user scope', () => {
    const rows = applyMemorySearchParams(all, { user: 'member-1' });
    expect(rows.map(r => r.id)).toEqual(['c']);
  });

  it('applies a case-insensitive content search', () => {
    const rows = applyMemorySearchParams(all, { q: 'CELSIUS' });
    expect(rows.map(r => r.id).sort()).toEqual(['b', 'd']);
  });

  it('combines multiple filters (AND semantics)', () => {
    const rows = applyMemorySearchParams(all, { type: 'Preference', q: 'celsius' });
    expect(rows.map(r => r.id)).toEqual(['b']);
  });

  it('respects limit, defaulting to 50', () => {
    const rows = applyMemorySearchParams(all, { limit: 2 });
    expect(rows).toHaveLength(2);
  });
});

describe('clampPreview', () => {
  it('leaves short content untouched', () => {
    expect(clampPreview('short')).toBe('short');
  });

  it('truncates pathologically long content with an ellipsis', () => {
    const huge = 'x'.repeat(500);
    const clamped = clampPreview(huge);
    expect(clamped.length).toBeLessThan(huge.length);
    expect(clamped.endsWith('…')).toBe(true);
  });
});

describe('supersededChain', () => {
  it('walks a superseded chain in order', () => {
    const byId = new Map<string, Memory>([
      ['mem-006', memory({ id: 'mem-006', superseded_by: 'mem-002' })],
      ['mem-002', memory({ id: 'mem-002', superseded_by: null })],
    ]);
    expect(supersededChain(byId, 'mem-006')).toEqual(['mem-006', 'mem-002']);
  });

  it('never infinite-loops on a malformed cycle', () => {
    const byId = new Map<string, Memory>([
      ['a', memory({ id: 'a', superseded_by: 'b' })],
      ['b', memory({ id: 'b', superseded_by: 'a' })],
    ]);
    expect(supersededChain(byId, 'a')).toEqual(['a', 'b']);
  });
});
