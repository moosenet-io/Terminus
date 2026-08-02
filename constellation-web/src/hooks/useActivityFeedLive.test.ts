// MACT-08 (MUSE-128), review round 2: mutation-proven coverage for the two cadence rules that
// survive after the WS-tick path was dropped (see useActivityFeedLive.ts's own module doc for
// why): each tier polls at its OWN specified interval, and hiding the panel/tab stops polling
// entirely. `refetch` fakes here deliberately match the REAL `MuseSection.refetch` contract
// (`Promise<boolean>`, never rejects — see useMuse.ts) rather than an arbitrary shape, because
// the prior round's backoff test faked a promise-returning `refetch` the production hook never
// actually supplied (`fetchOnce` returned `void`), so the backoff path was proven against a
// contract nothing in the real app had. `useMuse.test.ts` now separately proves
// `useMuseSection`/`useMuseTypedSection` actually resolve `Promise<boolean>`, closing the loop
// between "this hook believes refetch returns a promise" and "the real hooks actually do".
//
// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { act, renderHook } from '@testing-library/react';
import { ACTIVITY_TIER_POLL_MS, useActivityFeedLive } from './useActivityFeedLive';
import type { ActivityFeedTier } from './useActivityFeedLive';

function setHidden(hidden: boolean) {
  Object.defineProperty(document, 'hidden', { configurable: true, get: () => hidden });
  document.dispatchEvent(new Event('visibilitychange'));
}

/** A `refetch` shaped exactly like the real `MuseSection.refetch`: resolves `true`/`false`,
 *  never rejects. */
function realShapedRefetch(outcomes: () => boolean = () => true) {
  return vi.fn((): Promise<boolean> => Promise.resolve(outcomes()));
}

/** Advances fake timers AND flushes the microtask queue -- required whenever `refetch` is
 *  promise-based (the real `MuseSection` contract this hook is written against): the timer
 *  fires synchronously, but `settle()` only runs after the returned promise resolves, which
 *  fake timers alone do not flush. */
async function advanceAndFlush(ms: number) {
  await act(async () => {
    vi.advanceTimersByTime(ms);
    await Promise.resolve();
    await Promise.resolve();
  });
}

beforeEach(() => {
  vi.useFakeTimers();
  setHidden(false);
});

afterEach(() => {
  vi.useRealTimers();
});

describe('useActivityFeedLive — per-tier polling cadence', () => {
  it.each<[ActivityFeedTier, number]>([
    ['live', ACTIVITY_TIER_POLL_MS.live],
    ['tiles', ACTIVITY_TIER_POLL_MS.tiles],
    ['history', ACTIVITY_TIER_POLL_MS.history],
  ])('polls %s at exactly its %ims cadence, not a neighbouring tier\'s', async (tier, intervalMs) => {
    const refetch = realShapedRefetch();
    renderHook(() => useActivityFeedLive(tier, refetch));
    await advanceAndFlush(0);

    // Mount must NOT fetch: the caller's own `useMuseSection` already fetched from its mount
    // effect, so an immediate poll here would double every initial request (ten of them for
    // ActivityTiles' five-source bundle). Round 3 (codex) caught this; the prior version of
    // this test asserted the duplicate as if it were correct.
    expect(refetch).not.toHaveBeenCalled();

    await advanceAndFlush(intervalMs - 1);
    expect(refetch).not.toHaveBeenCalled();
    await advanceAndFlush(1);
    expect(refetch).toHaveBeenCalledTimes(1);

    // A second cycle at the same interval confirms it's the recurring cadence, not a one-off.
    refetch.mockClear();
    await advanceAndFlush(intervalMs);
    expect(refetch).toHaveBeenCalledTimes(1);
  });

  it('renders live=false always — there is no WS/tick path anymore', () => {
    const { result } = renderHook(() => useActivityFeedLive('live', realShapedRefetch()));
    expect(result.current.live).toBe(false);
    act(() => { vi.advanceTimersByTime(ACTIVITY_TIER_POLL_MS.live); });
    expect(result.current.live).toBe(false);
  });
});

describe('useActivityFeedLive — poll-failure backoff (against the real MuseSection contract)', () => {
  it('escalates the poll interval on repeated failures, capped, and resets on success', async () => {
    let shouldFail = true;
    const refetch = realShapedRefetch(() => !shouldFail);
    const { result } = renderHook(() => useActivityFeedLive('live', refetch));

    // The first poll is SCHEDULED, not immediate, so drive one interval to get a failure.
    await advanceAndFlush(ACTIVITY_TIER_POLL_MS.live);
    expect(result.current.pollIntervalMs).toBe(ACTIVITY_TIER_POLL_MS.live * 2);

    // Exact ladder, not a range: 5s -> 10s -> 20s -> 30s (capped), then PINNED at 30s however
    // many further failures arrive. Round 3 (codex) caught the prior version asserting only
    // `<= 30000`, which passes just as happily with the cap deleted entirely -- an assertion
    // that cannot tell capped from uncapped is not evidence of a cap.
    await advanceAndFlush(result.current.pollIntervalMs);
    expect(result.current.pollIntervalMs).toBe(20000);
    await advanceAndFlush(result.current.pollIntervalMs);
    expect(result.current.pollIntervalMs).toBe(30000); // 40000 would exceed the cap
    for (let i = 0; i < 3; i++) {
      await advanceAndFlush(result.current.pollIntervalMs);
      expect(result.current.pollIntervalMs).toBe(30000);
    }

    // Recovery resets to the tier's base cadence.
    shouldFail = false;
    const current = result.current.pollIntervalMs;
    await advanceAndFlush(current);
    expect(result.current.pollIntervalMs).toBe(ACTIVITY_TIER_POLL_MS.live);
  });

  it('never backs off the history tier — its cap equals its own 60s base', async () => {
    // The README previously claimed "all tiers back off up to a 30s cap", which is false here:
    // backoffCapMs is max(30s, base), so history's cap IS its base and repeated failures leave
    // the cadence untouched. Pinned so the doc and the behaviour cannot drift apart again.
    const refetch = realShapedRefetch(() => false);
    const { result } = renderHook(() => useActivityFeedLive('history', refetch));
    for (let i = 0; i < 4; i++) {
      await advanceAndFlush(ACTIVITY_TIER_POLL_MS.history);
      expect(result.current.pollIntervalMs).toBe(ACTIVITY_TIER_POLL_MS.history);
    }
  });

  it('never backs off on a refetch that returns void (a caller that has not opted into outcome reporting)', async () => {
    // The safe-default path: a `refetch` with no return value must never be MISREAD as a
    // failure (that would be a worse regression than never backing off at all).
    const refetch = vi.fn(() => undefined);
    const { result } = renderHook(() => useActivityFeedLive('tiles', refetch));
    await advanceAndFlush(ACTIVITY_TIER_POLL_MS.tiles * 3);
    expect(result.current.pollIntervalMs).toBe(ACTIVITY_TIER_POLL_MS.tiles);
  });
});

describe('useActivityFeedLive — visibility gating', () => {
  it('stops polling entirely when the panel/tab is hidden, and resumes with an immediate refetch on visible', async () => {
    const refetch = realShapedRefetch();
    renderHook(() => useActivityFeedLive('history', refetch));
    await advanceAndFlush(0);
    refetch.mockClear();

    act(() => { setHidden(true); });

    // A very long time passes while hidden — several multiples of every tier's cadence
    // (including 'history's own 60s) — and NOT ONE refetch may fire.
    await advanceAndFlush(10 * ACTIVITY_TIER_POLL_MS.history);
    expect(refetch).not.toHaveBeenCalled();

    // Becoming visible again does an immediate refetch (APPROACH's own "reconnect + immediate
    // refetch on becoming visible" — there is no socket anymore, just the poll loop resuming).
    act(() => { setHidden(false); });
    expect(refetch).toHaveBeenCalledTimes(1);
  });

  it('never leaves a stray timer running after hide (a second hide/show cycle does not double-fire)', async () => {
    const refetch = realShapedRefetch();
    renderHook(() => useActivityFeedLive('tiles', refetch));
    await advanceAndFlush(0);
    refetch.mockClear();

    act(() => { setHidden(true); });
    act(() => { setHidden(false); });
    await advanceAndFlush(0);
    refetch.mockClear(); // drop the resume-refetch; only care about the recurring poll below

    await advanceAndFlush(ACTIVITY_TIER_POLL_MS.tiles);
    expect(refetch).toHaveBeenCalledTimes(1); // exactly one recurring poll fired, not two
  });

  it('ignores a stale in-flight completion that resolves after a hide/show cycle (generation guard, review round 3)', async () => {
    // Sequence this reproduces: pollOnce fires -> request in flight -> tab hides (stop) -> tab
    // shows (start, issuing a NEW poll) -> the OLD request finally settles. Without a
    // generation guard, that stale settle would still see `stopped === false` (the new cycle's
    // value) and overwrite `currentPollMs`/reschedule the timer with a stale result — a
    // pre-hide failure backing off a feed that already recovered.
    const deferreds: Array<(ok: boolean) => void> = [];
    const refetch = vi.fn(
      () => new Promise<boolean>(resolve => { deferreds.push(resolve); }),
    );

    const { result } = renderHook(() => useActivityFeedLive('live', refetch));
    // Mount only SCHEDULES (the section hook does the initial fetch), so advance one interval
    // to get a real poll in flight — this is the request that must later be ignored.
    act(() => { vi.advanceTimersByTime(ACTIVITY_TIER_POLL_MS.live); });
    expect(deferreds).toHaveLength(1); // poll 1 in flight, left UNRESOLVED

    act(() => { setHidden(true); }); // stop() — bumps the generation; poll 1's promise is now stale
    act(() => { setHidden(false); }); // start() — bumps the generation again, issues poll 2 (immediate)
    expect(deferreds).toHaveLength(2);

    // Settle the RECOVERED (current) cycle first, as a clean success.
    deferreds[1](true);
    await advanceAndFlush(0);
    expect(result.current.pollIntervalMs).toBe(ACTIVITY_TIER_POLL_MS.live);

    // NOW the stale pre-hide request finally resolves — as a FAILURE, specifically because a
    // failure is what would corrupt state if it were wrongly let through (a success might
    // coincidentally leave `pollIntervalMs` looking right for the wrong reason).
    deferreds[0](false);
    await advanceAndFlush(0);

    // The recovered cycle's interval must be UNCHANGED by the stale failure.
    expect(result.current.pollIntervalMs).toBe(ACTIVITY_TIER_POLL_MS.live);

    // And only the recovered cycle's own (correct) timer is pending — advancing exactly its
    // interval fires the next poll. If the stale settle had been let through, it would have
    // backed the interval off and rescheduled at 2x, so this exact-interval advance would NOT
    // have fired anything yet.
    refetch.mockClear();
    await advanceAndFlush(ACTIVITY_TIER_POLL_MS.live);
    expect(refetch).toHaveBeenCalledTimes(1);
  });
});
