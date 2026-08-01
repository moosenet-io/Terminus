// MGUI-20: Movies and TV Shows are what Library CONTAINS, not siblings beside it. Flat, the rail
// showed three entries over the same rows — two partitioning the third, with nothing saying so.
import { describe, it, expect } from 'vitest';
import { railOrder, type PanelDescriptor } from './moduleRegistry';

const p = (id: string, extra: Partial<PanelDescriptor> = {}): PanelDescriptor => ({
  id,
  system: 'muse',
  title: id,
  path: `/${id}`,
  available: true,
  component: () => null,
  ...extra,
});

describe('rail nesting', () => {
  it('puts children directly under their parent, indented', () => {
    const out = railOrder([
      p('muse.dashboard'),
      p('muse.library', { groupOnly: true }),
      p('muse.discover'),
      p('muse.library.movies', { parentId: 'muse.library' }),
      p('muse.library.shows', { parentId: 'muse.library' }),
    ]);
    expect(out.map(o => `${o.panel.id}@${o.depth}`)).toEqual([
      'muse.dashboard@0',
      'muse.library@0',
      'muse.library.movies@1',
      'muse.library.shows@1',
      'muse.discover@0',
    ]);
  });

  it('never emits a child twice', () => {
    // The parent loop and the outer loop both walk the same array; a child must be skipped at
    // top level exactly because it is emitted under its parent.
    const out = railOrder([p('a'), p('b', { parentId: 'a' })]);
    expect(out.filter(o => o.panel.id === 'b')).toHaveLength(1);
  });

  it('falls back to FLAT for a child whose parent is absent', () => {
    // A mis-typed parentId, or a parent that is unavailable/hidden, must degrade to the old
    // layout — not silently remove the panel from navigation altogether.
    const out = railOrder([p('orphan', { parentId: 'does.not.exist' })]);
    expect(out).toEqual([{ panel: expect.objectContaining({ id: 'orphan' }), depth: 1 - 1 }]);
    expect(out[0].depth).toBe(0);
  });

  it('leaves a flat list untouched', () => {
    const out = railOrder([p('a'), p('b'), p('c')]);
    expect(out.map(o => o.panel.id)).toEqual(['a', 'b', 'c']);
    expect(out.every(o => o.depth === 0)).toBe(true);
  });

  it('preserves registration order among children', () => {
    const out = railOrder([
      p('lib'),
      p('shows', { parentId: 'lib' }),
      p('movies', { parentId: 'lib' }),
    ]);
    expect(out.map(o => o.panel.id)).toEqual(['lib', 'shows', 'movies']);
  });
});
