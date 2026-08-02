// MACT-08 (MUSE-128): mutation-proven coverage for the three cadence rules the spec item
// pins down verbatim: tick coalescing (<=1 refetch/2s), typed-close-frame fallback to the
// tier's polling interval, and "not visible => stop polling entirely". Every test drives the
// hook through a fake `aggregationClient.ws.connect` double -- never a real socket -- so the
// timing assertions are exact and don't depend on a real transport.
//
// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { act, renderHook } from '@testing-library/react';
import {
  ACTIVITY_TICK_COALESCE_MS,
  ACTIVITY_TIER_POLL_MS,
  useActivityFeedLive,
} from './useActivityFeedLive';

type Handlers = { onEvent: (e: unknown) => void; onClose?: () => void; onOpen?: () => void };

let lastHandlers: Handlers | null = null;
let connectSpy: ReturnType<typeof vi.fn>;
let closeSpy: ReturnType<typeof vi.fn>;

vi.mock('../lib/aggregationClient', () => ({
  getAggregationClient: () => ({
    ws: {
      connect: (handlers: Handlers) => {
        lastHandlers = handlers;
        connectSpy(handlers);
        return { send: vi.fn(), close: closeSpy };
      },
    },
  }),
}));

function setHidden(hidden: boolean) {
  Object.defineProperty(document, 'hidden', { configurable: true, get: () => hidden });
  document.dispatchEvent(new Event('visibilitychange'));
}

function sendTick() {
  lastHandlers?.onEvent({ source: 'muse', event: { type: 'activity_tick', ts: Date.now() } });
}

function closeSocket() {
  lastHandlers?.onClose?.();
}

beforeEach(() => {
  vi.useFakeTimers();
  lastHandlers = null;
  connectSpy = vi.fn();
  closeSpy = vi.fn();
  setHidden(false);
});

afterEach(() => {
  vi.useRealTimers();
});

describe('useActivityFeedLive — tick coalescing', () => {
  it('coalesces a burst of ticks to at most one refetch per 2s', () => {
    const refetch = vi.fn();
    renderHook(() => useActivityFeedLive('live', refetch));

    // The initial mount does one immediate poll-mode fetch -- not part of what we're counting.
    refetch.mockClear();

    act(() => {
      sendTick();
      sendTick();
      sendTick();
      sendTick();
    });
    // No refetch yet -- the coalescing window (ACTIVITY_TICK_COALESCE_MS) hasn't elapsed.
    expect(refetch).not.toHaveBeenCalled();

    act(() => { vi.advanceTimersByTime(ACTIVITY_TICK_COALESCE_MS); });
    expect(refetch).toHaveBeenCalledTimes(1);

    // A second burst after the window closes starts a fresh coalescing window.
    act(() => { sendTick(); sendTick(); });
    act(() => { vi.advanceTimersByTime(ACTIVITY_TICK_COALESCE_MS); });
    expect(refetch).toHaveBeenCalledTimes(2);
  });

  it('renders live=true once a tick has actually arrived, not merely on socket connect', () => {
    const refetch = vi.fn();
    const { result } = renderHook(() => useActivityFeedLive('live', refetch));
    expect(result.current.live).toBe(false);

    act(() => { sendTick(); });
    expect(result.current.live).toBe(true);
  });
});

describe('useActivityFeedLive — polling fallback on a typed close frame', () => {
  it('falls back to polling at the tier-specific interval when the socket closes', () => {
    const refetch = vi.fn();
    const { result } = renderHook(() => useActivityFeedLive('tiles', refetch));

    // Prove the live path first (so the fallback is a real transition, not just "never got live").
    act(() => { sendTick(); });
    expect(result.current.live).toBe(true);
    refetch.mockClear();

    // The relay's typed close frames (4000 NO_UPSTREAM / 4001 UPSTREAM_LOST) both surface as
    // a plain `onClose()` call on this client-side handle -- see aggregationClient.ts's
    // `ws.connect`. The hook does not need the code itself, only that the tick source ended.
    act(() => { closeSocket(); });
    expect(result.current.live).toBe(false);
    // enterPolling() does an immediate poll on top of scheduling the recurring one.
    expect(refetch).toHaveBeenCalledTimes(1);
    refetch.mockClear();

    // Next poll fires at exactly the 'tiles' tier's specified cadence (10s), not 'live's (5s).
    act(() => { vi.advanceTimersByTime(ACTIVITY_TIER_POLL_MS.tiles - 1); });
    expect(refetch).not.toHaveBeenCalled();
    act(() => { vi.advanceTimersByTime(1); });
    expect(refetch).toHaveBeenCalledTimes(1);

    expect(result.current.pollIntervalMs).toBe(ACTIVITY_TIER_POLL_MS.tiles);
  });

  it('falls back to polling when the tick source goes silent on an otherwise-open socket', () => {
    // The named edge case from the spec's own TEST PLAN: connected but silent must not read
    // as "live" forever.
    const refetch = vi.fn();
    const { result } = renderHook(() => useActivityFeedLive('live', refetch));
    act(() => { sendTick(); });
    expect(result.current.live).toBe(true);

    // No further ticks arrive; the stale timer (a few multiples of the server's own 3s tick
    // cadence) should demote the hook back to polling without any close event at all.
    act(() => { vi.advanceTimersByTime(9000); });
    expect(result.current.live).toBe(false);
  });

  it('escalates the poll interval on repeated failures, capped, and resets on success', async () => {
    let shouldFail = true;
    const refetch = vi.fn(() => (shouldFail ? Promise.reject(new Error('401')) : Promise.resolve()));
    const { result } = renderHook(() => useActivityFeedLive('live', refetch));

    // Let the initial poll (mount-time) settle as a failure.
    await act(async () => { await Promise.resolve(); await Promise.resolve(); });
    expect(result.current.pollIntervalMs).toBeGreaterThan(ACTIVITY_TIER_POLL_MS.live);

    const afterFirstFailure = result.current.pollIntervalMs;
    act(() => { vi.advanceTimersByTime(afterFirstFailure); });
    await act(async () => { await Promise.resolve(); await Promise.resolve(); });
    expect(result.current.pollIntervalMs).toBeGreaterThanOrEqual(afterFirstFailure);
    expect(result.current.pollIntervalMs).toBeLessThanOrEqual(30000);

    // Recovery resets to the tier's base cadence.
    shouldFail = false;
    const current = result.current.pollIntervalMs;
    act(() => { vi.advanceTimersByTime(current); });
    await act(async () => { await Promise.resolve(); await Promise.resolve(); });
    expect(result.current.pollIntervalMs).toBe(ACTIVITY_TIER_POLL_MS.live);
  });
});

describe('useActivityFeedLive — visibility gating', () => {
  it('stops polling entirely when the panel/tab is hidden, and resumes on visible', () => {
    const refetch = vi.fn();
    renderHook(() => useActivityFeedLive('history', refetch));
    refetch.mockClear();

    act(() => { setHidden(true); });
    expect(closeSpy).toHaveBeenCalled(); // the WS subscription itself is torn down, not just paused

    // A very long time passes while hidden -- covering several multiples of every tier's
    // cadence (including 'history's own 60s) -- and NOT ONE refetch may fire.
    act(() => { vi.advanceTimersByTime(10 * ACTIVITY_TIER_POLL_MS.history); });
    expect(refetch).not.toHaveBeenCalled();

    // Becoming visible again reconnects AND does an immediate refetch (APPROACH's own
    // "reconnect + immediate refetch on becoming visible").
    act(() => { setHidden(false); });
    expect(refetch).toHaveBeenCalledTimes(1);
  });

  it('never leaves a stray timer running after hide (a second hide/show cycle does not double-fire)', () => {
    const refetch = vi.fn();
    renderHook(() => useActivityFeedLive('tiles', refetch));
    refetch.mockClear();

    act(() => { setHidden(true); });
    act(() => { setHidden(false); });
    refetch.mockClear(); // drop the resume-refetch; only care about the recurring poll below

    act(() => { vi.advanceTimersByTime(ACTIVITY_TIER_POLL_MS.tiles); });
    expect(refetch).toHaveBeenCalledTimes(1); // exactly one recurring poll fired, not two
  });
});
