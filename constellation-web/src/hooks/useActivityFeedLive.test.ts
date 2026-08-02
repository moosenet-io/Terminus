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
    await advanceAndFlush(0); // flush the mount-time immediate fetch's promise

    // The mount itself does one immediate fetch — not part of what's being timed below.
    expect(refetch).toHaveBeenCalledTimes(1);
    refetch.mockClear();

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

    // Let the initial mount-time poll settle as a failure.
    await advanceAndFlush(0);
    expect(result.current.pollIntervalMs).toBeGreaterThan(ACTIVITY_TIER_POLL_MS.live);

    const afterFirstFailure = result.current.pollIntervalMs;
    await advanceAndFlush(afterFirstFailure);
    expect(result.current.pollIntervalMs).toBeGreaterThanOrEqual(afterFirstFailure);
    expect(result.current.pollIntervalMs).toBeLessThanOrEqual(30000);

    // Recovery resets to the tier's base cadence.
    shouldFail = false;
    const current = result.current.pollIntervalMs;
    await advanceAndFlush(current);
    expect(result.current.pollIntervalMs).toBe(ACTIVITY_TIER_POLL_MS.live);
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
});
