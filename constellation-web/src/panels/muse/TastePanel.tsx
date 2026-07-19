// CONST-20: muse.taste -- taste-cluster scatter, watch-history stacked area, group dynamics
// table (spec §5.4). All three read-only. Same independent-per-section degrade boundary as
// DashboardPanel (see that file's top comment and `hooks/useMuse.ts`) -- one dead endpoint
// degrades only its own ChartCard.
//
// Cluster fold rule (§4.2/§5.4): the taste-cluster scatter is an all-pairs form, capped at 4
// series -- clusters beyond the first 4 (first-seen order, i.e. array order from the backend)
// are merged into a single "Other" bucket rather than getting their own color, exactly the
// `ALL_PAIRS_CEILING`/`exceedsAllPairsCap` rule in `viz/palette.ts`. `MOCK_MUSE_TASTE_CLUSTERS`
// ships 5 clusters specifically so this fold is exercised by default, not only provable by
// editing a mock.
import { useMemo } from 'react';
import { ChartCard } from '../../viz/ChartCard';
import { ChartLegend } from '../../viz/ChartLegend';
import { ChartTooltip } from '../../viz/ChartTooltip';
import { DataTable } from '../../components/DataTable';
import type { DataTableColumn } from '../../components/DataTable';
import { TableView, TableViewControls, useTableView } from '../../viz/TableViewToggle';
import { ScatterChart, Scatter, XAxis, YAxis, CartesianGrid, Tooltip, ResponsiveContainer, AreaChart, Area } from '../../viz/recharts';
import { rechartsGridProps, rechartsTickStyle } from '../../viz/theme';
import { CATEGORICAL_HEX, CHART_CHROME, ALL_PAIRS_CEILING, SlotAssigner } from '../../viz/palette';
import {
  useMuseTasteClusters,
  useMuseWatchHistory,
  useMuseGroupDynamics,
  type MuseTasteCluster,
  type MuseGroupDynamicsRow,
} from '../../hooks/useMuse';

const OTHER_LABEL = 'Other';

interface FoldedCluster {
  label: string;
  color: string;
  points: Array<{ x: number; y: number; model: string }>;
}

/** §4.2 all-pairs fold: first 4 clusters (array/first-seen order) keep their own categorical
 *  slot; anything beyond that is merged into one deemphasized "Other" series. */
function foldClusters(clusters: MuseTasteCluster[]): FoldedCluster[] {
  const kept = clusters.slice(0, ALL_PAIRS_CEILING).map((c, i) => ({
    label: c.label,
    color: CATEGORICAL_HEX[i],
    points: c.points,
  }));
  const overflow = clusters.slice(ALL_PAIRS_CEILING);
  if (overflow.length > 0) {
    kept.push({
      label: OTHER_LABEL,
      color: CHART_CHROME.deemphasis,
      points: overflow.flatMap(c => c.points),
    });
  }
  return kept;
}

interface TasteTableRow {
  cluster: string;
  x: number;
  y: number;
  model: string;
}

const TASTE_COLUMNS: DataTableColumn<TasteTableRow>[] = [
  { key: 'cluster', header: 'Cluster', render: r => r.cluster },
  { key: 'model', header: 'Item', render: r => r.model },
  { key: 'x', header: 'X', align: 'right', render: r => r.x.toFixed(2) },
  { key: 'y', header: 'Y', align: 'right', render: r => r.y.toFixed(2) },
];

function TasteClusterSection() {
  const { data, loading, degraded } = useMuseTasteClusters();
  const { view, setView } = useTableView();
  const folded = useMemo(() => foldClusters(data?.clusters ?? []), [data]);
  const rows: TasteTableRow[] = useMemo(
    () => folded.flatMap(c => c.points.map(p => ({ cluster: c.label, x: p.x, y: p.y, model: p.model }))),
    [folded],
  );
  const empty = !loading && !degraded && rows.length === 0;
  const tick = rechartsTickStyle();

  return (
    <ChartCard
      title="Taste Clusters"
      subtitle="Viewing taste, clustered (first 4 shown; rest folded to Other)"
      controls={<TableViewControls view={view} onChange={setView} />}
      height={280}
      loading={loading}
      degraded={degraded}
      empty={empty}
      emptyMessage="No taste data yet"
      emptyHint="Clusters build up as Muse's audience watches more"
      footer={<ChartLegend entries={folded.map(c => ({ id: c.label, label: c.label, color: c.color }))} />}
    >
      <TableView view={view} columns={TASTE_COLUMNS} rows={rows} rowKey={(r, i) => `${r.cluster}-${i}`}>
        <ResponsiveContainer width="100%" height={280}>
          <ScatterChart margin={{ top: 8, right: 8, bottom: 8, left: 8 }}>
            <CartesianGrid {...rechartsGridProps()} />
            <XAxis type="number" dataKey="x" name="x" tick={tick} />
            <YAxis type="number" dataKey="y" name="y" tick={tick} />
            <Tooltip
              cursor={{ strokeDasharray: undefined }}
              content={({ active, payload }) => {
                if (!active || !payload?.length) return null;
                const p = payload[0]?.payload as { x: number; y: number; model: string } | undefined;
                if (!p) return null;
                return (
                  <ChartTooltip
                    title={p.model}
                    rows={[
                      { key: 'x', label: 'x', value: p.x.toFixed(2) },
                      { key: 'y', label: 'y', value: p.y.toFixed(2) },
                    ]}
                  />
                );
              }}
            />
            {folded.map(c => (
              <Scatter key={c.label} name={c.label} data={c.points} fill={c.color} />
            ))}
          </ScatterChart>
        </ResponsiveContainer>
      </TableView>
    </ChartCard>
  );
}

interface WatchHistoryTableRow {
  date: string;
  [key: string]: string | number;
}

function WatchHistorySection() {
  const { data, loading, degraded } = useMuseWatchHistory();
  const { view, setView } = useTableView();
  const rows = data?.series ?? [];
  const seriesKeys = useMemo(() => {
    const keys = new Set<string>();
    rows.forEach(r => Object.keys(r).forEach(k => { if (k !== 'date') keys.add(k); }));
    return Array.from(keys);
  }, [rows]);
  const slots = useMemo(() => {
    const assigner = new SlotAssigner();
    const colors: Record<string, string> = {};
    seriesKeys.forEach(k => { colors[k] = assigner.colorFor(k); });
    return colors;
  }, [seriesKeys]);
  const empty = !loading && !degraded && rows.length === 0;
  const tick = rechartsTickStyle();

  const tableColumns: DataTableColumn<WatchHistoryTableRow>[] = [
    { key: 'date', header: 'Date', render: r => String(r.date) },
    ...seriesKeys.map(k => ({ key: k, header: k, align: 'right' as const, render: (r: WatchHistoryTableRow) => String(r[k] ?? 0) })),
  ];

  return (
    <ChartCard
      title="Watch History"
      subtitle="Sessions per cluster over time"
      controls={<TableViewControls view={view} onChange={setView} />}
      height={240}
      loading={loading}
      degraded={degraded}
      empty={empty}
      emptyMessage="No watch history yet"
      emptyHint="Session activity by taste cluster will chart here"
      footer={<ChartLegend entries={seriesKeys.map(k => ({ id: k, label: k, color: slots[k] }))} />}
    >
      <TableView view={view} columns={tableColumns} rows={rows as WatchHistoryTableRow[]} rowKey={(r, i) => `${r.date}-${i}`}>
        <ResponsiveContainer width="100%" height={240}>
          <AreaChart data={rows}>
            <CartesianGrid {...rechartsGridProps()} />
            <XAxis dataKey="date" tick={tick} />
            <YAxis tick={tick} />
            <Tooltip
              content={({ active, payload, label }) => {
                if (!active || !payload?.length) return null;
                return (
                  <ChartTooltip
                    title={String(label)}
                    rows={payload.map(p => ({
                      key: String(p.dataKey),
                      label: String(p.dataKey),
                      value: String(p.value),
                      color: typeof p.color === 'string' ? p.color : undefined,
                    }))}
                  />
                );
              }}
            />
            {seriesKeys.map(k => (
              <Area key={k} type="monotone" dataKey={k} stackId="1" stroke={slots[k]} fill={slots[k]} fillOpacity={0.35} />
            ))}
          </AreaChart>
        </ResponsiveContainer>
      </TableView>
    </ChartCard>
  );
}

const GROUP_DYNAMICS_COLUMNS: DataTableColumn<MuseGroupDynamicsRow>[] = [
  { key: 'participant', header: 'Participant', render: r => r.participant },
  { key: 'genre', header: 'Favorite Genre', render: r => r.favorite_genre },
  { key: 'together', header: 'Watched Together', align: 'right', render: r => `${r.watched_together_pct}%` },
  { key: 'sessions', header: 'Sessions', align: 'right', render: r => String(r.sessions) },
];

function GroupDynamicsSection() {
  const { data, loading, degraded } = useMuseGroupDynamics();
  const rows = data?.rows ?? [];
  const empty = !loading && !degraded && rows.length === 0;
  return (
    <ChartCard
      title="Group Dynamics"
      height={220}
      loading={loading}
      degraded={degraded}
      empty={empty}
      emptyMessage="No group data yet"
      emptyHint="Shared-viewing patterns across participants will list here"
    >
      <DataTable columns={GROUP_DYNAMICS_COLUMNS} rows={rows} rowKey={r => r.participant} emptyMessage="No group data yet" />
    </ChartCard>
  );
}

export function TastePanel() {
  return (
    <div style={{ padding: 'var(--space-5)', display: 'flex', flexDirection: 'column', gap: 'var(--space-4)' }}>
      <TasteClusterSection />
      <WatchHistorySection />
      <GroupDynamicsSection />
    </div>
  );
}
