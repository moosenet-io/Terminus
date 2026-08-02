// MACT-07 (MUSE-127): component-level regression coverage for the terminate control. This
// repo has no jsdom/testing-library (see ActivityPanel.test.tsx's module doc), so — same
// convention as every other panel test in this directory — these use `react-dom/server`'s
// `renderToStaticMarkup` and assert on the resulting markup, plus the pure logic in
// `terminateOutcome.ts` (see terminateOutcome.test.ts) for anything that would otherwise need a
// simulated click. `TerminateOutcomeBanner` takes a plain `result` prop (no hooks), so it
// renders synchronously with no data fetching involved, same as `LivePane`/`HistoryPane`.
import { describe, it, expect } from 'vitest';
import { renderToStaticMarkup } from 'react-dom/server';
import { TerminateOutcomeBanner } from './TerminateControl';
import { LiveSessionCard } from './ActivityPanel';
import { AuthRoleProvider } from '../../hooks/AuthRoleContext';
import type { LiveSession, MuseTerminateResult } from '../../lib/aggregationClient';
import { OPERATOR_ROLE_REQUIRED } from './terminateOutcome';

function liveSession(overrides: Partial<LiveSession> = {}): LiveSession {
  return {
    session_id: 1, source: 'muse-derived', session_key: 'sess-1',
    account: { id: 1, display_name: 'Mock Viewer' },
    item: {
      media_item_id: 6655, title: 'The Martian', year: 2015, kind: 'movie',
      season_number: null, episode_number: null, episode_title: null,
    },
    poster_url: null, backdrop_url: null,
    view_offset_ms: 1_284_000, duration_ms: 8_520_000, progress_pct: 15.1,
    player: 'Plex Web', platform: 'Chrome', product: 'Plex Web', device: 'Living Room TV',
    state: 'playing', last_event_at: new Date().toISOString(), started_at: new Date().toISOString(),
    decision: {
      video_decision: 'direct_play', audio_decision: 'direct_play', transcode_decision: null,
      transcode_reason: null, container: 'mkv', video_codec: 'hevc', audio_codec: 'eac3',
      audio_channels: 6, video_resolution: '1080', bitrate: 12000,
    },
    ...overrides,
  };
}

describe('LiveSessionCard — a viewer sees the terminate control disabled', () => {
  it('renders aria-disabled + the operator-role tooltip for a viewer session', () => {
    const html = renderToStaticMarkup(
      <AuthRoleProvider role="viewer">
        <LiveSessionCard session={liveSession()} />
      </AuthRoleProvider>,
    );
    expect(html).toContain('aria-disabled="true"');
    expect(html).toContain(OPERATOR_ROLE_REQUIRED);
  });

  it('renders the control enabled (no aria-disabled wrapper) for an operator session', () => {
    const html = renderToStaticMarkup(
      <AuthRoleProvider role="operator">
        <LiveSessionCard session={liveSession()} />
      </AuthRoleProvider>,
    );
    expect(html).not.toContain('aria-disabled="true"');
    expect(html).toContain('Stop');
  });
});

describe('LiveSessionCard — no bulk terminate-all control exists anywhere in the card', () => {
  it('renders at most one Stop trigger, never an "all" variant', () => {
    const html = renderToStaticMarkup(<LiveSessionCard session={liveSession()} />);
    expect(html.toLowerCase()).not.toContain('stop all');
    expect(html.toLowerCase()).not.toContain('terminate all');
  });
});

describe('TerminateOutcomeBanner — renders the honest outcome, mirrors terminateOutcome.ts', () => {
  it('renders nothing before any attempt', () => {
    expect(renderToStaticMarkup(<TerminateOutcomeBanner result={null} />)).toBe('');
  });

  it('stopped:false renders the did-not-stop message, never a success banner', () => {
    const result: MuseTerminateResult = { kind: 'ok', stopped: false, backend: 'plex', reason_delivered: true };
    const html = renderToStaticMarkup(<TerminateOutcomeBanner result={result} />);
    expect(html.toLowerCase()).toContain('did not stop');
    expect(html).not.toContain('var(--status-success)');
  });

  it('stopped:true renders a success-toned banner', () => {
    const result: MuseTerminateResult = { kind: 'ok', stopped: true, backend: 'plex', reason_delivered: true };
    const html = renderToStaticMarkup(<TerminateOutcomeBanner result={result} />);
    expect(html).toContain('var(--status-success)');
  });

  it('a 403 renders "operator role required", distinguishable from a transport error', () => {
    const forbiddenHtml = renderToStaticMarkup(
      <TerminateOutcomeBanner result={{ kind: 'forbidden', detail: 'forbidden' }} />,
    );
    const errorHtml = renderToStaticMarkup(
      <TerminateOutcomeBanner result={{ kind: 'error', detail: 'network down' }} />,
    );
    expect(forbiddenHtml).toContain(OPERATOR_ROLE_REQUIRED);
    expect(errorHtml).not.toContain(OPERATOR_ROLE_REQUIRED);
    expect(errorHtml).not.toBe(forbiddenHtml);
  });

  it('a 409 conflict renders its own message, not the generic error copy', () => {
    const conflictHtml = renderToStaticMarkup(
      <TerminateOutcomeBanner result={{ kind: 'conflict', detail: 'ambiguous session' }} />,
    );
    expect(conflictHtml.toLowerCase()).toContain('more than one session');
    expect(conflictHtml).not.toContain('Could not reach the stream controller');
  });
});
