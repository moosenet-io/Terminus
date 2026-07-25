// CGUI-10 (TERM #533): MINT — Overview. The cross-category summary that fronts the module:
// fleet-wide MINT headline metrics, profiling activity over time, and a coverage roll-up across
// EVERY category (how many models are profiled, and the current best) — all live DB reads via
// the CGUI-08 client. The per-category deep-dive reports live in the Category Reports panel.
//
// The coverage roll-up fans out one `mint.categorySummary` read per new task-category (fail-open
// per category — an un-profiled category simply shows 0 models, never an error), and reads the
// legacy suite run counts from `mint.summary`. Tokens only; charts from the viz kit.
import { useEffect, useMemo, useState } from 'react';
import { PanelRoot } from '../../components/PanelRoot';
import { CardTitle } from '../../components/Card';
import { MetricCard } from '../../components/MetricCard';
import { DataTable } from '../../components/DataTable';
import type { DataTableColumn } from '../../components/DataTable';
import { ChartCard } from '../../viz/ChartCard';
import { ChartLegend } from '../../viz/ChartLegend';
import { ChartTooltip } from '../../viz/ChartTooltip';
import { TableView, TableViewControls, useTableView } from '../../viz/TableViewToggle';
import {
  BarChart, Bar, XAxis, YAxis, CartesianGrid, Tooltip, ResponsiveContainer, Cell,
} from '../../viz/recharts';
import { rechartsGridProps, rechartsTickStyle } from '../../viz/theme';
import { CATEGORICAL_HEX, CHART_CHROME, SlotAssigner } from '../../viz/palette';
import { getAggregationClient } from '../../lib/aggregationClient';
import { useMintSection } from '../../hooks/useMint';
import { MINT_CATEGORY_META, metricLabel, formatMetricValue } from './categoryMeta';
import { rankingFromCategory, metricsOfCategory } from './transforms';
import type { MintActivityResponse } from '../../types/mint';

interface CoverageRow {
  id: string;
  label: string;
  kind: string;
  models: number;
  bestModel: string | null;
  primaryMetric: string | null;
  bestValue: number | null;
}

function useCategoryCoverage() {
  const [rows, setRows] = useState<CoverageRow[] | null>(null);

  useEffect(() => {
    let cancelled = false;
    const client = getAggregationClient();
    const newcats = MINT_CATEGORY_META.filter(c => c.kind === 'newcat');

    Promise.all(
      newcats.map(async c => {
        try {
          const summary = await client.mint.categorySummary(c.clientKey!);
          const primaryMetric = metricsOfCategory(summary)[0] ?? null;
          const ranked = rankingFromCategory(summary, primaryMetric);
          const best = ranked[0] ?? null;
          return {
            id: c.id, label: c.label, kind: c.kind,
            models: summary.models.length,
            bestModel: best?.model ?? null,
            primaryMetric,
            bestValue: best?.value ?? null,
          } as CoverageRow;
        } catch {
          // fail-open: an un-reachable/erroring category contributes a zero row, never throws.
          return { id: c.id, label: c.label, kind: c.kind, models: 0, bestModel: null, primaryMetric: null, bestValue: null } as CoverageRow;
        }
      }),
    ).then(res => { if (!cancelled) setRows(res); });

    return () => { cancelled = true; };
  }, []);

  return rows;
}

function HeadlineRow() {
  const summary = useMintSection(c => c.mint.summary(), 'ov-summary');
  const s = summary.data;
  return (
    <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(150px, 1fr))', gap: 'var(--space-3)' }}>
      <MetricCard label="Models Profiled" value={s ? String(s.models_profiled) : '—'} valueColor="accent" />
      <MetricCard label="Total Runs" value={s ? s.runs.total.toLocaleString() : '—'} />
      <MetricCard label="GPU Hours" value={s ? s.gpu_hours.toLocaleString() : '—'} />
      <MetricCard label="Current Epoch" value={s ? s.epoch : '—'} valueColor="secondary" />
      <MetricCard label="Fleet Best" value={s?.fleet_best_model ? `${(s.fleet_best_model.pass_hat_3 * 100).toFixed(0)}%` : '—'} valueColor="success" />
    </div>
  );
}

function ActivitySection() {
  const activity = useMintSection<MintActivityResponse>(c => c.mint.activity('30d'), 'ov-activity');
  const { view, setView } = useTableView();
  const days = activity.data?.days ?? [];
  const empty = !activity.loading && !activity.degraded && days.length === 0;
  const tick = rechartsTickStyle();
  const series = ['code', 'context', 'agent'] as const;
  const colors: Record<string, string> = { code: CATEGORICAL_HEX[0], context: CATEGORICAL_HEX[1], agent: CATEGORICAL_HEX[2] };

  const columns: DataTableColumn<{ date: string; code: number; context: number; agent: number }>[] = [
    { key: 'date', header: 'Date', render: r => r.date },
    { key: 'code', header: 'Code', align: 'right', render: r => String(r.code) },
    { key: 'context', header: 'Context', align: 'right', render: r => String(r.context) },
    { key: 'agent', header: 'Agent', align: 'right', render: r => String(r.agent) },
  ];

  return (
    <ChartCard
      title="Profiling Activity"
      subtitle="Runs per day by legacy suite (last 30 days)"
      controls={<TableViewControls view={view} onChange={setView} />}
      height={240}
      loading={activity.loading}
      degraded={activity.degraded}
      empty={empty}
      emptyMessage="No recent profiling activity"
      emptyHint="Daily run counts chart here as MINT profiles the fleet"
      footer={<ChartLegend entries={series.map(k => ({ id: k, label: k, color: colors[k] }))} />}
    >
      <TableView view={view} columns={columns} rows={days} rowKey={r => r.date}>
        <ResponsiveContainer width="100%" height={240}>
          <BarChart data={days} margin={{ top: 8, right: 8, bottom: 4, left: 4 }}>
            <CartesianGrid {...rechartsGridProps()} vertical={false} />
            <XAxis dataKey="date" tick={tick} minTickGap={24} />
            <YAxis tick={tick} />
            <Tooltip
              cursor={{ fill: 'var(--accent-soft)' }}
              content={({ active, payload, label }) => {
                if (!active || !payload?.length) return null;
                return (
                  <ChartTooltip
                    title={String(label)}
                    rows={payload.map(p => ({ key: String(p.dataKey), label: String(p.dataKey), value: String(p.value), color: typeof p.color === 'string' ? p.color : undefined }))}
                  />
                );
              }}
            />
            {series.map(k => <Bar key={k} dataKey={k} stackId="a" fill={colors[k]} isAnimationActive={false} />)}
          </BarChart>
        </ResponsiveContainer>
      </TableView>
    </ChartCard>
  );
}

function CoverageSection() {
  const rows = useCategoryCoverage();
  const { view, setView } = useTableView();
  const loading = rows === null;
  const data = rows ?? [];
  const empty = !loading && data.every(r => r.models === 0);
  const tick = rechartsTickStyle();

  const slots = useMemo(() => {
    const assigner = new SlotAssigner();
    const map: Record<string, string> = {};
    data.forEach(r => { map[r.id] = assigner.colorFor(r.id); });
    return map;
  }, [data]);

  const columns: DataTableColumn<CoverageRow>[] = [
    { key: 'label', header: 'Category', render: r => r.label },
    { key: 'models', header: 'Models', align: 'right', render: r => String(r.models) },
    { key: 'best', header: 'Best Model', render: r => r.bestModel ?? '—' },
    { key: 'metric', header: 'Primary Metric', render: r => (r.primaryMetric ? metricLabel(r.primaryMetric) : '—') },
    { key: 'value', header: 'Best Value', align: 'right', render: r => formatMetricValue(r.primaryMetric ?? '', r.bestValue) },
  ];

  const chartRows = [...data].sort((a, b) => b.models - a.models);

  return (
    <ChartCard
      title="Category Coverage"
      subtitle="Models profiled per new task-category, with the current leader"
      controls={<TableViewControls view={view} onChange={setView} />}
      height={Math.max(220, chartRows.length * 34 + 48)}
      loading={loading}
      empty={empty}
      emptyMessage="No category profiling yet"
      emptyHint="Each new-category bar grows as models are profiled on it"
    >
      <TableView view={view} columns={columns} rows={data} rowKey={r => r.id}>
        <ResponsiveContainer width="100%" height={Math.max(200, chartRows.length * 34 + 30)}>
          <BarChart data={chartRows} layout="vertical" margin={{ top: 4, right: 32, bottom: 4, left: 8 }}>
            <CartesianGrid {...rechartsGridProps()} horizontal={false} />
            <XAxis type="number" allowDecimals={false} tick={tick} />
            <YAxis type="category" dataKey="label" width={148} tick={tick} />
            <Tooltip
              cursor={{ fill: 'var(--accent-soft)' }}
              content={({ active, payload }) => {
                if (!active || !payload?.length) return null;
                const r = payload[0]?.payload as CoverageRow | undefined;
                if (!r) return null;
                return (
                  <ChartTooltip
                    title={r.label}
                    rows={[
                      { key: 'm', label: 'Models', value: String(r.models) },
                      { key: 'b', label: 'Best', value: r.bestModel ?? '—' },
                    ]}
                  />
                );
              }}
            />
            <Bar dataKey="models" radius={[0, 3, 3, 0]} isAnimationActive={false}>
              {chartRows.map(r => (
                <Cell key={r.id} fill={r.models > 0 ? (slots[r.id] ?? CATEGORICAL_HEX[0]) : CHART_CHROME.deemphasis} />
              ))}
            </Bar>
          </BarChart>
        </ResponsiveContainer>
      </TableView>
    </ChartCard>
  );
}

export function OverviewPanel() {
  return (
    <PanelRoot style={{ padding: 'var(--space-5)', display: 'flex', flexDirection: 'column', gap: 'var(--space-4)' }}>
      <CardTitle subtitle="Fleet benchmark overview across every MINT category — live profiling database">
        MINT — Overview
      </CardTitle>
      <HeadlineRow />
      <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(360px, 1fr))', gap: 'var(--space-4)' }}>
        <ActivitySection />
        <CoverageSection />
      </div>
    </PanelRoot>
  );
}
