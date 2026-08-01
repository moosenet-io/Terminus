// MACT-05 (MUSE-125): coverage for the Import/acquisition section's pure helpers. Same
// mutation-test discipline as nowPlaying.test.ts — each test here is one that would be
// VIOLATED if the corresponding behaviour in importActivity.ts were deleted or reverted to a
// naive implementation (CLAUDE.md's "a test that would pass with the feature removed is worse
// than no test").
import { describe, it, expect } from 'vitest';
import {
  emptyQueueReason,
  formatQueueAge,
  formatQueueSize,
  groupQueueByPipelineStatus,
  importProgressDisplay,
  statusGroupLabel,
  wantedCountLabel,
  wiringDisplay,
} from './importActivity';
import type { MuseDownloadQueueRow } from '../../hooks/useMuse';

function row(overrides: Partial<MuseDownloadQueueRow> = {}): MuseDownloadQueueRow {
  return {
    id: 1,
    request_id: null,
    monitored_item_id: 5,
    release_title: 'Some.Movie.2024.1080p',
    indexer: 'Indexer1',
    protocol: 'torrent',
    status: 'downloading',
    size_bytes: 4_500_000_000,
    added_at: new Date('2026-08-01T00:00:00Z').toISOString(),
    progress: null,
    ...overrides,
  };
}

// ── THE REQUIRED TEST: null progress renders the seam, never a 0% bar ───────────────────────

describe('importProgressDisplay — the seam must render honestly, never a fabricated 0%', () => {
  it('a null progress renders "not tracked", tracked:false, and a tooltip naming the seam', () => {
    const p = importProgressDisplay(null);
    expect(p.label).toBe('not tracked');
    expect(p.tracked).toBe(false);
    expect(p.tooltip).toMatch(/SEAM/);
    // The literal failure mode this item exists to prevent: never render as a percentage.
    expect(p.label).not.toMatch(/%/);
  });

  it('an undefined progress (key absent) behaves identically to null', () => {
    const p = importProgressDisplay(undefined);
    expect(p.label).toBe('not tracked');
    expect(p.tracked).toBe(false);
  });

  it('a real numeric progress DOES render a percentage (future-proofs the seam closing)', () => {
    const p = importProgressDisplay(42);
    expect(p.label).toBe('42%');
    expect(p.tracked).toBe(true);
    expect(p.tooltip).toBeNull();
  });

  it('clamps an out-of-range numeric progress rather than rendering garbage', () => {
    expect(importProgressDisplay(150).label).toBe('100%');
    expect(importProgressDisplay(-5).label).toBe('0%');
  });
});

// ── Pipeline grouping — row order tells the story ────────────────────────────────────────────

describe('groupQueueByPipelineStatus — queued -> downloading -> importing -> completed order', () => {
  it('orders groups in pipeline order regardless of input order', () => {
    const rows = [
      row({ id: 1, status: 'completed' }),
      row({ id: 2, status: 'queued' }),
      row({ id: 3, status: 'importing' }),
      row({ id: 4, status: 'downloading' }),
    ];
    const groups = groupQueueByPipelineStatus(rows);
    expect(groups.map(g => g.status)).toEqual(['queued', 'downloading', 'importing', 'completed']);
  });

  it('omits empty groups rather than rendering an empty header for every status on every load', () => {
    const rows = [row({ id: 1, status: 'queued' })];
    const groups = groupQueueByPipelineStatus(rows);
    expect(groups).toHaveLength(1);
    expect(groups[0].status).toBe('queued');
  });

  it('appends an unrecognised status verbatim after the known pipeline, never dropping it', () => {
    const rows = [row({ id: 1, status: 'queued' }), row({ id: 2, status: 'failed' })];
    const groups = groupQueueByPipelineStatus(rows);
    expect(groups.map(g => g.status)).toEqual(['queued', 'failed']);
    expect(groups[1].known).toBe(false);
  });

  it('groups every row of a shared status together', () => {
    const rows = [row({ id: 1, status: 'downloading' }), row({ id: 2, status: 'downloading' })];
    const groups = groupQueueByPipelineStatus(rows);
    expect(groups[0].rows).toHaveLength(2);
  });
});

describe('statusGroupLabel — known statuses title-cased, unknown ones flagged', () => {
  it('title-cases a known status', () => {
    expect(statusGroupLabel({ status: 'downloading', known: true, rows: [] })).toBe('Downloading');
  });

  it('marks an unrecognised status verbatim, never coerced into a known label', () => {
    expect(statusGroupLabel({ status: 'weird_status', known: false, rows: [] })).toBe('weird_status (unrecognised)');
  });
});

// ── Size / age — facts, no editorialising ────────────────────────────────────────────────────

describe('formatQueueSize — never a fabricated 0 B', () => {
  it('formats bytes into a compact unit', () => {
    expect(formatQueueSize(4_500_000_000)).toBe('4.2 GB');
  });

  it('null, undefined, and 0 all render as "—", not "0 B"', () => {
    expect(formatQueueSize(null)).toBe('—');
    expect(formatQueueSize(undefined)).toBe('—');
    expect(formatQueueSize(0)).toBe('—');
  });
});

describe('formatQueueAge — a fact (elapsed time), never a verdict', () => {
  const now = new Date('2026-08-01T12:00:00Z').getTime();

  it('renders "just now" for a sub-minute age', () => {
    expect(formatQueueAge(new Date(now - 10_000).toISOString(), now)).toBe('just now');
  });

  it('renders minutes, hours, then days as the age grows', () => {
    expect(formatQueueAge(new Date(now - 5 * 60_000).toISOString(), now)).toBe('5m ago');
    expect(formatQueueAge(new Date(now - 3 * 3_600_000).toISOString(), now)).toBe('3h ago');
    expect(formatQueueAge(new Date(now - 4 * 86_400_000).toISOString(), now)).toBe('4d ago');
  });

  it('does not editorialise a multi-day age -- still just "Nd ago", no verdict text', () => {
    const label = formatQueueAge(new Date(now - 30 * 86_400_000).toISOString(), now);
    expect(label).toBe('30d ago');
    expect(label).not.toMatch(/stuck|stale|problem|warning/i);
  });
});

// ── Wanted count -- links to MGUI-14, never duplicates it ────────────────────────────────────

describe('wantedCountLabel', () => {
  it('names zero explicitly rather than a bare "0"', () => {
    expect(wantedCountLabel([])).toBe('Nothing waiting on a release');
  });

  it('reports a real count', () => {
    expect(wantedCountLabel([{}, {}, {}])).toBe('3 waiting on a release');
  });
});

// ── Wiring state -- reused from /api/subsystems, never re-derived ───────────────────────────

describe('wiringDisplay -- the same live/worker/seam/unmounted vocabulary as SubsystemHealth', () => {
  it('maps each known state to a distinct tone', () => {
    expect(wiringDisplay('live').tone).toBe('green');
    expect(wiringDisplay('worker').tone).toBe('blue');
    expect(wiringDisplay('seam').tone).toBe('amber');
    expect(wiringDisplay('unmounted').tone).toBe('neutral');
  });

  it('renders an unrecognised state verbatim + "(unclassified)", never coerced to a known tone', () => {
    const w = wiringDisplay('totally-new-state');
    expect(w.label).toBe('totally-new-state (unclassified)');
    expect(w.known).toBe(false);
  });

  it('renders null (not yet loaded / key missing) as "unknown", never a guessed state', () => {
    const w = wiringDisplay(null);
    expect(w.label).toBe('unknown');
    expect(w.known).toBe(false);
  });
});

describe('emptyQueueReason -- names WHY the queue is empty from subsystem wiring, not a guess', () => {
  it('names the missing download client when acquisition is unmounted', () => {
    expect(emptyQueueReason('unmounted')).toMatch(/download client/);
  });

  it('distinguishes "no download client" (seam) from "unmounted" entirely', () => {
    const seam = emptyQueueReason('seam');
    const unmounted = emptyQueueReason('unmounted');
    expect(seam).not.toBe(unmounted);
  });

  it('reports a neutral "nothing queued" when acquisition is actually wired', () => {
    expect(emptyQueueReason('live')).not.toMatch(/download client/);
    expect(emptyQueueReason('worker')).not.toMatch(/download client/);
  });
});
