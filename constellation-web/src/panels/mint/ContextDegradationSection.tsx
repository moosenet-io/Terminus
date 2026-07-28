// CGUI-10/CONST-23/24 reconciliation: ports CONST-23/24's context-degradation-over-tokens
// chart into the live Category Reports panel, wired to the real `client.mint.contextProfiles()`
// method (already typed and shipped — see types/mint.ts `MintContextProfilesResponse`). Main's
// generic legacy-suite transforms (radar/heatmap/box/ranking/failures, driven by `mint.matrix`/
// `mint.runs`) don't cover this: they show pass-rate style capability, not "how does throughput
// and recall degrade as context grows, and where does each model go OOM" — a genuinely
// additive view for the 'context' legacy category, only rendered when that category is
// selected. Shown as two sibling ChartCards (never a dual-axis chart, matching the module's
// existing chart-form conventions) beneath the standard per-category section grid.
import { useMemo } from 'react';
import { ChartCard } from './../../viz/ChartCard';
import {
  LineChart, Line, XAxis, YAxis, CartesianGrid, Tooltip, ResponsiveContainer, ReferenceLine, Scatter, ComposedChart,
} from '../../viz/recharts';
import { rechartsGridProps, rechartsTickStyle, rechartsTooltipStyle } from '../../viz/theme';
import { SlotAssigner } from '../../viz/palette';
import { ChartLegend } from '../../viz/ChartLegend';
import { TableView, TableViewControls, useTableView } from '../../viz/TableViewToggle';
import type { DataTableColumn } from '../../components/DataTable';
import { useMintSection } from '../../hooks/useMint';

const CHART_HEIGHT = 260;
const CONTEXT_TICKS = [2048, 4096, 8192, 16384, 32768, 65536];

interface PivotRow {
  context_tokens: number;
  [seriesKey: string]: number | null | undefined;
}

interface TableRow {
  model: string;
  context_tokens: number;
  throughput: number | null;
  recall_score: number | null;
  ttft_ms: number | null;
  memory_usage_mb: number | null;
  oom: boolean;
}

export function ContextDegradationSection() {
  const ctx = useMintSection(c => c.mint.contextProfiles(), 'context-degradation');
  const { view, setView } = useTableView();
  const slots = useMemo(() => new SlotAssigner(), []);

  // Up to 4 profiled models, same all-pairs ceiling the rest of the viz kit observes (§4.2).
  const profiles = useMemo(() => (ctx.data?.models ?? []).slice(0, 4), [ctx.data]);

  const { throughputRows, recallRows, tableRows, oomMarkers } = useMemo(() => {
    const throughputRows: PivotRow[] = CONTEXT_TICKS.map(context_tokens => ({ context_tokens }));
    const recallRows: PivotRow[] = CONTEXT_TICKS.map(context_tokens => ({ context_tokens }));
    const tableRows: TableRow[] = [];
    const oomMarkers: { model: string; context_tokens: number; y: number; color: string }[] = [];

    for (const p of profiles) {
      let lastGoodThroughput: number | null = null;
      p.tiers.forEach((tier, i) => {
        const tRow = throughputRows[i];
        const rRow = recallRows[i];
        if (tRow) tRow[p.model] = tier.throughput_tok_per_sec;
        if (rRow) rRow[p.model] = tier.recall_score;
        tableRows.push({
          model: p.model, context_tokens: tier.context_tokens, throughput: tier.throughput_tok_per_sec,
          recall_score: tier.recall_score, ttft_ms: tier.ttft_ms, memory_usage_mb: tier.memory_usage_mb, oom: tier.oom,
        });
        if (tier.oom && lastGoodThroughput != null) {
          oomMarkers.push({ model: p.model, context_tokens: tier.context_tokens, y: lastGoodThroughput, color: slots.colorFor(p.model) });
        }
        if (!tier.oom && tier.throughput_tok_per_sec != null) lastGoodThroughput = tier.throughput_tok_per_sec;
      });
    }
    return { throughputRows, recallRows, tableRows, oomMarkers };
  }, [profiles, slots]);

  const columns: DataTableColumn<TableRow>[] = [
    { key: 'model', header: 'Model', render: r => r.model },
    { key: 'ctx', header: 'Context', align: 'right', render: r => r.context_tokens.toLocaleString() },
    { key: 'throughput', header: 'Throughput', align: 'right', render: r => (r.throughput != null ? String(r.throughput) : (r.oom ? 'OOM' : '—')) },
    { key: 'recall', header: 'Recall', align: 'right', render: r => (r.recall_score != null ? r.recall_score.toFixed(2) : '—') },
    { key: 'ttft', header: 'TTFT (ms)', align: 'right', render: r => (r.ttft_ms != null ? String(r.ttft_ms) : '—') },
    { key: 'mem', header: 'Mem (MB)', align: 'right', render: r => (r.memory_usage_mb != null ? String(r.memory_usage_mb) : '—') },
  ];

  const legendEntries = profiles.map(p => ({ id: p.model, label: p.model, color: slots.colorFor(p.model) }));
  const empty = !ctx.loading && !ctx.degraded && profiles.length === 0;

  return (
    <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(22.5rem, 1fr))', gap: 'var(--space-4)' }}>
      <ChartCard
        title="Context degradation — throughput"
        subtitle="x = context tokens (log) · max_context_safe hairline · ✕ = OOM"
        height={CHART_HEIGHT}
        loading={ctx.loading}
        degraded={ctx.degraded}
        empty={empty}
        emptyMessage="No context profiles recorded yet"
        emptyHint="Throughput-by-context-tier data lands here as MINT profiles the fleet"
        controls={<TableViewControls view={view} onChange={setView} />}
        footer={<ChartLegend entries={legendEntries} />}
      >
        <TableView view={view} columns={columns} rows={tableRows} rowKey={(r, i) => `${r.model}-${r.context_tokens}-${i}`}>
          <ResponsiveContainer width="100%" height={CHART_HEIGHT}>
            <ComposedChart data={throughputRows}>
              <CartesianGrid {...rechartsGridProps()} />
              <XAxis dataKey="context_tokens" scale="log" domain={['auto', 'auto']} ticks={CONTEXT_TICKS} tick={rechartsTickStyle()} tickFormatter={v => `${Math.round(Number(v) / 1024)}k`} />
              <YAxis tick={rechartsTickStyle()} />
              <Tooltip contentStyle={rechartsTooltipStyle()} labelFormatter={v => `${Math.round(Number(v) / 1024)}k tokens`} />
              {profiles.map(p => (
                <Line key={p.model} type="monotone" dataKey={p.model} name={p.model} stroke={slots.colorFor(p.model)} strokeWidth={2} dot={false} connectNulls={false} />
              ))}
              {profiles.map(p => p.max_context_safe != null && (
                <ReferenceLine
                  key={`safe-${p.model}`}
                  x={p.max_context_safe}
                  stroke={slots.colorFor(p.model)}
                  strokeWidth={1}
                  label={{ value: `${p.model} safe`, position: 'top', fill: slots.colorFor(p.model), fontSize: 10 }}
                />
              ))}
              <Scatter data={oomMarkers} dataKey="y" shape={(props: { cx?: number; cy?: number; payload?: { color: string } }) => (
                <text x={props.cx} y={props.cy} textAnchor="middle" dominantBaseline="central" fill={props.payload?.color ?? 'var(--flux-rose)'} fontSize={14} fontWeight={700}>✕</text>
              )} />
            </ComposedChart>
          </ResponsiveContainer>
        </TableView>
      </ChartCard>

      <ChartCard
        title="Context degradation — recall"
        subtitle="sibling chart (never a second axis on the throughput chart)"
        height={CHART_HEIGHT}
        loading={ctx.loading}
        degraded={ctx.degraded}
        empty={empty}
        emptyMessage="No context profiles recorded yet"
        footer={<ChartLegend entries={legendEntries} />}
      >
        <ResponsiveContainer width="100%" height={CHART_HEIGHT}>
          <LineChart data={recallRows}>
            <CartesianGrid {...rechartsGridProps()} />
            <XAxis dataKey="context_tokens" scale="log" domain={['auto', 'auto']} ticks={CONTEXT_TICKS} tick={rechartsTickStyle()} tickFormatter={v => `${Math.round(Number(v) / 1024)}k`} />
            <YAxis tick={rechartsTickStyle()} domain={[0, 1]} />
            <Tooltip contentStyle={rechartsTooltipStyle()} labelFormatter={v => `${Math.round(Number(v) / 1024)}k tokens`} />
            {profiles.map(p => (
              <Line key={p.model} type="monotone" dataKey={p.model} name={p.model} stroke={slots.colorFor(p.model)} strokeWidth={2} dot={false} connectNulls={false} />
            ))}
          </LineChart>
        </ResponsiveContainer>
      </ChartCard>
    </div>
  );
}
