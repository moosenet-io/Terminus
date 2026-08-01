// MACT-04 (MUSE-124): coverage for the Maestro Activity panel's pure helpers. Every claim
// here is one that would be VIOLATED if the corresponding line of nowPlaying.ts were deleted or
// reverted to a naive implementation — see CLAUDE.md's "a test that would pass with the feature
// removed is worse than no test."
import { describe, it, expect } from 'vitest';
import {
  accountLabel,
  classifyDecision,
  degradeCause,
  distinctBy,
  formatMs,
  historySourceLabel,
  isItemResolved,
  itemTitle,
  liveSourceLabel,
  progressInfo,
  statePillLabel,
  statePillState,
} from './nowPlaying';
import type { SessionAccount, SessionDecision, SessionItem } from '../../lib/aggregationClient';

function decision(overrides: Partial<SessionDecision> = {}): SessionDecision {
  return {
    video_decision: null, audio_decision: null, transcode_decision: null, transcode_reason: null,
    container: null, video_codec: null, audio_codec: null, audio_channels: null,
    video_resolution: null, bitrate: null,
    ...overrides,
  };
}

function item(overrides: Partial<SessionItem> = {}): SessionItem {
  return {
    media_item_id: null, title: null, year: null, kind: null,
    season_number: null, episode_number: null, episode_title: null,
    ...overrides,
  };
}

describe('formatMs', () => {
  it('renders null/undefined as an em dash, NEVER as 0:00', () => {
    expect(formatMs(null)).toBe('—');
    expect(formatMs(undefined)).toBe('—');
  });

  it('formats sub-hour durations as m:ss', () => {
    expect(formatMs(0)).toBe('0:00');
    expect(formatMs(84_000)).toBe('1:24');
  });

  it('formats hour-plus durations as h:mm:ss', () => {
    expect(formatMs(8_520_000)).toBe('2:22:00');
  });
});

describe('progressInfo — progress is NOT double-scaled', () => {
  it('passes an already-0..100-scaled progress_pct straight through, unmodified', () => {
    // MACT-01 says progress_pct arrives PRE-SCALED to 0..100. A regression that re-applies
    // *100 or /100 here would silently corrupt every progress bar in the panel.
    expect(progressInfo(1_284_000, 8_520_000, 15.1).pct).toBe(15.1);
    expect(progressInfo(0, 100, 0).pct).toBe(0);
    expect(progressInfo(100, 100, 100).pct).toBe(100);
  });

  it('reports pct as null (not 0) when progress_pct was never sent (unknown duration)', () => {
    // Rust OMITS the key entirely (skip_serializing_if) — the client passes `undefined` through
    // here, and this function must turn that into `null`, never a fabricated `0`.
    const info = progressInfo(900_000, null, undefined);
    expect(info.pct).toBeNull();
    expect(info.durationLabel).toBe('—');
  });

  it('builds a combined position/duration label from formatMs', () => {
    expect(progressInfo(84_000, 8_520_000, 1).combinedLabel).toBe('1:24 / 2:22:00');
  });
});

describe('classifyDecision', () => {
  it('classifies matched direct_play/direct_play as Direct play', () => {
    const d = classifyDecision(decision({ video_decision: 'direct_play', audio_decision: 'direct_play' }));
    expect(d.kind).toBe('direct_play');
    expect(d.label).toBe('Direct play');
  });

  it('classifies a direct_play/direct_stream mix as Remux, not Direct play', () => {
    const d = classifyDecision(decision({ video_decision: 'direct_play', audio_decision: 'direct_stream' }));
    expect(d.kind).toBe('remux');
    expect(d.label).toBe('Remux');
  });

  it('classifies any transcode side as Transcode, even if the other side is direct_play', () => {
    const d = classifyDecision(decision({ video_decision: 'transcode', audio_decision: 'direct_play' }));
    expect(d.kind).toBe('transcode');
    expect(d.label).toBe('Transcode');
  });

  it('renders an unrecognised decision value verbatim + "(unclassified)", never as Direct play', () => {
    const d = classifyDecision(decision({ video_decision: 'quantum_leap', audio_decision: 'direct_play' }));
    expect(d.kind).toBe('unclassified');
    expect(d.label).toBe('quantum_leap (unclassified)');
    expect(d.label).not.toContain('Direct play');
  });

  it('carries transcode_reason through as the badge tooltip', () => {
    const d = classifyDecision(decision({
      video_decision: 'transcode', audio_decision: 'transcode',
      transcode_reason: 'bitrate exceeds the network ceiling',
    }));
    expect(d.tooltip).toBe('bitrate exceeds the network ceiling');
  });

  it('treats both-null decisions as unclassified, never as Direct play', () => {
    const d = classifyDecision(decision());
    expect(d.kind).toBe('unclassified');
  });
});

describe('statePillState / statePillLabel — stale is its own state', () => {
  it('maps playing/paused/stale to three distinct pill states', () => {
    expect(statePillState('playing')).toBe('online');
    expect(statePillState('paused')).toBe('idle');
    expect(statePillState('stale')).toBe('warm');
    // Load-bearing: stale must not collapse onto paused's pill state.
    expect(statePillState('stale')).not.toBe(statePillState('paused'));
  });

  it('gives stale its own distinct label from paused', () => {
    expect(statePillLabel('stale')).not.toBe(statePillLabel('paused'));
    expect(statePillLabel('stale')).toBe('Stale');
  });
});

describe('liveSourceLabel / historySourceLabel — rendered from the envelope, not hardcoded', () => {
  it('renders the H1 muse-derived source with its documented copy', () => {
    expect(liveSourceLabel('muse-derived')).toBe('live view derived from Muse watch history');
  });

  it('renders the H2 maestro-live source distinctly, proving the flip is visible', () => {
    const h1 = liveSourceLabel('muse-derived');
    const h2 = liveSourceLabel('maestro-live');
    expect(h2).not.toBe(h1);
    expect(h2).toBe('Maestro live sessions');
  });

  it('renders an unrecognised source verbatim rather than silently picking one of the two known labels', () => {
    const label = liveSourceLabel('something-new');
    expect(label).toContain('something-new');
    expect(label).not.toBe('live view derived from Muse watch history');
    expect(label).not.toBe('Maestro live sessions');
  });

  it('history source is permanently muse-history, distinct copy from live', () => {
    expect(historySourceLabel('muse-history')).toBe("Muse's permanent historical record");
  });
});

describe('degradeCause — names the actual cause, never a bare "unavailable"', () => {
  it('names CONSTELLATION_MUSE_TOKEN specifically on a 401 (TERM-549)', () => {
    const cause = degradeCause('HTTP 401 for /api/muse/api/sessions/live');
    expect(cause).toContain('CONSTELLATION_MUSE_TOKEN');
    expect(cause).toContain('549');
  });

  it('names "not wired yet" on a 404/501', () => {
    expect(degradeCause('HTTP 404 for /api/muse/api/sessions/live')).toMatch(/wired/i);
    expect(degradeCause('HTTP 501 for /api/muse/api/sessions/live')).toMatch(/wired/i);
  });

  it('passes an unrecognised detail through rather than swallowing it', () => {
    expect(degradeCause('network error: ECONNREFUSED')).toBe('network error: ECONNREFUSED');
  });

  it('never returns a bare "unavailable" with zero explanation when detail is missing', () => {
    expect(degradeCause(undefined)).not.toBe('unavailable');
    expect(degradeCause(undefined).length).toBeGreaterThan('unavailable'.length);
  });
});

describe('accountLabel — a Muse account label, never the shell user', () => {
  it('prefers display_name', () => {
    const a: SessionAccount = { id: 5, display_name: 'Mock Viewer' };
    expect(accountLabel(a)).toBe('Mock Viewer');
  });

  it('falls back to Account #id when display_name is null', () => {
    const a: SessionAccount = { id: 5, display_name: null };
    expect(accountLabel(a)).toBe('Account #5');
  });

  it('falls back to an explicit unknown label when both are null, never blank', () => {
    const a: SessionAccount = { id: null, display_name: null };
    expect(accountLabel(a)).toBe('Unknown account');
  });
});

describe('itemTitle / isItemResolved — an unresolved item renders as such, never blank', () => {
  it('marks a session with no resolved item as unresolved', () => {
    const i = item();
    expect(isItemResolved(i)).toBe(false);
    expect(itemTitle(i)).toBe('Unresolved item');
  });

  it('marks a fully resolved item as resolved', () => {
    const i = item({ media_item_id: 6655, title: 'The Martian', year: 2015, kind: 'movie' });
    expect(isItemResolved(i)).toBe(true);
    expect(itemTitle(i)).toBe('The Martian (2015)');
  });

  it('renders season/episode + episode title for a show', () => {
    const i = item({
      media_item_id: 7001, title: 'Example Series', kind: 'show',
      season_number: 1, episode_number: 4, episode_title: 'Example Episode Title',
    });
    expect(itemTitle(i)).toBe('Example Series — S01E04 · Example Episode Title');
  });
});

describe('distinctBy — filter chip options', () => {
  it('returns sorted, deduped, non-null values', () => {
    const rows = [{ v: 'b' }, { v: 'a' }, { v: 'b' }, { v: null }, { v: '' }];
    expect(distinctBy(rows, r => r.v)).toEqual(['a', 'b']);
  });

  it('returns an empty array when nothing is present, never throwing', () => {
    expect(distinctBy([], (r: { v: string }) => r.v)).toEqual([]);
  });
});
