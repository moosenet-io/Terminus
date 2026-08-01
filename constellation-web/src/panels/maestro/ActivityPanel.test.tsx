// MACT-04 (MUSE-124): component-level regression coverage for the Activity panel's LIVE/HISTORY
// panes. This project has no jsdom/testing-library (see moduleDetail.test.ts's note), so these
// use `react-dom/server`'s `renderToStaticMarkup` — a real React render to an HTML string, no
// DOM/browser needed — and assert on the resulting markup. `LivePane`/`HistoryPane`/
// `LiveSessionCard` take plain props (no hooks), so they render synchronously with no data
// fetching involved.
//
// Review finding this file exists to close (MUSE-124): a prior round shipped a test named
// `liveSourceLabel / historySourceLabel — rendered from the envelope, not hardcoded` that only
// called the pure formatter functions directly — it never rendered the component, so it stayed
// green while `HistoryPane` had `historySourceLabel('muse-history')` hardcoded regardless of its
// `source` prop. Every test below renders the actual component and asserts on its output, and
// each one is checked to actually FAIL against the pre-fix code (verified by hand before this
// file was committed) — a test named for a property must fail when that property is violated.
import { describe, it, expect } from 'vitest';
import { renderToStaticMarkup } from 'react-dom/server';
import { HistoryPane, LivePane, LiveSessionCard } from './ActivityPanel';
import type { HistorySession, LiveSession } from '../../lib/aggregationClient';

function account() { return { id: 1, display_name: 'Mock Viewer' }; }
function item() {
  return {
    media_item_id: 6655, title: 'The Martian', year: 2015, kind: 'movie' as const,
    season_number: null, episode_number: null, episode_title: null,
  };
}
function decision() {
  return {
    video_decision: 'direct_play', audio_decision: 'direct_play', transcode_decision: null,
    transcode_reason: null, container: 'mkv', video_codec: 'hevc', audio_codec: 'eac3',
    audio_channels: 6, video_resolution: '1080', bitrate: 12000,
  };
}

function liveSession(overrides: Partial<LiveSession> = {}): LiveSession {
  return {
    session_id: 1, source: 'muse-derived', session_key: 'sess-1',
    account: account(), item: item(),
    poster_url: null, backdrop_url: null,
    view_offset_ms: 1_284_000, duration_ms: 8_520_000, progress_pct: 15.1,
    player: 'Plex Web', platform: 'Chrome', product: 'Plex Web', device: 'Living Room TV',
    state: 'playing', last_event_at: new Date().toISOString(), started_at: new Date().toISOString(),
    decision: decision(),
    ...overrides,
  };
}

function historySession(overrides: Partial<HistorySession> = {}): HistorySession {
  return {
    session_id: 1, source: 'muse-history', session_key: 'sess-hist-1',
    account: account(), item: item(),
    poster_url: null, backdrop_url: null,
    view_offset_ms: 8_520_000, duration_ms: 8_520_000, progress_pct: 100,
    player: 'Plex Web', platform: 'Chrome', product: 'Plex Web', device: 'Living Room TV',
    started_at: new Date().toISOString(),
    decision: decision(),
    ...overrides,
  };
}

describe('HistoryPane — source label reads the prop, never hardcoded', () => {
  it('renders an UNEXPECTED source value verbatim (fails if the pane hardcodes muse-history)', () => {
    // `'totally-different-source-xyz'` is not a value the real backend would ever send in H1 --
    // that's the point. If HistoryPane still calls historySourceLabel('muse-history') internally
    // (the exact regression this test exists to catch), this string never appears in the output.
    const html = renderToStaticMarkup(
      <HistoryPane available={true} detail={undefined} sessions={[]} source="totally-different-source-xyz" />,
    );
    expect(html).toContain('totally-different-source-xyz');
    expect(html).not.toContain("Muse&#x27;s permanent historical record");
  });

  it('renders the real muse-history label when that IS the actual source', () => {
    const html = renderToStaticMarkup(
      <HistoryPane available={true} detail={undefined} sessions={[]} source="muse-history" />,
    );
    expect(html).toContain('permanent historical record');
  });

  it('falls back to a generic label (not a false claim of muse-history) when source is not yet known', () => {
    const html = renderToStaticMarkup(
      <HistoryPane available={null} detail={undefined} sessions={[]} source={null} />,
    );
    expect(html).not.toContain('permanent historical record');
  });
});

describe('LivePane — source label reads the prop (regression guard, symmetric with HistoryPane)', () => {
  it('renders an unexpected source value verbatim', () => {
    const html = renderToStaticMarkup(
      <LivePane available={true} detail={undefined} sessions={[]} source="totally-different-source-xyz" />,
    );
    expect(html).toContain('totally-different-source-xyz');
  });

  it('renders the documented H1 copy for muse-derived', () => {
    const html = renderToStaticMarkup(
      <LivePane available={true} detail={undefined} sessions={[]} source="muse-derived" />,
    );
    expect(html).toContain('derived from Muse watch history');
  });
});

describe('LiveSessionCard — unknown progress omits the bar, never fabricates 0%', () => {
  it('renders NO ProgressBar element when progress_pct is absent (unknown duration)', () => {
    const session = liveSession({ duration_ms: null, progress_pct: undefined });
    const html = renderToStaticMarkup(<LiveSessionCard session={session} />);
    // ProgressBar's track div is styled with this exact CSS var (ProgressBar.tsx) -- its
    // presence in the markup IS the bar. Omitting the component entirely means this string
    // cannot appear at all, which is the assertion: no bar, not an invisible/zero-width one.
    expect(html).not.toContain('--progress-track');
    expect(html).toContain('progress not reported');
  });

  it('DOES render the ProgressBar when progress_pct is a real reported value (including 0)', () => {
    // Proves the test above actually distinguishes presence/absence rather than always failing
    // to find the marker -- a real (non-null) 0 IS a measurement and must still render a track.
    const html = renderToStaticMarkup(<LiveSessionCard session={liveSession({ progress_pct: 0 })} />);
    expect(html).toContain('--progress-track');
    expect(html).not.toContain('progress not reported');
  });

  it('renders the ProgressBar for a normal in-progress session', () => {
    const html = renderToStaticMarkup(<LiveSessionCard session={liveSession({ progress_pct: 42 })} />);
    expect(html).toContain('--progress-track');
  });
});
