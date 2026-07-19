// CONST-23 §7.3/§7.2 C7: context degradation lines. x = context_tokens (log, 2k..64k ticks),
// y = throughput; recall is a SIBLING chart (its own ChartCard) — never a second y-axis (§4.4
// "no dual-axis charts anywhere"). max_context_safe renders as a vertical hairline + direct
// label per model; OOM renders as a ✕ marker at the last-good y (the point right before the
// model stops producing data); tooltip is shared across all series (one crosshair, all values)
// since every model reports at the same fixed tier x-positions.
import { useMemo } from 'react';
import { ChartCard } from '../../viz/ChartCard';
import {
  LineChart, Line, XAxis, YAxis, CartesianGrid, Tooltip, ResponsiveContainer, ReferenceLine, Scatter, ComposedChart,
} from '../../viz/recharts';
import { rechartsGridProps, rechartsTickStyle, rechartsTooltipStyle } from '../../viz/theme';
import { SlotAssigner } from '../../viz/palette';
import { ChartLegend } from '../../viz/ChartLegend';
import { TableView, TableViewControls, useTableView } from '../../viz/TableViewToggle';
import type { DataTableColumn } from '../../components/DataTable';
import { useMintContextProfiles } from '../../hooks/useMint';
import type { MintFilters } from '../../hooks/useMint';
import { mintSectionTitleStyle } from './mintShared';

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

export function ContextSection({ filters }: { filters: MintFilters }) {
  const ctx = useMintContextProfiles(filters);
  const { view, setView } = useTableView();
  const slots = useMemo(() => new SlotAssigner(), []);

  const profiles = useMemo(() => {
    if (!ctx.data) return [];
    const all = ctx.data.profiles;
    if (filters.models.length === 0) return all.slice(0, 4);
    return all.filter(p => filters.models.includes(p.model)).slice(0, 4);
  }, [ctx.data, filters.models]);

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
        if (tRow) tRow[p.model] = tier.throughput;
        if (rRow) rRow[p.model] = tier.recall_score;
        tableRows.push({ model: p.model, context_tokens: tier.context_tokens, throughput: tier.throughput, recall_score: tier.recall_score, ttft_ms: tier.ttft_ms, memory_usage_mb: tier.memory_usage_mb, oom: tier.oom });
        if (tier.oom && lastGoodThroughput != null) {
          oomMarkers.push({ model: p.model, context_tokens: tier.context_tokens, y: lastGoodThroughput, color: slots.colorFor(p.model) });
        }
        if (!tier.oom && tier.throughput != null) lastGoodThroughput = tier.throughput;
      });
    }
    return { throughputRows, recallRows, tableRows, oomMarkers };
  }, [profiles, slots]);

  const columns: DataTableColumn<TableRow>[] = [
    { key: 'model', header: 'Model', render: r => r.model },
    { key: 'ctx', header: 'Context', align: 'right', render: r => r.context_tokens.toLocaleString() },
    { key: 'throughput', header: 'Throughput', align: 'right', render: r => r.throughput != null ? String(r.throughput) : (r.oom ? 'OOM' : '—') },
    { key: 'recall', header: 'Recall', align: 'right', render: r => r.recall_score != null ? r.recall_score.toFixed(2) : '—' },
    { key: 'ttft', header: 'TTFT (ms)', align: 'right', render: r => r.ttft_ms != null ? String(r.ttft_ms) : '—' },
    { key: 'mem', header: 'Mem (MB)', align: 'right', render: r => r.memory_usage_mb != null ? String(r.memory_usage_mb) : '—' },
  ];

  const legendEntries = profiles.map(p => ({ id: p.model, label: p.model, color: slots.colorFor(p.model) }));
  const needSelection = !ctx.loading && profiles.length === 0;

  return (
    <section id="context" style={{ scrollMarginTop: 64 }}>
      <h3 style={mintSectionTitleStyle}>Context</h3>

      <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(420px, 1fr))', gap: 'var(--space-3)' }}>
        <ChartCard
          title="Context degradation — throughput"
          subtitle="x = context tokens (log) · max_context_safe hairline · ✕ = OOM"
          height={CHART_HEIGHT}
          loading={ctx.loading && !ctx.data}
          isRefetching={ctx.loading && !!ctx.data}
          degraded={ctx.degraded}
          empty={needSelection}
          emptyMessage="No context profiles for this filter"
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
                {profiles.map(p => (
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
          loading={ctx.loading && !ctx.data}
          isRefetching={ctx.loading && !!ctx.data}
          degraded={ctx.degraded}
          empty={needSelection}
          emptyMessage="No context profiles for this filter"
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
    </section>
  );
}
