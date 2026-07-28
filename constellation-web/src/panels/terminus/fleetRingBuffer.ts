// CONST-28: pure, client-held ring buffer backing FleetPanel's uptime sparklines.
// Deliberately framework-free (no React) so it composes with any hook and is directly
// unit-testable — FleetPanel.tsx only ever calls `pushHealthPoll`/`transitions`.
import type { HealthStatus, SystemId } from '../../lib/aggregationClient';

/** One retained sample: a system's availability at one `/api/health` poll. */
export interface HealthPoint {
  /** Poll timestamp (ms since epoch) — NOT the server's clock, the client's poll time. */
  t: number;
  available: boolean;
}

/** Fixed retention: the last 120 polls per system (spec: "client-held ring buffer of the
 *  last 120 polls"). At a 5s poll interval this is a 10-minute rolling window. */
export const RING_BUFFER_CAPACITY = 120;

/** One ring buffer per system, keyed by `SystemId`. Plain object (not a Map) so it can be
 *  handed to `useState` and compared/cloned trivially. */
export type FleetRingBuffers = Partial<Record<SystemId, HealthPoint[]>>;

/** A detected state change for one system between the previous and current poll. */
export interface Transition {
  system: SystemId;
  from: boolean | null;
  to: boolean;
  t: number;
}

/** Push one poll's `/api/health` snapshot into `buffers`, immutably: returns a NEW
 *  `FleetRingBuffers` (never mutates the input) with each system's array capped at
 *  `RING_BUFFER_CAPACITY` (oldest samples drop off the front once full — true ring-buffer
 *  behavior via a bounded array shift, not an unbounded push). A system absent from `health`
 *  (a wholesale poll failure, or the payload just not including it) is left untouched — its
 *  buffer keeps its last-known content rather than being cleared (spec edge case: "health
 *  poll failing (keep last-known ring buffer content)"). */
export function pushHealthPoll(
  buffers: FleetRingBuffers,
  health: HealthStatus[],
  t: number = Date.now(),
): FleetRingBuffers {
  const next: FleetRingBuffers = { ...buffers };
  for (const h of health) {
    const existing = next[h.system] ?? [];
    const point: HealthPoint = { t, available: h.available };
    const appended = existing.length >= RING_BUFFER_CAPACITY
      ? [...existing.slice(existing.length - RING_BUFFER_CAPACITY + 1), point]
      : [...existing, point];
    next[h.system] = appended;
  }
  return next;
}

/** The most recent sample for a system, or `null` if its buffer is empty/absent. */
export function latest(buffers: FleetRingBuffers, system: SystemId): HealthPoint | null {
  const arr = buffers[system];
  return arr && arr.length > 0 ? arr[arr.length - 1] : null;
}

/** Uptime ratio (0..1) over a system's retained window — the sparkline's y-values are derived
 *  from the raw points directly, but panels/tests needing a single summary number (e.g. the
 *  fleet card's uptime %) use this. Returns `null` for an empty buffer (nothing to compute). */
export function uptimeRatio(buffers: FleetRingBuffers, system: SystemId): number | null {
  const arr = buffers[system];
  if (!arr || arr.length === 0) return null;
  const up = arr.filter(p => p.available).length;
  return up / arr.length;
}

/**
 * Availability-transition detection: walks one system's buffer and returns every point where
 * `available` differs from the immediately preceding point (or from `null` at the very first
 * sample — a "transition into the observed window", not assumed to be a real flap). Used by
 * FleetPanel to annotate the sparkline / surface a "flapped N times" note without re-deriving
 * it from scratch on every render.
 */
export function transitions(buffers: FleetRingBuffers, system: SystemId): Transition[] {
  const arr = buffers[system];
  if (!arr || arr.length === 0) return [];
  const out: Transition[] = [];
  let prev: boolean | null = null;
  for (const point of arr) {
    if (prev === null || point.available !== prev) {
      out.push({ system, from: prev, to: point.available, t: point.t });
    }
    prev = point.available;
  }
  return out;
}

/** Empty buffers for every known system — FleetPanel's initial state. */
export function emptyFleetRingBuffers(): FleetRingBuffers {
  return {};
}
