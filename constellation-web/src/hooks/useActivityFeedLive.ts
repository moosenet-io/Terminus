// MACT-08 (MUSE-128): the Muse activity panel's polling cadence controller.
//
// ORIGINAL PLAN (rejected in review round 2, recorded here and in
// `Terminus/src/constellation/ws.rs`'s own "MACT-08 evaluated..." doc section): fan a Muse
// `activity_tick` change-signal over the existing `/ws` relay and treat its arrival as "live".
// That was rejected because the tick would have been a CLOCK running inside the relay's
// `pipe()` loop, never actually observing Muse -- receiving it would prove only that the
// relay's OWN socket was alive, nothing about Muse being reachable or having changed. Worse,
// the clock's ~3s cadence was TIGHTER than every one of the panel's own specified polling
// cadences (live 5s / tiles 10s / history 60s), so wiring it would have INCREASED backend load
// (five Muse requests per tick for the stat-tile row alone) while adding no information, on top
// of a spurious dependency on Harmony's upstream WS leg for a feature that has nothing to do
// with Harmony. `ws.rs` has NO functional change for MACT-08 -- only a doc comment recording
// this finding so the next person inherits it rather than rediscovering it.
//
// What ships instead: DIRECT per-tier polling, no WebSocket involved anywhere in this file --
// `aggregationClient.ts`'s `ws.connect` and the `/ws` relay remain exactly as untouched as
// `ws.rs` is. If Muse ever grows a real outbound event source, the `source` envelope seam
// `ws.rs` documents is still there, unmodified, as the right extension point -- this hook's
// cadence/backoff/visibility-gating logic would still apply, just triggered by a real signal
// instead of a poll timer, and would at that point be a genuine improvement over polling rather
// than a relabelled poll dressed up as "live" over an extra transport.
import { useEffect, useRef, useState } from 'react';

export type ActivityFeedTier = 'live' | 'tiles' | 'history';

/** Poll cadence per tier -- the exact numbers the spec item specifies ("WS unavailable: live
 *  pane every 5s, stat tiles every 10s, history every 60s"). Since there is no WS path at all
 *  now, this is simply THE cadence, not a fallback from something else. Exported so tests (and
 *  a panel wanting to preflight a label) reference the same numbers rather than duplicating
 *  them. */
export const ACTIVITY_TIER_POLL_MS: Record<ActivityFeedTier, number> = {
  live: 5000,
  tiles: 10000,
  history: 60000,
};

/** Backoff ceiling for consecutive poll failures (a 401 loop must not hammer the proxy).
 *  Doubles from the tier's own base on each failure, capped at the LARGER of 30s or the
 *  tier's own base -- `history`'s 60s base is already coarser than a 30s cap would be, so
 *  there's nothing to cap it down to; `live`/`tiles` (5s/10s bases) cap at 30s exactly as the
 *  spec's "5s -> 10s -> 30s cap" ladder describes. */
function backoffCapMs(tier: ActivityFeedTier): number {
  return Math.max(30000, ACTIVITY_TIER_POLL_MS[tier]);
}

export interface ActivityFeedLiveState {
  /** Always `false` -- there is no WS/tick "live" path (see this file's module doc for why).
   *  Kept in the return shape rather than deleted so `feedModeLabel` and callers have a stable
   *  contract if a genuine live source is ever wired in later; today it only ever renders
   *  "polling every Ns", which is the honest state of the world. */
  live: boolean;
  /** The interval currently governing the polling loop (post-backoff). */
  pollIntervalMs: number;
  /** ms epoch of the last time this hook actually invoked `refetch`, or null before the first
   *  call -- lets a panel dim/flag a reading that's gone stale relative to its own cadence. */
  lastUpdatedAt: number | null;
}

/**
 * Drives `refetch` on a fixed per-tier polling cadence, with backoff on repeated failures, and
 * stops ENTIRELY (no timer of any kind) while `document.visibilityState` is hidden.
 *
 * `refetch` SHOULD return a `Promise<boolean>` (`true` = success, `false` = degraded) so the
 * backoff ladder can react to a real outcome -- see `useMuse.ts`'s `MuseSection.refetch` for
 * the house contract this is written against (MUSE-128 review round 2: an earlier version of
 * `MuseSection.refetch` returned `void` in production while this hook's own test faked a
 * promise-returning `refetch` that the real function never actually supplied, so the backoff
 * path was tested against a contract the real code didn't have and could never engage in
 * production -- fixed in `useMuse.ts` alongside this file). A `refetch` that returns
 * `void`/`undefined` is still accepted, treated as an unconditional success (the interval never
 * backs off for it) -- a strictly safe default for a caller that hasn't opted into outcome
 * reporting, never a silent failure to detect failure.
 */
export function useActivityFeedLive(tier: ActivityFeedTier, refetch: () => unknown): ActivityFeedLiveState {
  const refetchRef = useRef(refetch);
  refetchRef.current = refetch;

  const [pollIntervalMs, setPollIntervalMs] = useState(ACTIVITY_TIER_POLL_MS[tier]);
  const [lastUpdatedAt, setLastUpdatedAt] = useState<number | null>(null);

  useEffect(() => {
    let stopped = true;
    let currentPollMs = ACTIVITY_TIER_POLL_MS[tier];
    let pollTimer: ReturnType<typeof setTimeout> | null = null;

    function schedulePoll() {
      if (stopped) return;
      if (pollTimer) clearTimeout(pollTimer);
      pollTimer = setTimeout(pollOnce, currentPollMs);
    }

    function pollOnce() {
      if (stopped) return;
      setLastUpdatedAt(Date.now());
      const result = refetchRef.current();
      const settle = (ok: boolean) => {
        if (stopped) return;
        currentPollMs = ok ? ACTIVITY_TIER_POLL_MS[tier] : Math.min(currentPollMs * 2, backoffCapMs(tier));
        setPollIntervalMs(currentPollMs);
        schedulePoll();
      };
      const maybePromise = result as Promise<unknown> | null | undefined;
      if (maybePromise && typeof maybePromise.then === 'function') {
        // A resolved value of exactly `false` is the only "failure" signal a well-behaved
        // `refetch` should ever produce (`MuseSection`'s contract never rejects) -- but a
        // rejection is still handled defensively for any other caller's `refetch`.
        maybePromise.then(val => settle(val !== false), () => settle(false));
      } else {
        settle(true);
      }
    }

    function start() {
      stopped = false;
      currentPollMs = ACTIVITY_TIER_POLL_MS[tier];
      setPollIntervalMs(currentPollMs);
      pollOnce();
    }

    function stop() {
      stopped = true;
      if (pollTimer) { clearTimeout(pollTimer); pollTimer = null; }
    }

    function handleVisibilityChange() {
      if (typeof document === 'undefined') return;
      if (document.hidden) {
        // "Panel not visible: stop polling ENTIRELY" -- no timer of any kind while hidden, not
        // merely a paused interval. A panel left open overnight in a background tab must not
        // poll at all.
        stop();
      } else {
        // "Reconnect + immediate refetch on becoming visible" (APPROACH) -- there is no socket
        // to reconnect anymore, so `start()`'s immediate `pollOnce()` covers the whole of it.
        start();
      }
    }

    const initiallyHidden = typeof document !== 'undefined' && document.hidden;
    if (!initiallyHidden) start();
    if (typeof document !== 'undefined') {
      document.addEventListener('visibilitychange', handleVisibilityChange);
    }

    return () => {
      stop();
      if (typeof document !== 'undefined') {
        document.removeEventListener('visibilitychange', handleVisibilityChange);
      }
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [tier]);

  return { live: false, pollIntervalMs, lastUpdatedAt };
}
