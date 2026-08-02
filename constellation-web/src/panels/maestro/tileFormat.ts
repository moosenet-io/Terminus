// MACT-06 (MUSE-126): pure formatters + tri-state tile-value classification for the Maestro
// Activity stat-tile row. Deliberately DOM-free (same rationale as nowPlaying.ts /
// importActivity.ts) so the honesty rule this file exists to enforce is unit-testable
// directly, without rendering the panel.
//
// THE HONESTY RULE (CLAUDE.md, "three states, never conflated"):
//   1. a REAL value, including a genuine `0` — rendered as the value, never coerced away.
//   2. NOT REPORTED (the endpoint answered 2xx but the field itself is null/absent) — an
//      em-dash, NEVER a fabricated `0`.
//   3. DEGRADED (the request itself failed / timed out / 401'd / never loaded) — visually
//      distinct from both of the above, naming the cause (never a bare "—" with no reason).
// A FOURTH, separate case exists only for the H2 (MACT-11) host/capacity tiles: they have NO
// H1 data source at all (no fetch, no endpoint), so `MAESTRO_SEAM_LABEL` is a fixed inert
// string a caller renders unconditionally — see `SeamTile` in ActivityTiles.tsx. It must never
// be confused with "degraded" (a real fetch that failed) or "not reported" (a real fetch that
// succeeded with an absent field) — nothing was ever asked.

/** The fixed H2 seam label. A `SeamTile` renders exactly this string regardless of any prop —
 *  there is no branch that could substitute a `0`, because there is no data path to source one
 *  from in H1. Exported so the mutation-proof test can assert the literal wording, and so a
 *  caller never hand-types a near-duplicate string that drifts from this one. */
export const MAESTRO_SEAM_LABEL = 'requires Maestro — not deployed';

/** Max length of the VISIBLE degraded-cause line `StatTile` renders beneath the `—`. Review
 *  finding (round 2, codex): the first cut of `StatTile` put the cause ONLY in an HTML `title`
 *  attribute — invisible without a mouse hover, unavailable on touch, not reliably surfaced by
 *  assistive tech, so the only VISIBLE difference between "degraded" and "not reported" was
 *  colour alone. That fails the three-states rule (colour-only is not a distinct rendered
 *  state) and is an independent accessibility defect. The fix renders a short cause line as
 *  real card content; this constant bounds it so a long `HttpStatusError` message (a full URL
 *  path, say) doesn't blow out the tile's fixed grid width. The full, untruncated detail stays
 *  available via `title` too — that remains a nice-to-have, not the sole carrier. */
export const DEGRADED_DETAIL_MAX_LEN = 28;

/** Truncates a degrade `detail` string to a tile-sized visible line, ellipsizing rather than
 *  hard-cutting mid-word where a trailing space exists. A string already within the bound is
 *  returned verbatim (never re-padded/re-shaped) — this function only ever shortens. */
export function truncateDetail(detail: string, maxLen: number = DEGRADED_DETAIL_MAX_LEN): string {
  if (detail.length <= maxLen) return detail;
  return `${detail.slice(0, Math.max(0, maxLen - 1)).trimEnd()}…`;
}

/** Restricted to the tones `StatTile` actually maps onto a `MetricCard` `valueColor`
 *  (components/Card.tsx's `StatusColor`) — kept narrow so a formatter cannot invent a tone the
 *  renderer doesn't understand. `undefined` (omitted) means "primary", the plain-fact default. */
export type TileTone = 'success' | 'warning' | 'tertiary';

export type TileValueState =
  | { kind: 'loading' }
  | { kind: 'degraded'; detail: string }
  | { kind: 'value'; text: string; tone?: TileTone };

/** Loading/degraded/ready classification shared by every H1 stat tile — built straight from the
 *  `MuseSection`-shaped contract every `useMuse*` hook (and the local Terminus-health hook)
 *  already returns (`{data, loading, degraded}`), so a caller never re-derives the rule.
 *
 *  `section.data === null` with `degraded === false` (fetch resolved but returned no body) is
 *  folded into `degraded` too — a null body is not a value to format, and treating it as
 *  "ready" would let a formatter dereference a field on `null`. This mirrors `useMuseSection`'s
 *  own null/undefined-is-not-wired handling one level up. */
export function tileStateFromSection<T>(
  section: { data: T | null; loading: boolean; degraded: { detail: string } | false },
  formatReady: (data: T) => { text: string; tone?: TileTone },
): TileValueState {
  if (section.loading) return { kind: 'loading' };
  if (section.degraded) return { kind: 'degraded', detail: section.degraded.detail };
  if (section.data === null) return { kind: 'degraded', detail: 'no data reported' };
  const { text, tone } = formatReady(section.data);
  return { kind: 'value', text, tone };
}

/** `null`/`undefined`/non-finite render "—", NEVER "0" — a fabricated zero reads as a real
 *  measurement (the same rule nowPlaying.ts's `formatMs` and importActivity.ts's
 *  `formatQueueSize` already enforce for their own fields). A genuine `0` renders "0" — this is
 *  the function the mutation-proof test pins both directions of. */
export function formatCount(n: number | null | undefined): string {
  if (n === null || n === undefined || !Number.isFinite(n)) return '—';
  return String(n);
}

/** Relative "ago" label for an ISO timestamp. `null`/unparsable render "—", never "0m ago" (a
 *  fabricated recency reads as a real ingest event). `now` is injectable so this stays
 *  deterministic under test without faking global timers — same convention as
 *  importActivity.ts's `formatQueueAge`. */
export function formatRelativeTimestamp(iso: string | null | undefined, now: number = Date.now()): string {
  if (!iso) return '—';
  const then = new Date(iso).getTime();
  if (!Number.isFinite(then)) return '—';
  const diffMs = Math.max(0, now - then);
  const sec = Math.floor(diffMs / 1000);
  if (sec < 60) return 'just now';
  const min = Math.floor(sec / 60);
  if (min < 60) return `${min}m ago`;
  const hr = Math.floor(min / 60);
  if (hr < 24) return `${hr}h ago`;
  const day = Math.floor(hr / 24);
  return `${day}d ago`;
}

/** Muse's own `/health` vocabulary — Muse `src/http/mod.rs::health` answers exactly
 *  `{"status":"ok","version":"<crate version>","db":"up"|"down"}` (a hand-built `json!({...})`,
 *  not a struct `Serialize`, so all three keys are unconditionally present). `status` is
 *  observed to always be the literal `"ok"` (the handler has no other branch that sets it), but
 *  an unrecognised value still renders verbatim + "(unrecognised)" — the same vocabulary-
 *  discipline rule `classifyDecision`/`wiringDisplay` already established in this codebase —
 *  rather than being silently coerced into looking healthy. */
export interface MuseHealthPayload {
  status: string;
  version?: string;
  db: string;
}

/** `db: "down"` is the one state this endpoint can report that is genuinely bad news while the
 *  HTTP request itself still succeeded (200) — the handler answers 200 either way (see its own
 *  `db_status` match). That is exactly why this is a "ready" value with a `warning` tone, not a
 *  `degraded` tile: the fetch worked and told the truth; degrading the whole tile would hide
 *  the very fact ("the DB probe failed") this tile exists to surface. */
export function formatMuseHealth(h: MuseHealthPayload): { text: string; tone?: TileTone } {
  const dbKnown = h.db === 'up' || h.db === 'down';
  const dbLabel = dbKnown ? h.db : `${h.db} (unrecognised)`;
  const statusKnown = h.status === 'ok';
  const statusLabel = statusKnown ? h.status : `${h.status} (unrecognised)`;
  const text = `${statusLabel} · db ${dbLabel}`;
  if (statusKnown && h.db === 'up') return { text, tone: 'success' };
  if (h.db === 'down') return { text, tone: 'warning' };
  return { text, tone: 'tertiary' };
}

/** "N live of M modules" from the SAME `/api/subsystems` payload `SubsystemHealth.tsx` already
 *  renders in full (MGUI-06) — this tile is a compact summary, never a second notion of wiring
 *  state. Case-insensitive to match `SubsystemHealth.tsx`'s own `state.toLowerCase()` check. An
 *  empty list still answers "0 live of 0" — a genuine fact from a successful, empty response,
 *  not a degrade. */
export function formatSubsystemWiring(subsystems: { state: string }[]): { text: string } {
  const live = subsystems.filter(s => s.state.toLowerCase() === 'live').length;
  return { text: `${live} live of ${subsystems.length}` };
}

/** "N up of M" from the shell's existing `GET /api/health` (`HealthStatus[]`, one entry per
 *  system) — the SAME payload App.tsx already polls to drive module availability; this tile
 *  re-presents it, it does not open a second notion of "is a module up". */
export function formatModuleHealth(entries: { available: boolean }[]): { text: string } {
  const up = entries.filter(e => e.available).length;
  return { text: `${up} up of ${entries.length}` };
}
