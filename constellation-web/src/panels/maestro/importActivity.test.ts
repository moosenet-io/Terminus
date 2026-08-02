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

// Review fix (round 2, codex, confirmed real): the original version of this describe block
// pinned two bugs. (1) It never passed a wanted count, so a non-empty `wanted[]` and this
// empty-queue message rendered side by side, contradicting each other. (2) It asserted the
// function invents a SPECIFIC diagnosis ("download client") the subsystem payload never
// actually asserts -- `seam`/`unmounted` only carry the generic meaning `get_subsystems`'s own
// doc comment defines. Round 3 (codex, confirmed real) caught a third, finer overclaim: "N
// titles monitored with nothing grabbed yet" asserted a history `wanted[]` doesn't carry (see
// `emptyQueueReason`'s doc for the corrected "no file yet" wording). These tests pin the fully
// corrected, honestly-scoped behaviour.
describe('emptyQueueReason -- reports only what wanted[]/subsystem state actually say, never a derived diagnosis', () => {
  it('names "nothing is currently monitored" when wantedCount is 0', () => {
    expect(emptyQueueReason('unmounted', 0)).toMatch(/nothing is currently monitored/);
  });

  it('names the actual monitored COUNT rather than staying silent about it -- the contradiction this fix closes', () => {
    const withWanted = emptyQueueReason('worker', 3);
    expect(withWanted).toMatch(/3 monitored titles have no file yet/);
    expect(withWanted).not.toMatch(/nothing is currently monitored/);
  });

  it('singularises a wanted count of exactly 1', () => {
    expect(emptyQueueReason('worker', 1)).toMatch(/1 monitored title has no file yet/);
  });

  // Review fix (round 3, codex, confirmed real): "nothing grabbed yet" asserted a history
  // `wanted[]` doesn't carry -- a title can be grabbed and complete while still having no file,
  // or have left the queue in a previous run. The message must never claim that.
  it('never claims anything about whether a title was ever grabbed -- only that it has no file', () => {
    const withWanted = emptyQueueReason('worker', 2);
    expect(withWanted).not.toMatch(/grabbed/i);
  });

  it('reports the subsystem state word + its OWN documented meaning, never a specific dependency diagnosis', () => {
    const unmounted = emptyQueueReason('unmounted', 0);
    expect(unmounted).toContain('"unmounted"');
    expect(unmounted).toMatch(/not configured/);
    // The fabricated claim this fix removes -- the payload never says WHICH dependency:
    expect(unmounted).not.toMatch(/Prowlarr/i);
    expect(unmounted).not.toMatch(/download client/i);
  });

  it('seam and unmounted report genuinely different, state-grounded text', () => {
    const seam = emptyQueueReason('seam', 0);
    const unmounted = emptyQueueReason('unmounted', 0);
    expect(seam).not.toBe(unmounted);
    expect(seam).toContain('"seam"');
    expect(seam).toMatch(/not yet producing data/);
  });

  it('an unrecognised state renders verbatim without a fabricated meaning', () => {
    const weird = emptyQueueReason('totally-new-state', 0);
    expect(weird).toContain('"totally-new-state"');
    expect(weird).not.toMatch(/not configured|not yet producing data|wired/);
  });

  it('a null state (not yet loaded) omits the subsystem sentence entirely rather than guessing', () => {
    const noState = emptyQueueReason(null, 0);
    expect(noState).not.toContain('Acquisition reports state');
  });
});
