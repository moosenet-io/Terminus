// MACT-06 (MUSE-126): the Maestro Activity tile row — the H1 (Muse-only) stat set. See the
// spec item's own reuse-vs-new accounting table: every tile here reads an EXISTING public
// endpoint (`/stats`, `/gaps`, `/api/subsystems`, `/health`), the shell's existing
// `GET /api/health`, or is derived client-side from MACT-01's live list. No new endpoint, no
// new worker, no new tracking table — this file only re-presents what already exists.
//
// `GET /metrics` is DELIBERATELY not touched here: it is Muse's Prometheus registry of exactly
// two recommend-engine counters (`muse_recommend_requests_total`,
// `muse_recommend_duration_seconds`, Muse `src/http/mod.rs::handle_metrics` /
// `src/metrics.rs`) and carries NO host CPU/RAM. Mining it for a host-stats tile would be
// inventing a data source the endpoint does not have — the exact failure this item's own brief
// calls out by name. Host CPU/RAM, transcodes-vs-cap and scratch headroom are H2 (MACT-11); in
// H1 they render `SeamTile` below, unconditionally, never a `0` and never a spinner.
//
// Every tile is a scalar stat (a count, a ratio, a short status phrase), never a time-series —
// so `MetricCard` is the right primitive throughout (per this item's own constraint) and
// `ChartCard` is correctly unused: there is no chart-shaped content in this row for its
// `degraded` prop to apply to. The equivalent honesty guarantee for a non-chart tile is
// `StatTile`'s own three-state render below (loading / degraded / value), built from
// `tileFormat.ts`'s `TileValueState` — same discipline as `ChartCard`'s `degraded` prop, just
// for a MetricCard-shaped surface instead of a chart-shaped one.
import { useCallback, useEffect, useState } from 'react';
import { MetricCard } from '../../components/MetricCard';
import type { StatusColor } from '../../components/Card';
import { getAggregationClient } from '../../lib/aggregationClient';
import type { HealthStatus } from '../../lib/aggregationClient';
import { useActivityFeedLive } from '../../hooks/useActivityFeedLive';
import {
  useMuseGaps,
  useMuseHealth,
  useMuseLiveSessions,
  useMuseStats,
  useMuseSubsystems,
} from '../../hooks/useMuse';
import {
  MAESTRO_SEAM_LABEL,
  formatCount,
  formatModuleHealth,
  formatMuseHealth,
  formatRelativeTimestamp,
  formatSubsystemWiring,
  tileStateFromSection,
  truncateDetail,
} from './tileFormat';
import type { TileTone, TileValueState } from './tileFormat';

// ── Terminus's own `GET /api/health` (App.tsx's shell-level poll) ───────────────────────────
//
// ActivityPanel is mounted standalone by the registry-driven router (no props threaded — same
// convention `useLumina`'s own `health` section fetch documents for the identical situation),
// so this tile fetches it itself rather than requiring a prop nobody else passes. `MuseSection`-
// shaped (`{data, loading, degraded}`) on purpose, so `tileStateFromSection` (built for the
// `useMuse*` hook contract) works unchanged here too.
function useTerminusHealth(): { data: HealthStatus[] | null; loading: boolean; degraded: { detail: string } | false } {
  const [data, setData] = useState<HealthStatus[] | null>(null);
  const [loading, setLoading] = useState(true);
  const [degraded, setDegraded] = useState<{ detail: string } | false>(false);

  const fetchOnce = useCallback(() => {
    setLoading(true);
    getAggregationClient()
      .health.list()
      .then(d => {
        setData(d);
        setDegraded(false);
        setLoading(false);
      })
      .catch(err => {
        setDegraded({ detail: err instanceof Error ? err.message : 'unknown error' });
        setData(null);
        setLoading(false);
      });
  }, []);

  useEffect(() => {
    fetchOnce();
  }, [fetchOnce]);

  return { data, loading, degraded };
}

// ── Presentation (pure props, no hooks — renderable via renderToStaticMarkup like
//    ActivityPanel.tsx's LivePane/HistoryPane/LiveSessionCard) ───────────────────────────────

function toneColor(tone: TileTone | undefined): StatusColor {
  switch (tone) {
    case 'success': return 'success';
    case 'warning': return 'warning';
    case 'tertiary': return 'tertiary';
    default: return 'primary';
  }
}

/** One H1 stat tile. Three visually-distinct renders, never conflated (the honesty rule this
 *  whole file exists to enforce):
 *   - loading  -> "…", tertiary (a render-in-progress placeholder, not a value)
 *   - degraded -> "—", WARNING tone, PLUS a short cause line rendered as real, visible card
 *                 content (not just an HTML `title` attribute — review finding, round 2: a
 *                 hover-only tooltip is invisible on touch and not reliably surfaced by
 *                 assistive tech, so colour alone was the only thing distinguishing "degraded"
 *                 from "not reported", which fails the three-states rule on its own terms).
 *                 `title` is still set too, for the untruncated detail on hover — a nice-to-
 *                 have now, never the sole carrier.
 *   - value    -> the formatted text verbatim, INCLUDING a genuine "0" (never re-dashed) */
export function StatTile({ label, state }: { label: string; state: TileValueState }) {
  if (state.kind === 'loading') {
    return <MetricCard label={label} value="…" valueColor="tertiary" />;
  }
  if (state.kind === 'degraded') {
    return (
      <div title={state.detail}>
        <MetricCard label={label} value="—" valueColor="warning" />
        <div
          style={{
            fontSize: 'var(--fs-xs)',
            color: 'var(--status-warning)',
            marginTop: 'var(--space-1)',
            overflowWrap: 'break-word',
          }}
        >
          {truncateDetail(state.detail)}
        </div>
      </div>
    );
  }
  return <MetricCard label={label} value={state.text} valueColor={toneColor(state.tone)} />;
}

/** An H2 (MACT-11) host/capacity placeholder. Deliberately takes NO data prop — there is no H1
 *  fetch for host CPU/RAM, transcodes-vs-cap, or scratch headroom to source one from, so there
 *  is no branch in this component that could ever substitute a `0` or a spinner for the seam
 *  text. That "no data path exists" property is what makes this mutation-proof: nothing to
 *  break loading/degraded logic for, because there is none. */
export function SeamTile({ label }: { label: string }) {
  return (
    <div title="H2 (MACT-11) fills this tile from a real Maestro backend; no such backend is deployed in H1.">
      <MetricCard label={label} value={MAESTRO_SEAM_LABEL} valueColor="tertiary" />
    </div>
  );
}

export interface TileRowStates {
  librarySize: TileValueState;
  pendingItems: TileValueState;
  lastIngest: TileValueState;
  gapsBacklog: TileValueState;
  subsystemWiring: TileValueState;
  moduleHealth: TileValueState;
  museHealth: TileValueState;
  liveStreams: TileValueState;
}

/** Pure render of the tile row from precomputed states — the thing `ActivityTiles.test.tsx`
 *  exercises directly (no hooks, no fetch), same split as `LivePane`/`HistoryPane` taking
 *  plain props while `ActivityPanel` owns the data fetching. */
export function TileRow({ states }: { states: TileRowStates }) {
  return (
    <div
      style={{
        display: 'grid',
        gridTemplateColumns: 'repeat(auto-fill, minmax(11rem, 1fr))',
        gap: 'var(--space-3)',
      }}
    >
      <StatTile label="Library size" state={states.librarySize} />
      <StatTile label="Pending items" state={states.pendingItems} />
      <StatTile label="Last ingest" state={states.lastIngest} />
      <StatTile label="Gaps backlog" state={states.gapsBacklog} />
      <StatTile label="Subsystem wiring" state={states.subsystemWiring} />
      <StatTile label="Modules up" state={states.moduleHealth} />
      <StatTile label="Muse liveness" state={states.museHealth} />
      <StatTile label="Live streams" state={states.liveStreams} />
      {/* H2 (MACT-11) — inert seam, never a fabricated 0 and never a spinner. */}
      <SeamTile label="Host CPU / RAM" />
      <SeamTile label="Transcodes vs cap" />
      <SeamTile label="Scratch headroom" />
    </div>
  );
}

// ── Data (the only hook-bearing export in this file) ─────────────────────────────────────────

/** Wires the eight H1 tiles to their EXISTING sources — see this file's top comment for the
 *  full reuse-vs-new accounting. Each `tileStateFromSection` call is independent, so one
 *  degraded source (e.g. `/stats` down) never blanks a sibling tile fed by a healthy one
 *  (`/api/subsystems`) — per-tile degradation, matching this item's own EDGE CASES. */
export function ActivityTiles() {
  const stats = useMuseStats();
  const gaps = useMuseGaps();
  const subsystems = useMuseSubsystems();
  const museHealthSection = useMuseHealth();
  const terminusHealth = useTerminusHealth();
  // Same live-session hook `ActivityPanel`'s LIVE pane already calls — "derived client-side"
  // per the spec's own accounting, not a new source. Two independent call sites each fetching
  // once is the existing `useMuse*` convention (see ImportActivity.tsx re-fetching the same
  // `/api/requests/queue` `useMuseDownloadQueue` already binds), not a regression here.
  const live = useMuseLiveSessions();

  // MACT-08 (MUSE-128): the 'tiles' cadence (10s polling fallback; WS-tick-coalesced when
  // live) — one shared `useActivityFeedLive` call refetches every source feeding this row
  // together, since a stale stat tile is no less honest a signal than a stale live pane.
  // `terminusHealth` intentionally excluded: it is the shell's OWN `/api/health` poll, not a
  // Muse activity source the tick fans in for.
  useActivityFeedLive('tiles', () => {
    stats.refetch();
    gaps.refetch();
    subsystems.refetch();
    museHealthSection.refetch();
    live.refetch();
  });

  const states: TileRowStates = {
    librarySize: tileStateFromSection(stats, s => ({ text: formatCount(s.library_size) })),
    pendingItems: tileStateFromSection(stats, s => ({ text: formatCount(s.pending_items) })),
    lastIngest: tileStateFromSection(stats, s => ({ text: formatRelativeTimestamp(s.last_ingest_at) })),
    gapsBacklog: tileStateFromSection(gaps, g => ({ text: formatCount(g.total) })),
    subsystemWiring: tileStateFromSection(subsystems, d => formatSubsystemWiring(d.subsystems)),
    moduleHealth: tileStateFromSection(terminusHealth, formatModuleHealth),
    museHealth: tileStateFromSection(museHealthSection, formatMuseHealth),
    // A successful, empty live-session set is a FACT (a real 200 with zero rows) — `0` here is
    // exactly as real as a nonzero count, never re-dashed into "not reported" (EDGE CASES:
    // "Zero live streams -> 0 is a FACT here").
    liveStreams: tileStateFromSection(live, d => ({ text: formatCount(d.sessions.length) })),
  };

  return <TileRow states={states} />;
}
