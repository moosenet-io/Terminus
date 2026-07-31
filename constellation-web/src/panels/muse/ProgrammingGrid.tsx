// MGUI-10 (S129): the TV-director PROGRAMMING GRID — guide screen 09. A channels × time
// grid with proportional programme blocks, a now marker, and tuner telemetry.
//
// It is the `chart` half of the Guide card in `ChannelsPanel`; the pre-existing DataTable
// timeline is kept as the `table` half (it is correct, denser, and the only view that shows
// exact start/end timestamps — this grid replaces nothing).
//
// ── WHAT THIS RENDERS TODAY, AND WHY IT IS EMPTY ─────────────────────────────────────────
//
// On this deployment the grid renders an EMPTY STATE, and that is the correct outcome:
//
//   • `GET /api/channels` returns `[]` — zero channels. Observed, through the proxy.
//   • `GET /guide` returns `{"raw":"<!doctype html>…"}` — Muse's own HUMAN-FACING guide
//     page, not a programme feed. There is no JSON schedule endpoint behind it today
//     (`/api/guide`, `/guide.json`, `/api/channels/guide`, `/xmltv`, `/api/epg` all 404).
//   • `channels.compose` — the thing that would CREATE a channel — is implemented and
//     tested but HAS NO HTTP ROUTE MOUNTED (stated as a seam in the design guide itself,
//     screen 08: "channels.compose is implemented + tested; no HTTP route mounts it yet").
//
// The empty state says exactly those three things and nothing more. In particular it does
// NOT claim a compose worker failed, stalled, or has not run — nothing in any response
// reports worker state, so that would be an invented cause. "There are no channels" is
// observable; "here is why there are no channels" is not, beyond the missing route.
//
// ── WHAT IS DELIBERATELY ABSENT FROM THE GUIDE'S SCREEN 09 ───────────────────────────────
//
//   • **XMLTV** (guide subtitle "HDHomeRun · XMLTV · 48h window"). No XMLTV endpoint exists
//     (`/xmltv` → 404). HDHomeRun IS real (see `/discover.json` below), so that half stays.
//   • **The fixed "48h window".** The axis span is DERIVED from the programme entries that
//     actually exist. A hardcoded 48h axis on an empty grid would be a fabricated frame,
//     complete with a meaningless now-marker position.
//   • **`ch.mode` badge** (per-channel mode chip). `/api/channels` is empty here, so the
//     element shape is unverified; the only channel fields this module has ever observed are
//     `id`/`name`/`item_count`. A badge with no backing field is not shipped.
//   • **Directional-tone coding** of programme blocks (colour by kind/genre). A guide entry
//     carries `channel_id`/`title`/`start`/`end` and nothing categorical. Every block is
//     therefore drawn in the same neutral tone — a colour scale over an absent field would
//     encode noise as meaning.
//   • **Live tuner OCCUPANCY.** `/discover.json` advertises `TunerCount` (a declared
//     capacity). No field reports how many are in use, so the footer says "4 tuners" and
//     never "n of 4 busy".
import { useMemo } from 'react';
import {
  museChannelList,
  museGuideEntries,
  useMuseChannels,
  useMuseGuide,
  useMuseTuner,
  useMuseTunerLineup,
  type MuseChannel,
  type MuseGuideEntry,
} from '../../hooks/useMuse';

/** One rendered row: a channel (or a channel id referenced only by the guide) + its blocks. */
interface GridRow {
  key: string;
  label: string;
  /** `null` when the row exists only because a guide entry named a channel the channel list
   *  does not contain — we know its id, not its item count. */
  channel: MuseChannel | null;
  entries: MuseGuideEntry[];
}

interface TimeWindow {
  startMs: number;
  endMs: number;
  /** Hour-aligned tick marks across the window, inclusive of the first. */
  ticks: number[];
}

const HOUR_MS = 3_600_000;
/** Aim for roughly this many axis labels; the step snaps up to a whole number of hours so
 *  every tick lands on the hour, which is what makes a schedule readable. */
const TARGET_TICKS = 8;

/** The axis span, derived ONLY from entries that exist. Returns `null` for no entries — the
 *  caller then draws no axis and no now marker rather than inventing a frame. */
export function deriveWindow(entries: MuseGuideEntry[], nowMs?: number): TimeWindow | null {
  const bounds: number[] = [];
  for (const e of entries) {
    const s = Date.parse(e.start);
    const t = Date.parse(e.end);
    // An unparseable timestamp is skipped, not coerced to `now` — placing a block at an
    // invented time is exactly the kind of default-an-absence-into-a-value error this
    // panel exists to avoid.
    if (Number.isFinite(s)) bounds.push(s);
    if (Number.isFinite(t)) bounds.push(t);
  }
  if (bounds.length === 0) return null;

  let startMs = Math.min(...bounds);
  let endMs = Math.max(...bounds);
  // Include "now" in the frame only when it is already close to the schedule (within one
  // window-length). Otherwise a single stale entry would stretch the axis across months
  // just to reach the now marker.
  if (nowMs !== undefined) {
    const span = Math.max(endMs - startMs, HOUR_MS);
    if (nowMs >= startMs - span && nowMs <= endMs + span) {
      startMs = Math.min(startMs, nowMs);
      endMs = Math.max(endMs, nowMs);
    }
  }
  // Snap outward to whole hours so ticks are hour-aligned.
  startMs = Math.floor(startMs / HOUR_MS) * HOUR_MS;
  endMs = Math.ceil(endMs / HOUR_MS) * HOUR_MS;
  if (endMs <= startMs) endMs = startMs + HOUR_MS;

  const hours = (endMs - startMs) / HOUR_MS;
  const stepHours = Math.max(1, Math.ceil(hours / TARGET_TICKS));
  const ticks: number[] = [];
  for (let t = startMs; t <= endMs; t += stepHours * HOUR_MS) ticks.push(t);
  return { startMs, endMs, ticks };
}

/** Left offset + width as percentages of the window. Clamped to the window so an entry that
 *  starts before / ends after the frame is drawn truncated rather than overflowing the row. */
export function blockGeometry(
  entry: MuseGuideEntry,
  win: TimeWindow,
): { leftPct: number; widthPct: number } | null {
  const s = Date.parse(entry.start);
  const e = Date.parse(entry.end);
  if (!Number.isFinite(s) || !Number.isFinite(e)) return null;
  const span = win.endMs - win.startMs;
  const from = Math.max(s, win.startMs);
  const to = Math.min(Math.max(e, from), win.endMs);
  const leftPct = ((from - win.startMs) / span) * 100;
  // A zero-length programme would otherwise be invisible; a hairline keeps it findable
  // without implying a duration it does not have.
  const widthPct = Math.max(((to - from) / span) * 100, 0.4);
  return { leftPct, widthPct };
}

/** Group guide entries under the channels they name. Channels with no programming keep an
 *  EMPTY row (an existing channel with a bare schedule is a real, reportable state), and a
 *  guide entry naming an unknown channel gets its own row labelled by raw id rather than
 *  being dropped. */
export function buildRows(channels: MuseChannel[], entries: MuseGuideEntry[]): GridRow[] {
  const byChannel = new Map<string, MuseGuideEntry[]>();
  for (const e of entries) {
    const list = byChannel.get(e.channel_id);
    if (list) list.push(e);
    else byChannel.set(e.channel_id, [e]);
  }
  const rows: GridRow[] = channels.map(c => ({
    key: c.id,
    label: c.name,
    channel: c,
    entries: byChannel.get(c.id) ?? [],
  }));
  const known = new Set(channels.map(c => c.id));
  for (const [channelId, list] of byChannel) {
    if (!known.has(channelId)) {
      rows.push({ key: `unlisted:${channelId}`, label: channelId, channel: null, entries: list });
    }
  }
  return rows;
}

function hhmm(ms: number): string {
  return new Date(ms).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
}

const LABEL_COL = 168;

function TimeAxis({ win }: { win: TimeWindow }) {
  const span = win.endMs - win.startMs;
  return (
    <div style={{ display: 'flex', alignItems: 'stretch', borderBottom: '1px solid var(--border)' }}>
      <div style={{ width: LABEL_COL, flex: `0 0 ${LABEL_COL}px` }} />
      <div style={{ position: 'relative', flex: 1, height: 20 }}>
        {win.ticks.map(t => (
          <span
            key={t}
            style={{
              position: 'absolute',
              left: `${((t - win.startMs) / span) * 100}%`,
              transform: 'translateX(-50%)',
              fontFamily: 'var(--font-mono)',
              fontSize: 'var(--fs-2xs, 10px)',
              color: 'var(--text-400, var(--text-300))',
              whiteSpace: 'nowrap',
            }}
          >
            {hhmm(t)}
          </span>
        ))}
      </div>
    </div>
  );
}

function ChannelRow({ row, win }: { row: GridRow; win: TimeWindow }) {
  return (
    <div style={{ display: 'flex', alignItems: 'stretch', borderBottom: '1px solid var(--border-subtle, rgba(255,255,255,0.05))' }}>
      <div
        style={{
          width: LABEL_COL,
          flex: `0 0 ${LABEL_COL}px`,
          padding: '6px var(--space-2) 6px 0',
          minWidth: 0,
        }}
      >
        <div
          style={{
            fontSize: 'var(--fs-xs)',
            color: 'var(--text-100)',
            overflow: 'hidden',
            textOverflow: 'ellipsis',
            whiteSpace: 'nowrap',
          }}
          title={row.label}
        >
          {row.label}
        </div>
        <div style={{ fontSize: 'var(--fs-2xs, 10px)', fontFamily: 'var(--font-mono)', color: 'var(--text-400, var(--text-300))' }}>
          {/* `item_count` is a real channel field. The unlisted-channel row has no channel
              record at all, so it says so instead of showing a zero it cannot justify. */}
          {row.channel ? `${row.channel.item_count} items` : 'not in channel list'}
        </div>
      </div>
      <div style={{ position: 'relative', flex: 1, minHeight: 34, padding: '4px 0' }}>
        {row.entries.length === 0 && (
          <span
            style={{
              position: 'absolute',
              left: 0,
              top: '50%',
              transform: 'translateY(-50%)',
              fontSize: 'var(--fs-2xs, 10px)',
              fontFamily: 'var(--font-mono)',
              color: 'var(--text-400, var(--text-300))',
            }}
          >
            no scheduled programming
          </span>
        )}
        {row.entries.map((e, i) => {
          const geo = blockGeometry(e, win);
          if (geo === null) return null;
          return (
            <div
              key={`${e.channel_id}-${e.start}-${i}`}
              title={`${e.title} · ${hhmm(Date.parse(e.start))}–${hhmm(Date.parse(e.end))}`}
              style={{
                position: 'absolute',
                left: `${geo.leftPct}%`,
                width: `${geo.widthPct}%`,
                top: 4,
                bottom: 4,
                // Single neutral tone on purpose — see the file header on directional-tone
                // coding: guide entries carry no categorical field to colour by.
                background: 'var(--accent-dim, rgba(139,92,246,0.18))',
                border: '1px solid var(--accent, #8b5cf6)',
                borderRadius: 'var(--radius-xs, 3px)',
                padding: '2px 5px',
                fontSize: 'var(--fs-2xs, 10px)',
                color: 'var(--text-100)',
                overflow: 'hidden',
                textOverflow: 'ellipsis',
                whiteSpace: 'nowrap',
                boxSizing: 'border-box',
              }}
            >
              {e.title}
            </div>
          );
        })}
      </div>
    </div>
  );
}

/** The vertical "now" rule. Rendered ONLY when there is a window AND now falls inside it —
 *  a now marker on an empty or non-overlapping grid would be a position with no meaning. */
function NowMarker({ win, nowMs }: { win: TimeWindow; nowMs: number }) {
  if (nowMs < win.startMs || nowMs > win.endMs) return null;
  const pct = ((nowMs - win.startMs) / (win.endMs - win.startMs)) * 100;
  return (
    <div
      aria-hidden
      style={{
        position: 'absolute',
        left: `calc(${LABEL_COL}px + (100% - ${LABEL_COL}px) * ${pct / 100})`,
        top: 0,
        bottom: 0,
        width: 1,
        background: 'var(--danger, #ff5a5a)',
        pointerEvents: 'none',
      }}
    />
  );
}

/** Guide screen 09's footer telemetry line. Every value comes from `/discover.json`; the
 *  advertised-lineup count comes from `/lineup.json`. Both degrade on their own — a dark
 *  tuner must not blank the grid above it. */
function TunerTelemetry({ nowMs }: { nowMs: number }) {
  const tuner = useMuseTuner();
  const lineup = useMuseTunerLineup();
  const d = tuner.data;

  const parts: string[] = [`now · ${hhmm(nowMs)}`];
  if (tuner.loading) {
    parts.push('tuner…');
  } else if (tuner.degraded || d === null) {
    parts.push(`tuner telemetry unavailable (${tuner.degraded ? tuner.degraded.detail : 'no response'})`);
  } else {
    parts.push(`${d.DeviceID} · ${d.FriendlyName} ${d.FirmwareName} ${d.FirmwareVersion}`);
    // Declared capacity, never occupancy — see the file header.
    parts.push(`${d.TunerCount} tuners advertised`);
    parts.push('advertising /discover.json');
  }
  if (!lineup.loading && !lineup.degraded && Array.isArray(lineup.data)) {
    parts.push(`lineup.json: ${lineup.data.length} channels`);
  }

  return (
    <div style={{ fontFamily: 'var(--font-mono)', fontSize: 'var(--fs-2xs, 10px)', color: 'var(--text-400, var(--text-300))' }}>
      {parts.join(' · ')}
    </div>
  );
}

export interface ProgrammingGridProps {
  /** Injected so the now marker and footer clock are deterministic under test. */
  nowMs?: number;
}

/**
 * The grid body. Rendered inside `ChannelsPanel`'s Guide `ChartCard`, so loading/degraded/
 * empty chrome is the CARD's job — this component owns the layout and the honest empty copy.
 */
export function ProgrammingGrid({ nowMs = Date.now() }: ProgrammingGridProps) {
  const channelsSection = useMuseChannels();
  const guideSection = useMuseGuide();

  const channels = museChannelList(channelsSection.data);
  const { entries, htmlOnly } = museGuideEntries(guideSection.data);

  const win = useMemo(() => deriveWindow(entries, nowMs), [entries, nowMs]);
  const rows = useMemo(() => buildRows(channels, entries), [channels, entries]);

  if (rows.length === 0) {
    return (
      <div
        style={{
          display: 'flex',
          flexDirection: 'column',
          gap: 'var(--space-2)',
          height: '100%',
          justifyContent: 'center',
          padding: 'var(--space-3)',
          fontSize: 'var(--fs-xs)',
          color: 'var(--text-300)',
        }}
      >
        <div style={{ color: 'var(--text-200)' }}>No channels to lay out.</div>
        {/* Observable facts only. No claim about WHY there are no channels beyond the one
            verified structural fact (the unmounted route) — see the file header. */}
        <div>
          <code style={{ fontFamily: 'var(--font-mono)' }}>GET /api/channels</code> returned an empty list.
        </div>
        <div>
          Channel creation (<code style={{ fontFamily: 'var(--font-mono)' }}>channels.compose</code>) is
          implemented and tested but has no HTTP route mounted, so no channel can be created from here.
        </div>
        {htmlOnly && (
          <div>
            <code style={{ fontFamily: 'var(--font-mono)' }}>GET /guide</code> serves Muse&apos;s HTML guide
            page, not a structured programme feed — this grid does not scrape it, so it draws no blocks
            from it.
          </div>
        )}
        <TunerTelemetry nowMs={nowMs} />
      </div>
    );
  }

  return (
    <div style={{ display: 'flex', flexDirection: 'column', gap: 'var(--space-2)', height: '100%', minHeight: 0 }}>
      {htmlOnly && (
        <div style={{ fontSize: 'var(--fs-2xs, 10px)', color: 'var(--text-400, var(--text-300))' }}>
          <code style={{ fontFamily: 'var(--font-mono)' }}>/guide</code> returned an HTML page rather than a
          programme feed — channel rows are shown, programme blocks are not available.
        </div>
      )}
      {/* Scrollable in its own container, and focusable: a scroll region a keyboard cannot
          reach is a pointer-only control. */}
      <div
        role="region"
        aria-label="Channel programming grid"
        tabIndex={0}
        style={{ position: 'relative', flex: 1, minHeight: 0, overflow: 'auto' }}
      >
        {win !== null && <TimeAxis win={win} />}
        {rows.map(r =>
          win !== null ? (
            <ChannelRow key={r.key} row={r} win={win} />
          ) : (
            // No parseable programme times anywhere => no axis, so a proportional row would
            // be meaningless. The channel is still listed; the absence is stated.
            <div
              key={r.key}
              style={{
                display: 'flex',
                gap: 'var(--space-2)',
                padding: '6px 0',
                fontSize: 'var(--fs-xs)',
                color: 'var(--text-100)',
                borderBottom: '1px solid var(--border-subtle, rgba(255,255,255,0.05))',
              }}
            >
              <span style={{ width: LABEL_COL, flex: `0 0 ${LABEL_COL}px` }}>{r.label}</span>
              <span style={{ fontFamily: 'var(--font-mono)', fontSize: 'var(--fs-2xs, 10px)', color: 'var(--text-400, var(--text-300))' }}>
                no scheduled programming
              </span>
            </div>
          ),
        )}
        {win !== null && <NowMarker win={win} nowMs={nowMs} />}
      </div>
      <TunerTelemetry nowMs={nowMs} />
    </div>
  );
}
