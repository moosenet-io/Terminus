// MACT-08 (MUSE-128): the Muse activity panel's live-vs-polling cadence controller.
//
// `Terminus/src/constellation/ws.rs` fans in a lightweight CHANGE SIGNAL --
// `{source:'muse', event:{type:'activity_tick', ts}}` -- on the SAME `/ws` socket every other
// event already rides (see that module's doc comment; there is exactly one WebSocket client in
// this app, `aggregationClient.ts`'s `ws.connect`, and this hook goes through it like every
// other consumer). This hook is the client half of that seam: while ticks are flowing, it
// coalesces them into refetches through the caller's own data hook; the moment they stop (a
// typed close frame, a network drop, or the fan-in source going silent on an otherwise-open
// socket), it falls back to polling at the cadence the epic specifies, and it stops entirely
// while the tab/route is not visible.
//
// REJECTED: pushing the Muse payload itself over the socket. The payload is credential-gated
// per system and already passes through `proxy_muse` + `mask_response` on `/api/*` -- doing it
// again here would duplicate that proxy's auth and masking on a second path (see ws.rs's own
// doc and this item's spec brief). A tick only ever says "go refetch"; the actual fetch still
// goes through `aggregationClient` exactly as it does today, so `aggregationClient.ts` remains
// the ONLY fetch site in this app.
//
// REJECTED: treating "socket open" as "live". A silent-but-open socket (the fan-in source
// stalled, or a pre-MACT-08 server) would then render "live" while the data is actually
// frozen -- exactly the failure this item's own TEST PLAN names. `live` here is earned by
// ACTUALLY RECEIVING ticks, and lapses back to `false` (LIVE_STALE_AFTER_MS below) the moment
// they stop arriving, whether or not the transport itself is still connected.
import { useEffect, useRef, useState } from 'react';
import { getAggregationClient } from '../lib/aggregationClient';
import type { WsConnection } from '../lib/aggregationClient';
import type { WsEnvelope } from '../types/events';

export type ActivityFeedTier = 'live' | 'tiles' | 'history';

/** Base poll cadence per tier when the WS tick source is not live -- the exact numbers the
 *  spec item specifies ("WS unavailable: live pane every 5s, stat tiles every 10s, history
 *  every 60s"). Exported so tests (and, if ever needed, a panel wanting to preflight a label)
 *  reference the same numbers rather than duplicating them. */
export const ACTIVITY_TIER_POLL_MS: Record<ActivityFeedTier, number> = {
  live: 5000,
  tiles: 10000,
  history: 60000,
};

/** "Coalesced to at most once per 2s" (spec, verbatim) -- a trailing-edge coalesce: however
 *  many ticks arrive inside one 2s window, at most one refetch fires, 2s after the window's
 *  first tick. The server's own tick cadence (`ws.rs`'s `ACTIVITY_TICK_INTERVAL`, 3s) already
 *  keeps this from mattering in the common case; this is the independent client-side
 *  guarantee that holds even under a reconnect burst or a tightened server interval. */
export const ACTIVITY_TICK_COALESCE_MS = 2000;

/** How long a socket may go without a fresh tick before this hook stops calling itself "live"
 *  and falls back to polling -- a small multiple of the server's tick interval so a couple of
 *  delayed/skipped ticks (`MissedTickBehavior::Delay` on a briefly slow send) don't flap the
 *  mode, while genuine silence (a stalled fan-in source on an otherwise-open socket) still
 *  degrades honestly within one visible cadence interval. */
const LIVE_STALE_AFTER_MS = 9000;

/** Backoff ceiling for consecutive poll failures (a 401 loop must not hammer the proxy).
 *  Doubles from the tier's own base on each failure, capped at the LARGER of 30s or the
 *  tier's own base -- `history`'s 60s base is already coarser than a 30s cap would be, so
 *  there's nothing to cap it down to; `live`/`tiles` (5s/10s bases) cap at 30s exactly as the
 *  spec's "5s -> 10s -> 30s cap" ladder describes. */
function backoffCapMs(tier: ActivityFeedTier): number {
  return Math.max(30000, ACTIVITY_TIER_POLL_MS[tier]);
}

export interface ActivityFeedLiveState {
  /** True only while `activity_tick` events are actively arriving -- never true merely because
   *  the socket is open. This is what a panel should render as "live" vs "polling every Ns";
   *  never derive that label from connection state alone. */
  live: boolean;
  /** The interval currently governing the polling fallback (post-backoff). Only meaningful
   *  while `live` is false; a panel wanting a "polling every Ns" label reads this. */
  pollIntervalMs: number;
  /** ms epoch of the last time this hook actually invoked `refetch`, or null before the first
   *  call -- lets a panel dim/flag a reading that's gone stale relative to its own cadence. */
  lastUpdatedAt: number | null;
}

/**
 * Drives `refetch` on the cadence MACT-08 specifies: WS-tick-coalesced while a live Muse
 * activity signal is flowing, polling at `tier`'s own interval (with failure backoff)
 * otherwise, and NOTHING AT ALL while `document.visibilityState` is hidden.
 *
 * This hook never owns the fetched data itself -- callers keep using their existing
 * `useMuse*`-style hook for `{data, degraded}` and pass THAT hook's `refetch` in here, per the
 * spec's "the client... add `activity_tick` handling that refetches through the client"
 * instruction. `refetch` may return a Promise; a rejection escalates the poll backoff, a
 * resolution (or a plain `void` return) resets it to the tier's base.
 */
export function useActivityFeedLive(tier: ActivityFeedTier, refetch: () => unknown): ActivityFeedLiveState {
  const refetchRef = useRef(refetch);
  refetchRef.current = refetch;

  const [live, setLive] = useState(false);
  const [pollIntervalMs, setPollIntervalMs] = useState(ACTIVITY_TIER_POLL_MS[tier]);
  const [lastUpdatedAt, setLastUpdatedAt] = useState<number | null>(null);

  useEffect(() => {
    // `stopped`/`liveLocal` are plain closure variables, not state reads -- this effect runs
    // ONCE per `tier` (deps below), so a `live`/state value captured at effect-start would go
    // stale the instant `setLive` fires; every internal branch below reads these locals
    // instead of the React state the hook returns.
    let stopped = true;
    let liveLocal = false;
    let currentPollMs = ACTIVITY_TIER_POLL_MS[tier];
    let conn: WsConnection | null = null;
    let pollTimer: ReturnType<typeof setTimeout> | null = null;
    let coalesceTimer: ReturnType<typeof setTimeout> | null = null;
    let staleTimer: ReturnType<typeof setTimeout> | null = null;
    let pendingTick = false;

    function doRefetch() {
      if (stopped) return;
      setLastUpdatedAt(Date.now());
      refetchRef.current();
    }

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
        maybePromise.then(() => settle(true), () => settle(false));
      } else {
        settle(true);
      }
    }

    function enterPolling() {
      if (stopped) return;
      liveLocal = false;
      setLive(false);
      if (staleTimer) { clearTimeout(staleTimer); staleTimer = null; }
      // A tick may have started coalescing (pendingTick) right before the source went silent
      // or the socket closed -- drop that pending coalesce so it can't fire a stray refetch
      // mid-poll-cycle once we're back on the polling cadence (review-caught: without this,
      // a tick immediately followed by a close leaked a refetch at the coalesce window's
      // delay, ahead of the tier's actual poll interval).
      if (coalesceTimer) { clearTimeout(coalesceTimer); coalesceTimer = null; }
      pendingTick = false;
      currentPollMs = ACTIVITY_TIER_POLL_MS[tier];
      setPollIntervalMs(currentPollMs);
      // Immediate poll rather than waiting out a full interval -- matches APPROACH's
      // "reconnect + immediate refetch" discipline for the visibility-resume case, and avoids
      // sitting on data that just went stale for LIVE_STALE_AFTER_MS on the silent-source case.
      pollOnce();
    }

    function armStaleTimer() {
      if (staleTimer) clearTimeout(staleTimer);
      staleTimer = setTimeout(enterPolling, LIVE_STALE_AFTER_MS);
    }

    function onTick() {
      if (stopped) return;
      armStaleTimer();
      if (!liveLocal) {
        liveLocal = true;
        setLive(true);
      }
      // Live now proven -- the poll loop's job is done until/unless ticks stop again.
      if (pollTimer) { clearTimeout(pollTimer); pollTimer = null; }
      if (pendingTick) return; // already coalescing this window
      pendingTick = true;
      coalesceTimer = setTimeout(() => {
        pendingTick = false;
        doRefetch();
      }, ACTIVITY_TICK_COALESCE_MS);
    }

    function start() {
      stopped = false;
      liveLocal = false;
      currentPollMs = ACTIVITY_TIER_POLL_MS[tier];
      setLive(false);
      setPollIntervalMs(currentPollMs);
      // Poll immediately AND connect the socket in parallel -- no gap between mount and the
      // first data while the WS handshake is in flight. `onTick` cancels the poll loop the
      // moment a real tick proves the live source is flowing; until then polling covers it.
      pollOnce();
      conn = getAggregationClient().ws.connect({
        onEvent: (raw) => {
          const envelope = raw as WsEnvelope;
          if (envelope?.source === 'muse' && envelope.event?.type === 'activity_tick') {
            onTick();
          }
        },
        // Any close -- including the relay's typed `4000 NO_UPSTREAM`/`4001 UPSTREAM_LOST`
        // frames -- is the documented fallback trigger. This hook doesn't need to branch on
        // the close CODE itself: the aggregationClient's own reconnect-with-backoff already
        // retries the transport, and if/when it succeeds, fresh ticks re-arm `live` via
        // `onTick` above with no extra plumbing needed here.
        onClose: () => enterPolling(),
      });
    }

    function stop() {
      stopped = true;
      conn?.close();
      conn = null;
      if (pollTimer) { clearTimeout(pollTimer); pollTimer = null; }
      if (coalesceTimer) { clearTimeout(coalesceTimer); coalesceTimer = null; }
      if (staleTimer) { clearTimeout(staleTimer); staleTimer = null; }
      pendingTick = false;
      liveLocal = false;
      setLive(false);
    }

    function handleVisibilityChange() {
      if (typeof document === 'undefined') return;
      if (document.hidden) {
        // "Panel not visible: stop polling ENTIRELY" -- this tears down the WS subscription
        // AND every timer, not merely pausing the interval. A panel left open overnight in a
        // background tab must not poll or hold a live socket at all.
        stop();
      } else {
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

  return { live, pollIntervalMs, lastUpdatedAt };
}
