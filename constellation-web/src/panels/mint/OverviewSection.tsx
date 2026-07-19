// CONST-23 §7.3: Overview = C0 (stat tiles) + C8 (sweep activity). C0 tiles deep-link into the
// other sections (§7.2) via the page's in-page anchors (see MintPage's sticky section nav).
import { useState } from 'react';
import { MetricCard } from '../../components/MetricCard';
import { ChartCard } from '../../viz/ChartCard';
import { AreaChart, Area, XAxis, YAxis, CartesianGrid, Tooltip, ResponsiveContainer, Legend, ReferenceLine } from '../../viz/recharts';
import { rechartsGridProps, rechartsTickStyle, rechartsTooltipStyle } from '../../viz/theme';
import { CATEGORICAL_HEX } from '../../viz/palette';
import { TableView, TableViewControls, useTableView } from '../../viz/TableViewToggle';
import type { DataTableColumn } from '../../components/DataTable';
import { useMintSummary, useMintActivity } from '../../hooks/useMint';
import type { MintFilters } from '../../hooks/useMint';
import type { MintActivityDay } from '../../lib/aggregationClient';
import { mintSectionTitleStyle } from './mintShared';
const CHART_HEIGHT = 260;

function fmtHours(h: number): string {
  return `${h.toFixed(1)}h`;
}

export function OverviewSection({ filters }: { filters: MintFilters }) {
  const summary = useMintSummary(filters);
  const [range, setRange] = useState<'30d' | '90d' | 'all'>('90d');
  const activity = useMintActivity(filters, range);
  const { view, setView } = useTableView();

  const s = summary.data;

  const columns: DataTableColumn<MintActivityDay>[] = [
    { key: 'date', header: 'Date', render: r => r.date },
    { key: 'code', header: 'Code', align: 'right', render: r => String(r.code) },
    { key: 'context', header: 'Context', align: 'right', render: r => String(r.context) },
    { key: 'agent', header: 'Agent', align: 'right', render: r => String(r.agent) },
  ];

  const days = activity.data?.days ?? [];
  const epochs = activity.data?.epochs ?? [];

  return (
    <section id="overview" style={{ scrollMarginTop: 64 }}>
      <h3 style={mintSectionTitleStyle}>Overview</h3>

      <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(160px, 1fr))', gap: 'var(--space-3)', marginBottom: 'var(--space-4)' }}>
        <a href="#coverage" style={tileLinkStyle}>
          <MetricCard label="Models profiled" value={summary.loading ? '…' : String(s?.models_profiled ?? 0)} />
        </a>
        <a href="#coverage" style={tileLinkStyle}>
          <MetricCard label="Runs this epoch" value={summary.loading ? '…' : String(s?.runs_this_epoch ?? 0)} />
        </a>
        <a href="#capability" style={tileLinkStyle}>
          <MetricCard
            label="Fleet-best (pass@3)"
            value={summary.loading ? '…' : s?.fleet_best ? `${s.fleet_best.model} · ${Math.round(s.fleet_best.pass_hat_3 * 100)}%` : '—'}
            valueColor="accent"
          />
        </a>
        <a href="#context" style={tileLinkStyle}>
          <MetricCard label="GPU-hours" value={summary.loading ? '…' : fmtHours(s?.gpu_hours ?? 0)} />
        </a>
        <a href="#overview" style={tileLinkStyle}>
          <MetricCard label="Current epoch" value={summary.loading ? '…' : s?.epoch ?? '—'} />
        </a>
      </div>

      <ChartCard
        title="Sweep activity"
        subtitle="runs/day by suite"
        height={CHART_HEIGHT}
        loading={activity.loading && !activity.data}
        isRefetching={activity.loading && !!activity.data}
        degraded={activity.degraded}
        empty={!activity.loading && days.length === 0}
        emptyMessage="No sweep activity in this range"
        controls={
          <div style={{ display: 'flex', gap: 8, alignItems: 'center' }}>
            {(['30d', '90d', 'all'] as const).map(r => (
              <button
                key={r}
                type="button"
                onClick={() => setRange(r)}
                aria-pressed={range === r}
                style={{
                  fontFamily: 'var(--font-mono)', fontSize: 'var(--fs-mono-sm)', textTransform: 'uppercase',
                  letterSpacing: 'var(--ls-label)', padding: '3px 10px', borderRadius: 'var(--radius-xs)',
                  border: 'none', cursor: 'pointer',
                  background: range === r ? 'var(--grad-accent)' : 'transparent',
                  color: range === r ? 'var(--accent-on)' : 'var(--text-muted)',
                }}
              >
                {r}
              </button>
            ))}
            <TableViewControls view={view} onChange={setView} />
          </div>
        }
      >
        <TableView view={view} columns={columns} rows={days} rowKey={(r, i) => `${r.date}-${i}`}>
          <ResponsiveContainer width="100%" height={CHART_HEIGHT}>
            <AreaChart data={days}>
              <CartesianGrid {...rechartsGridProps()} />
              <XAxis dataKey="date" tick={rechartsTickStyle()} />
              <YAxis tick={rechartsTickStyle()} />
              <Tooltip contentStyle={rechartsTooltipStyle()} />
              <Legend wrapperStyle={{ fontSize: 11 }} />
              <Area type="monotone" dataKey="code" name="Code" stackId="suite" stroke={CATEGORICAL_HEX[0]} fill={CATEGORICAL_HEX[0]} fillOpacity={0.1} strokeWidth={2} />
              <Area type="monotone" dataKey="context" name="Context" stackId="suite" stroke={CATEGORICAL_HEX[1]} fill={CATEGORICAL_HEX[1]} fillOpacity={0.1} strokeWidth={2} />
              <Area type="monotone" dataKey="agent" name="Agent" stackId="suite" stroke={CATEGORICAL_HEX[2]} fill={CATEGORICAL_HEX[2]} fillOpacity={0.1} strokeWidth={2} />
              {epochs.map(e => (
                <ReferenceLine key={e.epoch} x={e.date} stroke="var(--chart-axis)" strokeWidth={1} label={{ value: e.label, position: 'insideTopRight', fill: 'var(--text-muted)', fontSize: 10 }} />
              ))}
            </AreaChart>
          </ResponsiveContainer>
        </TableView>
      </ChartCard>
    </section>
  );
}

const tileLinkStyle: React.CSSProperties = { textDecoration: 'none', display: 'block' };
