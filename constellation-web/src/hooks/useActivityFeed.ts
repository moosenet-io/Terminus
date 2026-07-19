// CONST-26 (§3.3): wires the three feed sources (activity poll, health-transition diff, ws
// event seam) through the pure `lib/activityFeed.ts` merge logic into one live, capped feed.
// Owned by the Shell (`App.tsx`) so there is exactly ONE poll/subscription set for the whole
// app -- both the status-strip bell and the Overview activity-feed widget read from the same
// hook instance rather than each opening their own timer/socket.
import { useCallback, useEffect, useRef, useState } from 'react';
import { getAggregationClient } from '../lib/aggregationClient';
import type { HealthStatus } from '../lib/aggregationClient';
import {
  activityEntryToFeedItem,
  capFeed,
  detectHealthTransitions,
  mergeFeedItems,
  wsEventToFeedItem,
} from '../lib/activityFeed';
import type { FeedItem } from '../lib/activityFeed';

/** Same cadence as the shell's own `/api/health` poll (`App.tsx`'s `fetchHealth` interval) --
 *  the activity feed doesn't need to be more real-time than health already is; the ws seam (c)
 *  is what carries anything more immediate, when it's live. */
const ACTIVITY_POLL_MS = 30_000;

/** Upper bound on how many items this hook ever holds in memory across all three sources
 *  combined -- backs the bell menu's "last 50 in memory" contract with headroom (the bell caps
 *  its OWN display to 50 via `capFeed`; this is the hook's own ceiling so an unusually chatty ws
 *  stream can't grow this unboundedly between health-transition/activity-poll re-renders). */
const MAX_TRACKED = 200;

/**
 * @param health The shell's current `/api/health` snapshot (already grace-window-adjusted by
 *   `App.tsx`'s `applyGrace`) -- this hook diffs consecutive values itself to detect transitions.
 * @param onHealthTransition Optional callback fired once per detected transition (source (b)) --
 *   `App.tsx` wires this to `useToastContext().push` so a health flip ALSO surfaces as a toast,
 *   without this hook needing to know anything about the toast layer itself.
 */
export function useActivityFeed(
  health: HealthStatus[],
  onHealthTransition?: (item: FeedItem) => void,
): FeedItem[] {
  const [activityItems, setActivityItems] = useState<FeedItem[]>([]);
  const [wsItems, setWsItems] = useState<FeedItem[]>([]);
  const [healthItems, setHealthItems] = useState<FeedItem[]>([]);
  const prevHealthRef = useRef<HealthStatus[]>([]);
  const onHealthTransitionRef = useRef(onHealthTransition);
  onHealthTransitionRef.current = onHealthTransition;

  // (a) Poll GET /api/terminus/activity.
  const fetchActivity = useCallback(() => {
    getAggregationClient()
      .terminus.activity(MAX_TRACKED)
      .then(res => setActivityItems(res.entries.map(activityEntryToFeedItem)))
      .catch(() => {
        // Degrade silently -- the feed just doesn't gain new activity entries this cycle
        // (matches the shell's own health-poll-failure posture: never blank the UI over one
        // failed poll).
      });
  }, []);

  useEffect(() => {
    fetchActivity();
  }, [fetchActivity]);

  useEffect(() => {
    const id = setInterval(fetchActivity, ACTIVITY_POLL_MS);
    return () => clearInterval(id);
  }, [fetchActivity]);

  // (c) The CONST-18 ws seam. `ws.connect()` always returns a connection object in both
  // adapters; when no real event stream is configured it just never calls `onEvent` (mock) or
  // sends a typed close + backs off reconnecting (http) -- either way this degrades to "no ws
  // items, ever" with no special-casing needed here.
  useEffect(() => {
    const conn = getAggregationClient().ws.connect({
      onEvent: event => {
        const item = wsEventToFeedItem(event, new Date().toISOString());
        if (item) setWsItems(prev => capFeed([item, ...prev], MAX_TRACKED));
      },
    });
    return () => conn.close();
  }, []);

  // (b) Diff consecutive health snapshots for transitions.
  useEffect(() => {
    const ts = new Date().toISOString();
    const transitions = detectHealthTransitions(prevHealthRef.current, health, ts);
    if (transitions.length > 0) {
      setHealthItems(prev => capFeed([...transitions, ...prev], MAX_TRACKED));
      transitions.forEach(t => onHealthTransitionRef.current?.(t));
    }
    prevHealthRef.current = health;
  }, [health]);

  return capFeed(mergeFeedItems(activityItems, wsItems, healthItems), MAX_TRACKED);
}
