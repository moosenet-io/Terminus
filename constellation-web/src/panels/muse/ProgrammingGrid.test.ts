// MGUI-10: the rules that make the programming grid HONEST rather than decorative. Each test
// below guards a specific "never invent data" invariant, not just arithmetic.
import { describe, it, expect } from 'vitest';
import { museChannelList, museGuideEntries, type MuseGuideEntry } from '../../hooks/useMuse';
import { deriveWindow, blockGeometry, buildRows } from './ProgrammingGrid';

const T0 = Date.UTC(2026, 6, 31, 20, 0, 0); // 20:00Z
const entry = (channel_id: string, offsetH: number, durH: number, title = 'x'): MuseGuideEntry => ({
  channel_id,
  title,
  start: new Date(T0 + offsetH * 3_600_000).toISOString(),
  end: new Date(T0 + (offsetH + durH) * 3_600_000).toISOString(),
});

describe('/api/channels shape normalization', () => {
  // The live endpoint answers a bare array; the mock answers an envelope. Reading only
  // `data.channels` would render an empty list against a populated backend.
  it('accepts both the live bare array and the mocked envelope', () => {
    expect(museChannelList([{ id: 'a', name: 'A', item_count: 1 }])).toHaveLength(1);
    expect(museChannelList({ channels: [{ id: 'a', name: 'A', item_count: 1 }] })).toHaveLength(1);
  });

  it('yields an empty list rather than a guess for a null or unrecognized payload', () => {
    expect(museChannelList(null)).toEqual([]);
    expect(museChannelList({} as never)).toEqual([]);
  });
});

describe('/guide HTML payload', () => {
  // The live `/guide` is a rendered HTML page wrapped by the proxy as {raw}. It must be
  // reported as such, and it must NEVER be scraped into programme blocks.
  it('reports an HTML-only response instead of producing entries from it', () => {
    const res = museGuideEntries({ raw: '<!doctype html><html><body>21:00 Something</body></html>' });
    expect(res.entries).toEqual([]);
    expect(res.htmlOnly).toBe(true);
  });

  it('does not flag a real entries envelope as HTML-only', () => {
    const res = museGuideEntries({ entries: [entry('ch-1', 0, 1)] });
    expect(res.entries).toHaveLength(1);
    expect(res.htmlOnly).toBe(false);
  });
});

describe('time window derivation', () => {
  // No entries => no axis. A hardcoded 48h frame would put a now marker at a meaningless
  // position on an empty grid.
  it('returns null with no entries rather than inventing a window', () => {
    expect(deriveWindow([], T0)).toBeNull();
  });

  it('spans the entries that exist, snapped to whole hours', () => {
    const win = deriveWindow([entry('ch-1', 0, 2), entry('ch-2', 3, 1)], T0 + 3_600_000)!;
    expect(win.startMs).toBe(T0);
    expect(win.endMs).toBe(T0 + 4 * 3_600_000);
    expect(win.ticks[0]).toBe(T0);
  });

  it('ignores an unparseable timestamp instead of coercing it to now', () => {
    const bad: MuseGuideEntry = { channel_id: 'ch-1', title: 'bad', start: 'not-a-date', end: 'nope' };
    expect(deriveWindow([bad], T0)).toBeNull();
  });

  it('does not stretch the axis to reach a now far outside the schedule', () => {
    const far = T0 + 400 * 3_600_000;
    const win = deriveWindow([entry('ch-1', 0, 2)], far)!;
    expect(win.endMs).toBeLessThan(far);
  });
});

describe('block geometry', () => {
  it('places a block proportionally within the window', () => {
    const win = deriveWindow([entry('ch-1', 0, 4)], T0)!;
    const geo = blockGeometry(entry('ch-1', 1, 1), win)!;
    expect(geo.leftPct).toBeCloseTo(25, 5);
    expect(geo.widthPct).toBeCloseTo(25, 5);
  });

  it('truncates rather than overflowing when an entry runs past the frame', () => {
    const win = deriveWindow([entry('ch-1', 0, 2)], T0)!;
    const geo = blockGeometry(entry('ch-1', 1, 10), win)!;
    expect(geo.leftPct + geo.widthPct).toBeLessThanOrEqual(100.0001);
  });

  it('returns null for an unparseable entry instead of a default position', () => {
    const win = deriveWindow([entry('ch-1', 0, 2)], T0)!;
    expect(blockGeometry({ channel_id: 'ch-1', title: 'x', start: 'x', end: 'y' }, win)).toBeNull();
  });
});

describe('row assembly', () => {
  it('keeps a channel with no programming as an explicit empty row', () => {
    const rows = buildRows([{ id: 'ch-1', name: 'One', item_count: 3 }], []);
    expect(rows).toHaveLength(1);
    expect(rows[0].entries).toEqual([]);
  });

  it('surfaces a guide entry naming an unknown channel rather than dropping it', () => {
    const rows = buildRows([], [entry('ghost', 0, 1)]);
    expect(rows).toHaveLength(1);
    expect(rows[0].label).toBe('ghost');
    expect(rows[0].channel).toBeNull();
  });

  it('produces no rows at all when there are neither channels nor entries', () => {
    expect(buildRows([], [])).toEqual([]);
  });
});
