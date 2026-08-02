// MACT-04 (MUSE-124): pure helpers for the Maestro Activity panel — progress formatting,
// decision → badge classification, staleness, source labelling and the two-pane filter/degrade
// helpers. Deliberately DOM-free so every rule here is unit-testable directly (see
// `nowPlaying.test.ts`), without standing up the panel component or a browser.
//
// Two honesty rules this file exists to enforce (both were real defects elsewhere in the
// epic — see CLAUDE.md's "honesty requirements" for MACT-04):
//   1. A `null`/absent measurement renders as "—", NEVER as `0`/`0%` — a fabricated zero reads
//      as a real reading and sends an operator debugging a healthy system.
//   2. An unrecognised decision/source vocabulary value renders VERBATIM + "(unclassified)" /
//      "(unrecognised)", the same vocabulary-discipline rule `SubsystemHealth.tsx` established —
//      never silently coerced to a friendly default like "Direct play".
import type { BadgeTone } from '../../components/Badge';
import type { PillState } from '../../components/StatusPill';
import type { SessionAccount, SessionDecision, SessionItem, SessionPlayState } from '../../lib/aggregationClient';

// ── Progress (position/duration) ─────────────────────────────────────────────

/** `null`/undefined milliseconds render as "—", never "0:00" (a fabricated zero reads as a real
 *  measurement — see the module doc's rule 1). */
export function formatMs(ms: number | null | undefined): string {
  if (ms == null || !Number.isFinite(ms) || ms < 0) return '—';
  const totalSec = Math.floor(ms / 1000);
  const h = Math.floor(totalSec / 3600);
  const m = Math.floor((totalSec % 3600) / 60);
  const s = totalSec % 60;
  const ss = String(s).padStart(2, '0');
  return h > 0 ? `${h}:${String(m).padStart(2, '0')}:${ss}` : `${m}:${ss}`;
}

export interface ProgressInfo {
  /** Straight passthrough of the wire `progress_pct` — ALREADY 0..100 scaled (MACT-01). This
   *  function must never multiply/divide it; `progressInfo.test.ts`-equivalent cases pin that.
   *  `null` (not `0`) when Muse never sent the key (unknown duration) — see `LiveSessionOut`'s
   *  `progress_pct` doc in aggregationClient.ts for why that's modelled as an absent key. */
  pct: number | null;
  positionLabel: string;
  durationLabel: string;
  /** "21:24 / 2:22:00", or "—" pieces where a side is unknown. Never "0:00 / —" for an
   *  in-progress session with a real position — each side degrades independently. */
  combinedLabel: string;
}

/** Builds the position/duration/percent trio a LIVE or HISTORY card renders. `progressPct` is
 *  `undefined` when the wire key was absent (unknown duration) — kept distinct from `null` at
 *  the call boundary but collapsed to `null` here, since callers only need "unreportable". */
export function progressInfo(
  viewOffsetMs: number | null,
  durationMs: number | null,
  progressPct: number | undefined,
): ProgressInfo {
  const positionLabel = formatMs(viewOffsetMs);
  const durationLabel = formatMs(durationMs);
  return {
    pct: progressPct ?? null,
    positionLabel,
    durationLabel,
    combinedLabel: `${positionLabel} / ${durationLabel}`,
  };
}

// ── Decision → badge classification ──────────────────────────────────────────

/** Muse's `decision_kind_str` vocabulary for a single decision field (`video_decision` /
 *  `audio_decision`). Anything outside this set is unrecognised. */
const KNOWN_DECISION_VALUES = new Set(['direct_play', 'direct_stream', 'transcode', 'copy']);

export type DecisionKind = 'direct_play' | 'remux' | 'transcode' | 'unclassified';

export interface DecisionBadge {
  kind: DecisionKind;
  label: string;
  tone: BadgeTone;
  /** `transcode_reason` when the backend supplied one — the badge's tooltip content. `null`
   *  when absent, never coerced to an empty string (lets a caller tell "no reason given" from
   *  "no tooltip"). */
  tooltip: string | null;
}

/** Classifies a session's stream-decision fields into the three UI buckets (Direct play / Remux /
 *  Transcode), or `unclassified` for a value this vocabulary doesn't recognise — rendered
 *  verbatim + "(unclassified)", never silently defaulted to "Direct play" (the failure mode this
 *  function exists to prevent; see the module doc's rule 2).
 *
 *  Review fix (MUSE-124): the unknown-check MUST cover `transcode_decision`, not just
 *  `video_decision`/`audio_decision` — a session with known direct-play video/audio but a
 *  garbage `transcode_decision` was previously classified "Direct play" anyway, silently
 *  dropping the one field that actually carries an unrecognised value. All three fields share
 *  Muse's one `decision_kind_str` vocabulary (see `SessionDecision`'s doc comment), so all three
 *  gate the unclassified check; only `video_decision`/`audio_decision` decide the DIRECT_PLAY vs
 *  REMUX split once everything present is known. */
export function classifyDecision(decision: SessionDecision): DecisionBadge {
  const { video_decision, audio_decision, transcode_decision, transcode_reason } = decision;
  const values = [video_decision, audio_decision, transcode_decision];
  const allKnownOrNull = values.every(v => v === null || KNOWN_DECISION_VALUES.has(v));

  if (!allKnownOrNull) {
    const unknown = values.find(v => v !== null && !KNOWN_DECISION_VALUES.has(v));
    return { kind: 'unclassified', label: `${unknown} (unclassified)`, tone: 'neutral', tooltip: transcode_reason };
  }
  if (video_decision === null && audio_decision === null) {
    return { kind: 'unclassified', label: 'unknown (unclassified)', tone: 'neutral', tooltip: transcode_reason };
  }
  if (video_decision === 'transcode' || audio_decision === 'transcode' || transcode_decision === 'transcode') {
    return { kind: 'transcode', label: 'Transcode', tone: 'amber', tooltip: transcode_reason };
  }
  if (video_decision === 'direct_play' && audio_decision === 'direct_play') {
    return { kind: 'direct_play', label: 'Direct play', tone: 'green', tooltip: transcode_reason };
  }
  return { kind: 'remux', label: 'Remux', tone: 'blue', tooltip: transcode_reason };
}

// ── Play-state pill (staleness is its own state, never a synonym for paused) ────────────────

/** `stale` gets its OWN visibly-different pill (amber "warm") — it is a session Muse hasn't
 *  heard from recently, not a person who chose to pause. Coercing it into `paused`'s pill would
 *  hide exactly the distinction MACT-01 introduced the state to carry. */
export function statePillState(state: SessionPlayState): PillState {
  switch (state) {
    case 'playing': return 'online';
    case 'paused': return 'idle';
    case 'stale': return 'warm';
  }
}

export function statePillLabel(state: SessionPlayState): string {
  switch (state) {
    case 'playing': return 'Playing';
    case 'paused': return 'Paused';
    case 'stale': return 'Stale';
  }
}

// ── Source labelling (the H1→H2 flip must be visible, never a silent identity swap) ─────────

/** Renders the LIVE pane's own `source` value FROM THE ENVELOPE — never a hardcoded literal —
 *  so H2's flip from `"muse-derived"` to `"maestro-live"` shows up as a visible, explained
 *  change in the pane header instead of a silent identity swap (the whole point of this item;
 *  see the LiveSession/HistorySession doc comment in aggregationClient.ts). An unrecognised
 *  source string still renders verbatim, never coerced to either known label. */
export function liveSourceLabel(source: string): string {
  switch (source) {
    case 'muse-derived':
      return 'live view derived from Muse watch history';
    case 'maestro-live':
      return 'Maestro live sessions';
    default:
      return `${source} (unrecognised source)`;
  }
}

export function historySourceLabel(source: string): string {
  return source === 'muse-history' ? "Muse's permanent historical record" : `${source} (unrecognised source)`;
}

/** MACT-08 (MUSE-128): the panel's cadence label — CLAUDE.md's "no colour-only or title-only
 *  signal" rule applies here as much as anywhere: whether a pane is live or on a polling
 *  cadence must be a plain-text statement, not merely implied by (e.g.) a coloured dot.
 *  Renders "live" or "polling every Ns" from `useActivityFeedLive`'s returned
 *  `{live, pollIntervalMs}`. As of review round 2, `live` is always `false` (there is no WS
 *  path — see that hook's own module doc for why it was tried and dropped), so today this
 *  always renders "polling every Ns"; the `live` branch is kept so the function's contract
 *  stays correct if a genuine live source is ever wired in later, without a call-site change. */
export function feedModeLabel(live: boolean, pollIntervalMs: number): string {
  if (live) return 'live';
  const seconds = Math.round(pollIntervalMs / 1000);
  return `polling every ${seconds}s`;
}

// ── Degrade-cause naming (never a bare "unavailable") ────────────────────────────────────────

/** Turns the typed client's `detail` (an `HttpStatusError` message, `"HTTP {status} for {path}"`
 *  — see aggregationClient.ts's `HttpStatusError`/`classifyError`) into an operator-actionable
 *  cause. A 401 specifically names the unprovisioned bearer (TERM-549) rather than reading as a
 *  generic failure — this is the one case CLAUDE.md's honesty rules call out by name for this
 *  item, since it WILL happen in practice until that token is provisioned. */
// MACT-05 (MUSE-125): generalised with an optional `feedLabel` so the Import Activity section
// (a different protected endpoint, `/api/requests/queue`) gets the same 401/404/not-wired
// cause-naming discipline without a copy-pasted near-duplicate — every existing call site keeps
// the original "Muse session feed" wording via the default.
export function degradeCause(detail: string | undefined, feedLabel = 'Muse session feed'): string {
  if (!detail) return `${feedLabel} is unavailable (no further detail reported).`;
  if (/HTTP 401/.test(detail)) {
    return `${feedLabel} requires CONSTELLATION_MUSE_TOKEN, which is unprovisioned on this deployment (TERM-549) — the backend rejected the request as unauthenticated.`;
  }
  if (/HTTP (404|501)/.test(detail)) {
    return `${feedLabel} isn't wired on this backend yet (404/501).`;
  }
  return detail;
}

// ── Item / account labels (unresolved data renders as such, never blank/invented) ───────────

export function accountLabel(account: SessionAccount): string {
  if (account.display_name) return account.display_name;
  if (account.id != null) return `Account #${account.id}`;
  return 'Unknown account';
}

/** A session whose `item` never resolved (no `media_item_id`/`title`) still renders — the card
 *  shows device/account/progress with the item explicitly marked unresolved, per this item's
 *  EDGE CASES ("a session with no resolved item"). */
export function itemTitle(item: SessionItem): string {
  if (!item.title) return 'Unresolved item';
  if (item.kind === 'show') {
    const se = item.season_number != null && item.episode_number != null
      ? `S${String(item.season_number).padStart(2, '0')}E${String(item.episode_number).padStart(2, '0')}`
      : null;
    const epTitle = item.episode_title ?? null;
    const suffix = [se, epTitle].filter(Boolean).join(' · ');
    return suffix ? `${item.title} — ${suffix}` : item.title;
  }
  return item.year != null ? `${item.title} (${item.year})` : item.title;
}

export function isItemResolved(item: SessionItem): boolean {
  return item.media_item_id != null && item.title != null;
}

// ── Filter chips (built from the data's own distinct values, house pattern) ─────────────────

/** Distinct, sorted, non-null values for one field across a set of sessions — the same
 *  "chips built from the data's own distinct values" rule `terminus/ActivityPanel.tsx` uses,
 *  generalised so both the LIVE and HISTORY panes (and their different row shapes) share it. */
export function distinctBy<T>(rows: T[], pick: (row: T) => string | null | undefined): string[] {
  return Array.from(new Set(rows.map(pick).filter((v): v is string => v != null && v !== ''))).sort();
}
