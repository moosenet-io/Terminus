// CONST-23 §7.3: the spec's "Coder" section is C4+C3+C5+C6 (+ a language control), but C3/C5/C6
// are explicitly CONST-24 (phase 2) — this item only owns C4. Rather than leaving C4 with
// nowhere to live, this renders C4 now and reserves C3/C5/C6 as labeled phase-2 placeholders in
// the same section (item brief: "structure the sections so they can slot in") — no layout
// change needed when CONST-24 lands, and the language control arrives with it.
import { useMemo } from 'react';
import { ChartCard } from '../../viz/ChartCard';
import { ScatterChart } from '../../viz/ScatterChart';
import type { ScatterDatum, ScatterSeries } from '../../viz/ScatterChart';
import { SlotAssigner, CATEGORICAL_HEX, CHART_CHROME } from '../../viz/palette';
import { TableView, TableViewControls, useTableView } from '../../viz/TableViewToggle';
import type { DataTableColumn } from '../../components/DataTable';
import { useMintPareto } from '../../hooks/useMint';
import type { MintFilters } from '../../hooks/useMint';
import type { MintParetoPoint } from '../../lib/aggregationClient';
import { MINT_MODEL_SELECT_CAP } from './mintFilters';
import { mintSectionTitleStyle } from './mintShared';

const CHART_HEIGHT = 340;

/** Pareto front (upper-left non-dominated: min latency, max score) — sort by x ascending, keep
 *  points whose y strictly exceeds the running max seen so far. */
function paretoFront(points: MintParetoPoint[]): MintParetoPoint[] {
  const sorted = [...points].sort((a, b) => a.mean_latency_ms - b.mean_latency_ms);
  const front: MintParetoPoint[] = [];
  let bestY = -Infinity;
  for (const p of sorted) {
    if (p.mean_score > bestY) {
      front.push(p);
      bestY = p.mean_score;
    }
  }
  return front;
}

function sizeFor(vram: number, min: number, max: number): number {
  const t = max === min ? 0.5 : (Math.sqrt(vram) - Math.sqrt(min)) / (Math.sqrt(max) - Math.sqrt(min));
  return 8 + Math.max(0, Math.min(1, t)) * 16;
}

interface CoderSectionProps {
  filters: MintFilters;
  onFiltersChange: (next: MintFilters) => void;
}

export function CoderSection({ filters, onFiltersChange }: CoderSectionProps) {
  const pareto = useMintPareto(filters);
  const { view, setView } = useTableView();
  const slots = useMemo(() => new SlotAssigner(), []);

  const points = pareto.data?.points ?? [];
  const front = useMemo(() => paretoFront(points), [points]);
  const frontModels = useMemo(() => new Set(front.map(p => p.model)), [front]);
  const vramRange = useMemo(() => {
    const vrams = points.map(p => p.vram_gb);
    return { min: Math.min(...vrams, 8), max: Math.max(...vrams, 8) };
  }, [points]);

  const hasSelection = filters.models.length > 0;

  const toDatum = (p: MintParetoPoint): ScatterDatum => {
    const color = !hasSelection
      ? CATEGORICAL_HEX[0]
      : filters.models.includes(p.model)
        ? slots.colorFor(p.model)
        : CHART_CHROME.deemphasis;
    return {
      x: p.mean_latency_ms,
      y: p.mean_score,
      size: sizeFor(p.vram_gb, vramRange.min, vramRange.max),
      color,
      label: p.model,
      onFront: frontModels.has(p.model),
      tooltipRows: [
        { key: 'score', label: 'score', value: `${p.mean_score.toFixed(2)} ±${p.score_stddev.toFixed(2)}` },
        { key: 'p95', label: 'p95 latency', value: `${p.p95_latency_ms}ms` },
        { key: 'qpg', label: 'quality/gpu-s', value: p.quality_per_gpu_second.toFixed(3) },
        { key: 'vram', label: 'VRAM', value: `${p.vram_gb}GB` },
      ],
    };
  };

  const series: ScatterSeries[] = [{ id: 'models', data: points.map(toDatum) }];
  const frontData = front.map(toDatum);

  const columns: DataTableColumn<MintParetoPoint>[] = [
    { key: 'model', header: 'Model', render: r => r.model },
    { key: 'latency', header: 'Latency (ms)', align: 'right', render: r => String(r.mean_latency_ms) },
    { key: 'score', header: 'Score', align: 'right', render: r => r.mean_score.toFixed(2) },
    { key: 'vram', header: 'VRAM (GB)', align: 'right', render: r => String(r.vram_gb) },
    { key: 'front', header: 'Pareto front', render: r => frontModels.has(r.model) ? '✓' : '' },
  ];

  const handlePointClick = (point: ScatterDatum) => {
    const model = point.label;
    const already = filters.models.includes(model);
    if (already) {
      onFiltersChange({ ...filters, models: filters.models.filter(m => m !== model) });
      return;
    }
    if (filters.models.length >= MINT_MODEL_SELECT_CAP) return;
    onFiltersChange({ ...filters, models: [...filters.models, model] });
  };

  return (
    <section id="coder" style={{ scrollMarginTop: 64 }}>
      <h3 style={mintSectionTitleStyle}>Coder</h3>
      <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(420px, 1fr))', gap: 'var(--space-3)' }}>
        <ChartCard
          title="Quality × latency Pareto"
          subtitle="size = VRAM (√-scaled) · click a point to add/remove it from the global model filter"
          height={CHART_HEIGHT}
          loading={pareto.loading && !pareto.data}
          isRefetching={pareto.loading && !!pareto.data}
          degraded={pareto.degraded}
          empty={!pareto.loading && points.length === 0}
          emptyMessage="No profiled models for this filter"
          controls={<TableViewControls view={view} onChange={setView} />}
        >
          <TableView view={view} columns={columns} rows={points} rowKey={(r, i) => `${r.model}-${i}`}>
            <ScatterChart
              data={series}
              height={CHART_HEIGHT - 40}
              xScaleType="log"
              yScaleType="linear"
              frontPoints={frontData}
              onPointClick={handlePointClick}
            />
          </TableView>
        </ChartCard>

        <ChartCard title="Latency box plots (C3)" subtitle="phase 2 — CONST-24" height={CHART_HEIGHT} empty emptyMessage="Coming in CONST-24" emptyHint="per-model horizontal box plots, log-scale toggle, n<5 beeswarm fallback">
          <div />
        </ChartCard>
        <ChartCard title="Score beeswarm (C5)" subtitle="phase 2 — CONST-24" height={CHART_HEIGHT} empty emptyMessage="Coming in CONST-24" emptyHint="per-run judge scores, hollow dots for failed runs, lane median tick">
          <div />
        </ChartCard>
        <ChartCard title="Failure-class bars (C6)" subtitle="phase 2 — CONST-24" height={CHART_HEIGHT} empty emptyMessage="Coming in CONST-24" emptyHint="top-4 failure classes + Other, segment click filters the beeswarm">
          <div />
        </ChartCard>
      </div>
    </section>
  );
}
