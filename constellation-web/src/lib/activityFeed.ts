// CONST-26: pure, unit-testable merge logic for the Overview activity feed (§3.3). Combines
// three sources into one stable-ordered, deduplicated list of `FeedItem`s:
//   (a) activity entries from GET /api/terminus/activity (the CONST-02 audit-log tail)
//   (b) health transitions observed by the shell's existing 30s /api/health poll
//       (e.g. "chord -> unavailable")
//   (c) a seam for CONST-18 ws events (subscribed if the ws relay is live; silently produces
//       nothing otherwise -- see aggregationClient.ts's `ws.connect`, which degrades to a typed
//       close + reconnect-backoff loop with no events ever emitted when unconfigured)
//
// Every function here is pure (no network, no timers, no DOM) so this stays trivially unit
// testable even though this project has no test runner wired up yet (see this module's sibling
// components for the runtime wiring).

import type { ActivityEntry, HealthStatus, MutationResultEvent } from './aggregationClient';

/** Severity used only for color/log-line-prefix choice (§2.2 "[ok] ..." voice) -- never for
 *  filtering; every source contributes at most `ok`/`warn`/`error`. */
export type FeedLevel = 'ok' | 'warn' | 'error';

export interface FeedItem {
  /** Stable, source-prefixed id -- the dedupe key for `mergeFeedItems`. */
  id: string;
  /** ISO 8601 timestamp used for sort order (most-recent-first in the merged output). */
  ts: string;
  source: 'activity' | 'health' | 'ws' | 'mutation';
  level: FeedLevel;
  system?: string;
  /** Rendered log-line text, e.g. "[ok] POST /api/harmony/engine/restart". */
  text: string;
}

const LEVEL_PREFIX: Record<FeedLevel, string> = { ok: '[ok]', warn: '[warn]', error: '[error]' };

function line(level: FeedLevel, body: string): string {
  return `${LEVEL_PREFIX[level]} ${body}`;
}

/** (a) One `GET /api/terminus/activity` entry -> one feed item. Always `ok` -- the audit log
 *  only ever records that a mutating request REACHED the backend, not its outcome (see
 *  `crate::constellation::activity`'s doc on the Rust side); outcome-level severity comes from
 *  the `mutation` source below instead. */
export function activityEntryToFeedItem(entry: ActivityEntry): FeedItem {
  const who = entry.principal ? ` — ${entry.principal}` : '';
  return {
    // Dedupe id includes system + principal too (review fix): two operators or two
    // systems hitting the same method+path in the same timestamp second are DISTINCT
    // events and must not collapse into one feed item.
    id: `activity:${entry.ts}:${entry.system}:${entry.principal ?? ''}:${entry.method}:${entry.path}`,
    ts: entry.ts,
    source: 'activity',
    level: 'ok',
    system: entry.system,
    text: line('ok', `${entry.method} ${entry.path}${who}`),
  };
}

/** (b) Diff two consecutive `/api/health` snapshots and emit one feed item per system whose
 *  `available` flag flipped. A system with no PRIOR sighting (first poll ever, or a system
 *  that's brand new to the payload) is not a "transition" -- there is nothing to compare
 *  against, so it is skipped, matching the shell's own "everAvailable" bookkeeping in `App.tsx`. */
export function detectHealthTransitions(
  prev: HealthStatus[],
  next: HealthStatus[],
  ts: string,
): FeedItem[] {
  const prevAvailability = new Map(prev.map(h => [h.system, h.available]));
  const items: FeedItem[] = [];
  for (const h of next) {
    const before = prevAvailability.get(h.system);
    if (before === undefined || before === h.available) continue;
    items.push({
      id: `health:${h.system}:${ts}:${h.available ? 'up' : 'down'}`,
      ts,
      source: 'health',
      level: h.available ? 'ok' : 'error',
      system: h.system,
      text: line(h.available ? 'ok' : 'error', `${h.system} -> ${h.available ? 'available' : 'unavailable'}`),
    });
  }
  return items;
}

/** (c) A raw `/ws` event -> a feed item, or `null` if the event doesn't look like anything this
 *  feed should surface (an unrecognized/malformed payload) -- the ws seam degrades SILENTLY per
 *  spec, so "ignore it" rather than "throw" is the correct behavior for anything unexpected.
 *
 *  Per the relay's own contract (README "Real-time relay (`/ws`, CONST-18)"), every event the
 *  browser receives is wrapped as `{source: 'harmony', event: <opaque upstream payload>}` --
 *  `source` is the one thing guaranteed present; the inner `event`'s shape is upstream-owned and
 *  best-effort here (an optional `type`/`id` inside it, when present, makes for a more specific
 *  line/dedupe-id, but neither is required). A flat `{type, ...}` payload with no `source`/
 *  `event` wrapper is also accepted defensively, in case a future/alternate relay ever sends one. */
export function wsEventToFeedItem(event: unknown, ts: string): FeedItem | null {
  if (typeof event !== 'object' || event === null) return null;
  const envelope = event as Record<string, unknown>;

  const source = typeof envelope.source === 'string' ? envelope.source : undefined;
  const inner =
    typeof envelope.event === 'object' && envelope.event !== null
      ? (envelope.event as Record<string, unknown>)
      : envelope;

  const type = typeof inner.type === 'string' ? inner.type : source ? 'event' : null;
  if (!type) return null; // no source AND no recognizable inner type -- nothing usable here.

  const system = source ?? (typeof inner.system === 'string' ? inner.system : undefined);
  const idBasis = typeof inner.id === 'string' ? inner.id : `${system ?? 'ws'}:${type}:${ts}`;

  return {
    id: `ws:${idBasis}`,
    ts,
    source: 'ws',
    level: 'ok',
    system,
    text: line('ok', `${system ? `${system} ` : ''}${type}`),
  };
}

/** A mutation-result event (aggregationClient's `onMutationResult` seam) -> a feed item. Success
 *  is `ok`, failure is `error` -- this is the one source whose level reflects an actual outcome
 *  rather than a bare "it happened". */
export function mutationResultToFeedItem(event: MutationResultEvent, ts: string): FeedItem {
  const label = `${event.method} ${event.path}`;
  return {
    id: `mutation:${event.system}:${event.method}:${event.path}:${ts}`,
    ts,
    source: 'mutation',
    level: event.ok ? 'ok' : 'error',
    system: event.system,
    text: event.ok ? line('ok', label) : line('error', `${label} failed${event.error ? `: ${event.error}` : ''}`),
  };
}

/** Merge any number of feed-item groups into one stable-ordered, deduplicated list:
 *  most-recent-first by `ts`, ties broken by insertion order (later group / later item in a
 *  group wins the dedupe on a colliding `id`, which only happens for two attempts at describing
 *  the exact same event). */
export function mergeFeedItems(...groups: FeedItem[][]): FeedItem[] {
  const byId = new Map<string, FeedItem>();
  for (const group of groups) {
    for (const item of group) {
      byId.set(item.id, item);
    }
  }
  return Array.from(byId.values()).sort((a, b) => {
    if (a.ts === b.ts) return 0;
    return a.ts < b.ts ? 1 : -1;
  });
}

/** Cap a (already-sorted, most-recent-first) feed to at most `max` items -- backs the bell
 *  menu's "last 50 in memory" contract (§3.3; never persisted to browser storage). */
export function capFeed(items: FeedItem[], max = 50): FeedItem[] {
  return items.slice(0, max);
}
