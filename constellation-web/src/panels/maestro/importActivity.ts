// MACT-05 (MUSE-125): pure helpers for the Maestro Activity panel's Import/acquisition
// section. Deliberately DOM-free (same rationale as nowPlaying.ts) so the two honesty rules
// this file exists to enforce are unit-testable directly, without a fetch mock or a rendered
// component:
//
//   1. THE SEAM IS THE POINT. `GET /api/requests/queue` (Muse `src/web/dashboard.rs::
//      get_requests_queue`) emits `"progress": Value::Null` behind an in-code
//      `// SEAM: real download %/ETA not persisted` comment — qBittorrent per-torrent
//      progress genuinely is not persisted today. `importProgressDisplay` renders that as
//      "not tracked" with a tooltip naming the seam. It must NEVER draw a bar at an invented
//      percentage, and must NEVER silently drop the column as though nobody wanted it.
//   2. NO NEW TRACKING. Row grouping/labelling here only re-presents what the existing
//      endpoint already returns (`status`, `size_bytes`, `added_at`) — no client-side
//      "is this actually importing" heuristic gets invented; wiring state comes from the
//      EXISTING `/api/subsystems` payload (see `wiringBadge` below), never a parallel notion
//      built here.
import type { BadgeTone } from '../../components/Badge';
import type { MuseDownloadQueueRow, MuseWantedTitleRow } from '../../hooks/useMuse';

// ── Pipeline grouping (row order tells the story) ────────────────────────────

/** The real statuses `get_requests_queue` queries, in the order the acquisition pipeline
 *  actually moves a release through — queued → downloading → importing → completed. Group
 *  order mirrors this so a reader doesn't have to mentally re-sort a flat table. */
export const PIPELINE_STATUS_ORDER = ['queued', 'downloading', 'importing', 'completed'] as const;

export interface QueueStatusGroup {
  status: string;
  /** `false` for any status outside `PIPELINE_STATUS_ORDER` — rendered verbatim, never folded
   *  into a known bucket (same vocabulary-discipline rule `SubsystemHealth.tsx` and
   *  `classifyDecision` in nowPlaying.ts already established for this codebase). */
  known: boolean;
  rows: MuseDownloadQueueRow[];
}

/** Groups queue rows by status in pipeline order; any status this endpoint didn't enumerate
 *  (a future/unexpected value) is appended afterward, verbatim, rather than silently dropped.
 *  Groups with zero rows are omitted — the caller's outer empty state covers "nothing at all",
 *  and an empty "importing" header for every load would be noise, not signal. */
export function groupQueueByPipelineStatus(rows: MuseDownloadQueueRow[]): QueueStatusGroup[] {
  const groups: QueueStatusGroup[] = PIPELINE_STATUS_ORDER.map(status => ({
    status,
    known: true,
    rows: rows.filter(r => r.status === status),
  }));

  const knownSet = new Set<string>(PIPELINE_STATUS_ORDER);
  const unknownStatuses = Array.from(new Set(rows.map(r => r.status).filter(s => !knownSet.has(s)))).sort();
  for (const status of unknownStatuses) {
    groups.push({ status, known: false, rows: rows.filter(r => r.status === status) });
  }

  return groups.filter(g => g.rows.length > 0);
}

/** Title-cases a known status; an unknown one renders as-is + "(unrecognised)". */
export function statusGroupLabel(group: QueueStatusGroup): string {
  if (!group.known) return `${group.status} (unrecognised)`;
  return group.status.charAt(0).toUpperCase() + group.status.slice(1);
}

// ── Progress (the seam) ───────────────────────────────────────────────────────

export interface ImportProgressDisplay {
  label: string;
  tracked: boolean;
  tone: BadgeTone;
  /** Names the seam (the handler's own `// SEAM` comment) — never `null` when untracked, so
   *  every caller can point a tooltip at *why* without inventing its own wording. */
  tooltip: string | null;
}

const PROGRESS_SEAM_TOOLTIP =
  "Real download %/ETA is not persisted yet (Muse's get_requests_queue emits progress: null — " +
  'see its own "SEAM: real download %/ETA not persisted" comment). Persisting it is a follow-up ' +
  'against the acquisition worker, not this panel.';

/** `null`/`undefined` (the current, universal reality per the SEAM comment) renders "not
 *  tracked" with a tooltip naming the seam — NEVER a `0%` bar, which would read as a real
 *  measurement of a download that hasn't started. A genuine future numeric value (once the
 *  seam is closed) renders as an actual percentage, so this function does not need to change
 *  when that follow-up lands. */
export function importProgressDisplay(progress: number | null | undefined): ImportProgressDisplay {
  if (progress == null) {
    return { label: 'not tracked', tracked: false, tone: 'neutral', tooltip: PROGRESS_SEAM_TOOLTIP };
  }
  const clamped = Math.max(0, Math.min(100, progress));
  return { label: `${Math.round(clamped)}%`, tracked: true, tone: 'blue', tooltip: null };
}

// ── Size / age formatting (facts, no editorialising) ─────────────────────────

/** `null`/`undefined`/`0` all render "—" — a zero here is indistinguishable from "not
 *  reported" at this endpoint (same convention `RequestsPanel.tsx`'s `formatSize` already
 *  uses for this exact field), so it is never presented as a confirmed zero-byte release. */
export function formatQueueSize(bytes: number | null | undefined): string {
  if (bytes === null || bytes === undefined || bytes === 0) return '—';
  const units = ['B', 'KB', 'MB', 'GB', 'TB'];
  let v = bytes;
  let u = 0;
  while (v >= 1024 && u < units.length - 1) {
    v /= 1024;
    u += 1;
  }
  return `${v >= 10 || u === 0 ? Math.round(v) : v.toFixed(1)} ${units[u]}`;
}

/** How long a row has sat at its current status (age since `added_at`) — a FACT, never a
 *  verdict. A release stuck in `downloading` for three days still just reports "3d ago"; this
 *  function does not decide whether that is a problem (EDGE CASES: "do not editorialise a
 *  verdict"). `now` is injectable so this stays deterministic under test without faking global
 *  timers. */
export function formatQueueAge(addedAt: string, now: number = Date.now()): string {
  const then = new Date(addedAt).getTime();
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

// ── Wanted count (links to the existing Requests panel — MGUI-14, never duplicated) ──────────

/** A compact label for the "waiting on a release" count — the full list already lives on the
 *  Muse Requests panel (MGUI-14); this section only ever shows the count + a link there, per
 *  this item's own "does not duplicate that panel" requirement. */
export function wantedCountLabel(wanted: MuseWantedTitleRow[] | unknown[]): string {
  const n = wanted.length;
  if (n === 0) return 'Nothing waiting on a release';
  return `${n} waiting on a release`;
}

// ── Wiring state (reused from /api/subsystems — never a parallel "is it importing" guess) ────

export interface WiringDisplay {
  label: string;
  tone: BadgeTone;
  known: boolean;
}

/** Same four-state vocabulary `SubsystemHealth.tsx` renders (`live`/`worker`/`seam`/
 *  `unmounted`) mapped onto `Badge`'s fixed tone set. An unrecognised state renders verbatim +
 *  "(unclassified)", never coerced into a known tone — this file's other vocabulary-discipline
 *  rule, applied to the same wiring signal. `null` (subsystem not found in the payload, or the
 *  section hasn't loaded yet) renders as "unknown", never as a fabricated "unmounted". */
export function wiringDisplay(state: string | null): WiringDisplay {
  if (state === null) return { label: 'unknown', tone: 'neutral', known: false };
  switch (state.toLowerCase()) {
    case 'live': return { label: 'live', tone: 'green', known: true };
    case 'worker': return { label: 'worker', tone: 'blue', known: true };
    case 'seam': return { label: 'seam', tone: 'amber', known: true };
    case 'unmounted': return { label: 'unmounted', tone: 'neutral', known: true };
    default: return { label: `${state} (unclassified)`, tone: 'neutral', known: false };
  }
}

// Review fix (round 2, codex): the first version of `emptyQueueReason` (a) ignored `wanted`
// entirely, so a deployment with real monitored titles and an empty queue rendered "nothing is
// currently monitored" right next to a "N waiting on a release" link contradicting it in the
// same section; and (b) translated `seam`/`unmounted` into a SPECIFIC diagnosis ("needs
// Prowlarr + a download client") that the subsystem payload does not actually assert — `seam`
// only means "implemented, not yet producing data" and `unmounted` only means "not configured"
// (the literal vocabulary `get_subsystems`'s own doc comment defines, Muse `src/web/
// dashboard.rs`); neither says WHICH dependency is missing. That was the same fabricated-
// confidence failure mode this item's own progress-seam handling exists to prevent, just one
// function later — "the section correctly refuses to invent a download percentage, then
// invents a configuration diagnosis two functions later" (review finding, verified in source).
//
// Fixed: report the two facts the payload actually contains — whether anything is monitored
// (from `wanted`, which this function now takes), and the subsystem's OWN state word plus its
// OWN documented meaning (never a derived diagnosis) — and say nothing beyond that.
//
// SOURCE OF TRUTH for the four values below: the doc comment directly above
// `pub async fn get_subsystems` in Muse `src/web/dashboard.rs` (as of the S130 tree) --
//   "`live` (wired + has data), `worker` (wired, background/on-demand), `seam` (implemented,
//   not yet producing data), `unmounted` (not configured)."
// These strings are meant to TRACK that comment, not paraphrase it -- if it changes, re-check
// and update this map so the authority stays the authority.
const WIRING_STATE_MEANING: Record<string, string> = {
  live: 'wired + has data',
  worker: 'wired, background/on-demand',
  seam: 'implemented, not yet producing data',
  unmounted: 'not configured',
};

/** Why the queue is empty. Two independent, honestly-scoped facts, never conflated:
 *   1. Whether anything currently has no file at all (`wantedCount`, from the SAME payload's
 *      `wanted[]` — the count this section already renders as a link). `wanted[]` only means
 *      "monitored, no `media_files` row" (see `WantedTitleRow`'s doc in Muse's
 *      `src/repo/dashboard.rs`) — it does NOT establish that nothing has ever been grabbed (a
 *      release can complete and still have no file yet, or have left the queue in an earlier
 *      run), so the message says exactly that and nothing more.
 *   2. The acquisition subsystem's own `/api/subsystems` state WORD plus its OWN documented
 *      meaning (`WIRING_STATE_MEANING`, transcribed from `get_subsystems`'s doc comment) — never
 *      a derived claim about which dependency is missing, since the payload doesn't say that. */
export function emptyQueueReason(acquisitionState: string | null, wantedCount: number): string {
  // Review fix (round 3, codex): "N titles monitored with nothing grabbed yet" was itself an
  // overclaim -- `wanted[]` establishes "no file yet", not "never grabbed". A release can be
  // grabbed and complete while the file is still absent, or have left the queue in a previous
  // run; this endpoint carries no history that rules that out. Say only what `wanted[]` means.
  const monitoredNote = wantedCount > 0
    ? `${wantedCount} monitored title${wantedCount === 1 ? '' : 's'} ${wantedCount === 1 ? 'has' : 'have'} no file yet`
    : 'nothing is currently monitored';

  if (acquisitionState === null) {
    return `Nothing is currently queued (${monitoredNote}).`;
  }
  const meaning = WIRING_STATE_MEANING[acquisitionState.toLowerCase()];
  const stateNote = meaning
    ? `Acquisition reports state "${acquisitionState}" — ${meaning} (see Subsystem health).`
    : `Acquisition reports state "${acquisitionState}" (see Subsystem health).`;
  return `Nothing is currently queued (${monitoredNote}). ${stateNote}`;
}
