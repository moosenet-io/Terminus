// MACT-07 (MUSE-127), review round 2: a component-level test that actually drives the
// rendered cancel path — this is the load-bearing proof the earlier `shouldIssueTerminateCall`
// pure-function test was NOT (see that function's doc in terminateOutcome.ts for the full
// story of the finding this file exists to close: a mutation test can mutate a helper outside
// the code path it claims to protect, and still fail convincingly).
//
// This is the ONLY file in this directory that needs a real DOM — every other `*.test.tsx`
// here uses `renderToStaticMarkup` because the repo has no jsdom configured project-wide (see
// ActivityPanel.test.tsx's module doc). Rather than switching the whole suite over, this file
// opts into jsdom for itself via the `@vitest-environment` pragma below (Vitest reads that
// per-file, so every other test file keeps running under the default `node` environment
// unchanged — same test count, same speed, elsewhere).
//
// @vitest-environment jsdom
import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { render, screen, cleanup } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { TerminateControl } from './TerminateControl';
import { AuthRoleProvider } from '../../hooks/AuthRoleContext';
import type { LiveSession } from '../../lib/aggregationClient';

const mockTerminate = vi.fn();

// Mocks ONLY `useMuseTerminateSession` (the one hook this component calls) so `terminate()` is
// a spy we can assert on, without standing up the real aggregationClient/httpAdapter/mock
// machinery — this file is about the confirm/cancel wiring, not the network layer (that's
// terminateOutcome.test.ts + aggregationClient.sessions.test.ts's job).
vi.mock('../../hooks/useMuse', () => ({
  useMuseTerminateSession: () => ({ terminate: mockTerminate, inFlight: false }),
}));

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

function renderControl() {
  return render(
    <AuthRoleProvider role="operator">
      <TerminateControl session={liveSession()} />
    </AuthRoleProvider>,
  );
}

async function openDialog(user: ReturnType<typeof userEvent.setup>) {
  await user.click(screen.getByRole('button', { name: 'Stop' }));
  expect(screen.getByRole('dialog')).toBeTruthy();
}

beforeEach(() => {
  mockTerminate.mockReset();
  mockTerminate.mockResolvedValue({ kind: 'ok', stopped: true, backend: 'plex', reason_delivered: true });
});

// This repo has no global test-setup file wiring `@testing-library/react`'s auto-cleanup (the
// rest of the suite uses `renderToStaticMarkup`, which needs none), so this file — the only
// one that renders into a real jsdom document — unmounts explicitly between tests. Without
// this, `screen.getByRole` sees every previous test's un-unmounted tree too and fails with
// "multiple elements found" on the very first query.
afterEach(cleanup);

describe('TerminateControl — cancel never issues the terminate call, on any of its three surfaces', () => {
  it('the Cancel button closes the dialog WITHOUT calling terminate', async () => {
    const user = userEvent.setup();
    renderControl();
    await openDialog(user);

    await user.click(screen.getByRole('button', { name: 'Cancel' }));

    expect(screen.queryByRole('dialog')).toBeNull();
    expect(mockTerminate).not.toHaveBeenCalled();
  });

  it('the Escape key closes the dialog WITHOUT calling terminate', async () => {
    const user = userEvent.setup();
    renderControl();
    await openDialog(user);

    await user.keyboard('{Escape}');

    expect(screen.queryByRole('dialog')).toBeNull();
    expect(mockTerminate).not.toHaveBeenCalled();
  });

  it('a backdrop click closes the dialog WITHOUT calling terminate', async () => {
    const user = userEvent.setup();
    renderControl();
    await openDialog(user);

    // ConfirmDialog's backdrop is the `role="presentation"` element wrapping `role="dialog"`
    // (components/ConfirmDialog.tsx) — its own onClick IS onCancel, and the dialog panel itself
    // stops propagation so a click inside the panel never reaches it. Click the backdrop
    // directly (the parent), not the dialog panel.
    const dialog = screen.getByRole('dialog');
    const backdrop = dialog.parentElement;
    expect(backdrop).not.toBeNull();
    await user.click(backdrop as HTMLElement);

    expect(screen.queryByRole('dialog')).toBeNull();
    expect(mockTerminate).not.toHaveBeenCalled();
  });

  // Contrast case — proves the mock and the render actually work end to end, so the three
  // "not called" assertions above are meaningful rather than trivially true because nothing in
  // this test setup could ever call `terminate` at all.
  it('confirming DOES call terminate exactly once', async () => {
    const user = userEvent.setup();
    renderControl();
    await openDialog(user);

    await user.click(screen.getByRole('button', { name: 'Stop stream' }));

    expect(mockTerminate).toHaveBeenCalledTimes(1);
    expect(mockTerminate).toHaveBeenCalledWith('sess-1', undefined);
  });
});
