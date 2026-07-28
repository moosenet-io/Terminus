// CGUI-10/CONST-23/24 reconciliation: ports CONST-24's C9 (trade-off parallel coordinates)
// chart type into the live CGUI-08 data client. CONST-24's original TradeoffsSection read a
// fictional mock-only `/mint/tradeoffs` endpoint (see aggregationClient.ts's reconciliation
// note) that was never part of the real CONST-21 backend contract — there is no live
// `mint.tradeoffs()` method. Instead this section assembles the same 6-dimension trade-off
// space CLIENT-SIDE from two endpoints that ARE real and already typed on the CGUI-08 client:
//   - `client.mint.languageStats()`  -> mean_score, pass_hat_3, mean_throughput, p95_latency_ms,
//                                        vram_gb, aggregated per model across every language row
//   - `client.mint.contextProfiles()` -> max_context_safe per model
// Normalization ranges are computed DYNAMICALLY from the fetched fleet data (min/max actually
// observed) rather than the CONST-24 mock's hardcoded fixture bounds, since there's no fixed
// fixture to anchor against against the real endpoint.
//
// The chart itself (ParallelCoordinatesChart.tsx, viz/nivo-parallel-coordinates.d.ts) is
// ported unmodified from CONST-24 — it's a generic `{dims, points}` renderer with no coupling
// to the mock, built on the same MintTradeoffDim/MintTradeoffPoint shapes this section produces.
import { useMemo } from 'react';
import { ChartCard } from '../../viz/ChartCard';
import { ParallelCoordinatesChart, partitionCompleteTradeoffs } from '../../viz/ParallelCoordinatesChart';
import { SlotAssigner } from '../../viz/palette';
import { TableView, TableViewControls, useTableView } from '../../viz/TableViewToggle';
import type { DataTableColumn } from '../../components/DataTable';
import { useMintSection } from '../../hooks/useMint';
import type {
  MintLanguageStatsResponse, MintContextProfilesResponse,
} from '../../types/mint';
import type { MintTradeoffDim, MintTradeoffDimKey, MintTradeoffPoint } from '../../lib/aggregationClient';

const CHART_HEIGHT = 320;

const DIM_META: Array<{ key: MintTradeoffDimKey; label: string; unit: string; invert: boolean }> = [
  { key: 'mean_score', label: 'Mean score', unit: '', invert: false },
  { key: 'pass_hat_3', label: 'pass^3', unit: '', invert: false },
  { key: 'mean_throughput', label: 'Throughput', unit: 'tok/s', invert: false },
  { key: 'p95_latency_ms', label: 'p95 latency', unit: 'ms', invert: true },
  { key: 'vram_gb', label: 'VRAM', unit: 'GB', invert: true },
  { key: 'max_context_safe', label: 'Max safe context', unit: 'tok', invert: false },
];

interface TableRow {
  model: string;
  mean_score: string;
  pass_hat_3: string;
  mean_throughput: string;
  p95_latency_ms: string;
  vram_gb: string;
  max_context_safe: string;
}

/** Aggregates per-model raw trade-off values from the two real MINT endpoints (mean across
 *  language rows for the language-stats-sourced dims; the model's own max_context_safe for the
 *  context dim), then normalizes each dim to [0,1] against the OBSERVED fleet min/max (an
 *  `invert:true` dim — latency, VRAM — flips so 1 always means "best", matching the chart's
 *  contract). Returns empty when fewer than 2 models have every dimension populated. */
function buildTradeoffs(
  languageStats: MintLanguageStatsResponse | null,
  contextProfiles: MintContextProfilesResponse | null,
): { dims: MintTradeoffDim[]; points: MintTradeoffPoint[] } {
  const byModel = new Map<string, { sums: Partial<Record<MintTradeoffDimKey, number>>; counts: Partial<Record<MintTradeoffDimKey, number>> }>();
  const bump = (model: string, key: MintTradeoffDimKey, v: number | null | undefined) => {
    if (v == null) return;
    const entry = byModel.get(model) ?? { sums: {}, counts: {} };
    entry.sums[key] = (entry.sums[key] ?? 0) + v;
    entry.counts[key] = (entry.counts[key] ?? 0) + 1;
    byModel.set(model, entry);
  };

  for (const row of languageStats?.rows ?? []) {
    bump(row.model, 'mean_score', row.mean_score);
    bump(row.model, 'pass_hat_3', row.pass_hat_3);
    bump(row.model, 'mean_throughput', row.mean_throughput);
    bump(row.model, 'p95_latency_ms', row.p95_latency_ms);
    bump(row.model, 'vram_gb', row.vram_gb);
  }
  for (const p of contextProfiles?.models ?? []) {
    bump(p.model, 'max_context_safe', p.max_context_safe);
  }

  const rawByModel = new Map<string, Partial<Record<MintTradeoffDimKey, number>>>();
  for (const [model, { sums, counts }] of byModel) {
    const raw: Partial<Record<MintTradeoffDimKey, number>> = {};
    for (const key of Object.keys(sums) as MintTradeoffDimKey[]) {
      const c = counts[key] ?? 0;
      if (c > 0) raw[key] = (sums[key] ?? 0) / c;
    }
    rawByModel.set(model, raw);
  }

  // Observed [min,max] per dim across the fleet, for normalization.
  const bounds: Partial<Record<MintTradeoffDimKey, { min: number; max: number }>> = {};
  for (const raw of rawByModel.values()) {
    for (const meta of DIM_META) {
      const v = raw[meta.key];
      if (v == null) continue;
      const b = bounds[meta.key];
      if (!b) bounds[meta.key] = { min: v, max: v };
      else { b.min = Math.min(b.min, v); b.max = Math.max(b.max, v); }
    }
  }

  const dims: MintTradeoffDim[] = DIM_META.map(meta => ({
    key: meta.key,
    label: meta.label,
    unit: meta.unit,
    min: bounds[meta.key]?.min ?? 0,
    max: bounds[meta.key]?.max ?? 1,
    invert: meta.invert,
  }));

  const points: MintTradeoffPoint[] = Array.from(rawByModel.entries()).map(([model, raw]) => {
    const norm: Partial<Record<MintTradeoffDimKey, number>> = {};
    for (const dim of dims) {
      const v = raw[dim.key];
      if (v == null) continue;
      const span = dim.max - dim.min || 1;
      const t = Math.max(0, Math.min(1, (v - dim.min) / span));
      norm[dim.key] = dim.invert ? 1 - t : t;
    }
    return { model, raw, norm };
  });

  return { dims, points };
}

export function TradeoffsSection() {
  const languageStats = useMintSection(c => c.mint.languageStats(), 'tradeoffs-lang');
  const contextProfiles = useMintSection(c => c.mint.contextProfiles(), 'tradeoffs-ctx');
  const { view, setView } = useTableView();
  const slots = useMemo(() => new SlotAssigner(), []);

  const loading = languageStats.loading || contextProfiles.loading;
  const degraded = languageStats.degraded || contextProfiles.degraded;

  const { dims, points } = useMemo(
    () => buildTradeoffs(languageStats.data, contextProfiles.data),
    [languageStats.data, contextProfiles.data],
  );
  const { complete, excludedCount } = useMemo(() => partitionCompleteTradeoffs(dims, points), [dims, points]);
  // Up to the first 4 complete models are treated as the "selected" series (the chart itself has
  // no external model-filter row on this branch's reconciled Overview panel); every other
  // profiled model still renders as a de-emphasized context line.
  const selectedModels = useMemo(() => complete.slice(0, 4).map(p => p.model), [complete]);

  const columns: DataTableColumn<TableRow>[] = [
    { key: 'model', header: 'Model', render: r => r.model },
    { key: 'score', header: 'Mean score', align: 'right', render: r => r.mean_score },
    { key: 'pass3', header: 'pass^3', align: 'right', render: r => r.pass_hat_3 },
    { key: 'throughput', header: 'Throughput', align: 'right', render: r => r.mean_throughput },
    { key: 'p95', header: 'p95 latency', align: 'right', render: r => r.p95_latency_ms },
    { key: 'vram', header: 'VRAM', align: 'right', render: r => r.vram_gb },
    { key: 'ctx', header: 'Max safe context', align: 'right', render: r => r.max_context_safe },
  ];

  const tableRows: TableRow[] = complete.map(p => ({
    model: p.model,
    mean_score: p.raw.mean_score != null ? p.raw.mean_score.toFixed(2) : '—',
    pass_hat_3: p.raw.pass_hat_3 != null ? p.raw.pass_hat_3.toFixed(2) : '—',
    mean_throughput: p.raw.mean_throughput != null ? p.raw.mean_throughput.toFixed(1) : '—',
    p95_latency_ms: p.raw.p95_latency_ms != null ? String(Math.round(p.raw.p95_latency_ms)) : '—',
    vram_gb: p.raw.vram_gb != null ? p.raw.vram_gb.toFixed(1) : '—',
    max_context_safe: p.raw.max_context_safe != null ? p.raw.max_context_safe.toLocaleString() : '—',
  }));

  const empty = !loading && !degraded && complete.length < 2;

  return (
    <ChartCard
      title="Trade-off parallel coordinates"
      subtitle="mean_score, pass^3, throughput, p95 latency (inv), VRAM (inv), max safe context · drag an axis to brush-filter"
      height={CHART_HEIGHT}
      loading={loading}
      degraded={degraded}
      empty={empty}
      emptyMessage="Fewer than 2 models have all 6 trade-off dimensions profiled"
      emptyHint="Needs language-stats and context-profile coverage on the same models"
      controls={<TableViewControls view={view} onChange={setView} />}
      footer={excludedCount > 0 ? (
        <div style={{ fontSize: 'var(--fs-xs)', color: 'var(--text-faint)' }}>
          {excludedCount} model{excludedCount === 1 ? '' : 's'} excluded — missing at least one of the 6 dimensions
        </div>
      ) : undefined}
    >
      <TableView view={view} columns={columns} rows={tableRows} rowKey={(r, i) => `${r.model}-${i}`}>
        <ParallelCoordinatesChart
          dims={dims}
          points={complete}
          selectedModels={selectedModels}
          colorFor={model => slots.colorFor(model)}
          height={CHART_HEIGHT - 40}
        />
      </TableView>
    </ChartCard>
  );
}
