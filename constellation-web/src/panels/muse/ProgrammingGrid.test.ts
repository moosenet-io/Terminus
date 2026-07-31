// MGUI-10: the rules that make the programming grid HONEST rather than decorative. Each test
// below guards a specific "never invent data" invariant, not just arithmetic.
import { describe, it, expect } from 'vitest';
import { museChannelList, museGuideEntries, type MuseGuideEntry, type MuseChannel } from '../../hooks/useMuse';
import { deriveWindow, blockGeometry, buildRows, gridState } from './ProgrammingGrid';

const T0 = Date.UTC(2026, 6, 31, 20, 0, 0); // 20:00Z
const entry = (channel_id: string, offsetH: number, durH: number, title = 'x'): MuseGuideEntry => ({
  channel_id,
  title,
  start: new Date(T0 + offsetH * 3_600_000).toISOString(),
  end: new Date(T0 + (offsetH + durH) * 3_600_000).toISOString(),
});

/** A `ChannelSummary` as Muse actually returns it (Muse `src/web/guide.rs:35`) — no
 *  `item_count`, numeric id. These fixtures previously used the MOCK's invented shape, so
 *  they passed while the production type was wrong. */
const ch = (id: number, name: string): MuseChannel => ({
  id,
  name,
  kind: 'series',
  mode: 'shuffle',
  channel_number: null,
  enabled: true,
});

describe('/api/channels shape normalization', () => {
  // The live endpoint answers a bare array. The envelope form is still accepted because it
  // is the shape the guide's own reference payloads use; the mock was corrected to the live
  // bare array in this change, so the envelope case here is contract tolerance, not a
  // description of what the mock returns.
  it('accepts both the live bare array and the envelope form', () => {
    expect(museChannelList([ch(1, 'A')])).toHaveLength(1);
    expect(museChannelList({ channels: [ch(1, 'A')] })).toHaveLength(1);
  });

  it('returns null — not [] — for a body it cannot read', () => {
    expect(museChannelList({ unexpected: true } as never)).toBeNull();
    expect(museChannelList(null)).toBeNull();
  });

  it('keeps [] meaning exactly one thing: the server returned a list with no elements', () => {
    // This test previously asserted the OPPOSITE — that null and unrecognized payloads both
    // yield `[]`. That contract was the enabling half of the false-empty bug: it made an
    // unread body indistinguishable from an observed empty list. Reversed deliberately.
    expect(museChannelList([])).toEqual([]);
    expect(museChannelList({ channels: [] })).toEqual([]);
    expect(museChannelList({} as never)).toBeNull();
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
    const rows = buildRows([ch(1, 'One')], []);
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


describe('never asserts an empty list before one arrives', () => {
  // The regression: `museChannelList(null)` yields `[]`, so a still-in-flight or FAILED
  // channels fetch produced zero rows and rendered copy stating as fact that
  // "GET /api/channels returned an empty list" — an observation of a response that did not
  // exist. Each case below pins one state where that sentence must NOT be reachable.
  const base = {
    channelsLoading: false,
    guideLoading: false,
    channelsDegraded: false,
    channelsRecognized: true,
    guideDegraded: false,
    rowCount: 0,
  };

  it('reports loading while the channels fetch is in flight, not emptiness', () => {
    expect(gridState({ ...base, channelsLoading: true })).toBe('loading');
  });

  it('reports loading while the GUIDE fetch is in flight too', () => {
    // Both feed the grid; either one outstanding means the row set is not yet settled.
    expect(gridState({ ...base, guideLoading: true })).toBe('loading');
  });

  it('distinguishes a failed channels fetch from an empty one', () => {
    // An error is not an empty list. Collapsing the two is what let the panel claim the
    // library had no channels when the request had in fact failed.
    expect(gridState({ ...base, channelsDegraded: true })).toBe('channels-degraded');
  });

  it('ranks loading above degraded', () => {
    expect(gridState({ ...base, channelsLoading: true, channelsDegraded: true })).toBe('loading');
  });

  it('refuses to call an UNPARSEABLE channels body empty', () => {
    // `museChannelList` returns null for a shape it cannot read. Collapsing that to `[]`
    // would let the grid report "returned an empty list" about a body it never parsed.
    expect(gridState({ ...base, channelsRecognized: false })).toBe('channels-unrecognized');
  });

  it('surfaces a failed GUIDE fetch instead of implying an empty schedule', () => {
    // Channel rows with no blocks read as "nothing is scheduled". A guide that failed to
    // load is a different statement — and since the guide can itself contribute rows, even
    // zero rows is not a complete observation while it is down.
    expect(gridState({ ...base, guideDegraded: true, rowCount: 2 })).toBe('guide-degraded');
    expect(gridState({ ...base, guideDegraded: true, rowCount: 0 })).toBe('guide-degraded');
  });

  it('only calls it empty once BOTH fetches settled successfully with zero rows', () => {
    expect(gridState(base)).toBe('empty');
  });

  it('renders the grid when rows exist', () => {
    expect(gridState({ ...base, rowCount: 3 })).toBe('grid');
  });
});
