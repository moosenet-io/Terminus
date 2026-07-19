// CONST-23/24 §7.3: Coder = C4 (Pareto scatter, CONST-23) + C3 (latency box plots) + C5 (score
// beeswarm) + C6 (failure-class bars), with the language control that scopes ONLY this section
// (§7.1's one documented exception to the global filter row). Cross-chart drill-downs live here
// too: C6 segment click filters C5 to that failure class; C5 dot click switches C5 into table
// view and highlights the matching run row (§7.2 "click-through to the run row in the table
// view"); the heatmap (C2, CoverageSection) cell-click seam CONST-23 left is closed by MintPage
// forwarding `onFiltersChange` down to CoverageSection, which adds the clicked model to the
// global model filter — since C3/C5 already read that same global filter, a heatmap cell click
// visibly re-scopes this section without CoderSection needing to know about C2 at all.
import { useMemo, useState } from 'react';
import { ChartCard } from '../../viz/ChartCard';
import { ScatterChart } from '../../viz/ScatterChart';
import type { ScatterDatum, ScatterSeries } from '../../viz/ScatterChart';
import { BoxPlotChart } from '../../viz/BoxPlotChart';
import { SwarmPlotChart } from '../../viz/SwarmPlotChart';
import type { SwarmLane } from '../../viz/SwarmPlotChart';
import { FailureBarsChart } from '../../viz/FailureBarsChart';
import { SlotAssigner, CATEGORICAL_HEX, CHART_CHROME } from '../../viz/palette';
import { TableView, TableViewControls, useTableView } from '../../viz/TableViewToggle';
import type { DataTableColumn } from '../../components/DataTable';
import { useMintPareto, useMintBox, useMintRuns, useMintFailures } from '../../hooks/useMint';
import type { MintFilters } from '../../hooks/useMint';
import type { MintParetoPoint, MintRun } from '../../lib/aggregationClient';
import { MINT_MODEL_SELECT_CAP, MINT_LANGUAGES } from './mintFilters';
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
  // §7.1: the Coder section's ONE documented scoping exception — a language control local to
  // this section, not part of the global filter row / URL-deep-linked MintFilters object.
  const [language, setLanguage] = useState<string>('all');
  const [failureClassFilter, setFailureClassFilter] = useState<string>('all');
  const [logScale, setLogScale] = useState(true); // §7.2 C3: log-scale x toggle, default ON
  const [highlightedRunId, setHighlightedRunId] = useState<string | undefined>(undefined);

  const pareto = useMintPareto(filters, language);
  const box = useMintBox(filters, 'total_time_ms', language);
  const failures = useMintFailures(filters);
  const runs = useMintRuns(filters, { language, failureClass: failureClassFilter });

  const paretoView = useTableView();
  const boxView = useTableView();
  const swarmView = useTableView();
  const barsView = useTableView();
  const slots = useMemo(() => new SlotAssigner(), []);

  // ── C4: Pareto scatter (CONST-23, now language-scoped) ────────────────────
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
  const paretoSeries: ScatterSeries[] = [{ id: 'models', data: points.map(toDatum) }];
  const paretoFrontData = front.map(toDatum);
  const paretoColumns: DataTableColumn<MintParetoPoint>[] = [
    { key: 'model', header: 'Model', render: r => r.model },
    { key: 'latency', header: 'Latency (ms)', align: 'right', render: r => String(r.mean_latency_ms) },
    { key: 'score', header: 'Score', align: 'right', render: r => r.mean_score.toFixed(2) },
    { key: 'vram', header: 'VRAM (GB)', align: 'right', render: r => String(r.vram_gb) },
    { key: 'front', header: 'Pareto front', render: r => frontModels.has(r.model) ? '✓' : '' },
  ];

  const handleParetoPointClick = (point: ScatterDatum) => {
    const model = point.label;
    const already = filters.models.includes(model);
    if (already) {
      onFiltersChange({ ...filters, models: filters.models.filter(m => m !== model) });
      return;
    }
    if (filters.models.length >= MINT_MODEL_SELECT_CAP) return;
    onFiltersChange({ ...filters, models: [...filters.models, model] });
  };

  // ── C3: latency box plots ──────────────────────────────────────────────────
  const boxGroups = useMemo(() => {
    const all = box.data?.groups ?? [];
    if (filters.models.length === 0) return all;
    return all.filter(g => filters.models.includes(g.model));
  }, [box.data, filters.models]);
  const boxColumns: DataTableColumn<{ model: string; min: number; q1: number; median: number; q3: number; max: number; n: number }>[] = [
    { key: 'model', header: 'Model', render: r => r.model },
    { key: 'min', header: 'Min', align: 'right', render: r => String(r.min) },
    { key: 'q1', header: 'Q1', align: 'right', render: r => String(r.q1) },
    { key: 'median', header: 'Median', align: 'right', render: r => String(r.median) },
    { key: 'q3', header: 'Q3', align: 'right', render: r => String(r.q3) },
    { key: 'max', header: 'Max', align: 'right', render: r => String(r.max) },
    { key: 'n', header: 'n', align: 'right', render: r => String(r.n) },
  ];

  // ── C5: score beeswarm ──────────────────────────────────────────────────────
  const allRuns = runs.data?.runs ?? [];
  const swarmLaneModels = useMemo(() => {
    if (filters.models.length > 0) return filters.models.slice(0, MINT_MODEL_SELECT_CAP);
    // §7.2: "<=4 selected, else top-4 by n"
    const counts = new Map<string, number>();
    for (const r of allRuns) counts.set(r.model, (counts.get(r.model) ?? 0) + 1);
    return [...counts.entries()].sort((a, b) => b[1] - a[1]).slice(0, MINT_MODEL_SELECT_CAP).map(([m]) => m);
  }, [filters.models, allRuns]);
  const swarmLanes: SwarmLane[] = swarmLaneModels.map(model => ({ id: model, color: slots.colorFor(model) }));
  const swarmColumns: DataTableColumn<MintRun>[] = [
    { key: 'run', header: 'Run', render: r => r.run_id },
    { key: 'model', header: 'Model', render: r => r.model },
    { key: 'case', header: 'Case', render: r => r.case_id },
    { key: 'lang', header: 'Language', render: r => r.language },
    { key: 'score', header: 'Score', align: 'right', render: r => String(r.score) },
    { key: 'failure', header: 'Failure', render: r => r.failure_class },
    { key: 'time', header: 'Time (ms)', align: 'right', render: r => String(r.total_time_ms) },
  ];

  const handleDotClick = (run: MintRun) => {
    setHighlightedRunId(run.run_id);
    swarmView.setView('table');
  };
  const handleLaneHeaderClick = (model: string) => {
    // §7.2 C5: "lane header click -> C6 for that model" — scope C6 to just that model via the
    // global model filter (C6 already reads the same `filters.models`).
    if (!filters.models.includes(model) && filters.models.length < MINT_MODEL_SELECT_CAP) {
      onFiltersChange({ ...filters, models: [...filters.models, model] });
    }
  };

  // ── C6: failure-class bars ──────────────────────────────────────────────────
  const failureModels = useMemo(() => {
    const all = failures.data?.models ?? [];
    if (filters.models.length === 0) return all;
    return all.filter(m => filters.models.includes(m.model));
  }, [failures.data, filters.models]);
  const failureClasses = failures.data?.classes ?? [];
  const allNoneEpoch = !failures.loading && failureClasses.length === 0;

  const handleSegmentClick = (cls: string) => {
    setFailureClassFilter(prev => prev === cls ? 'all' : cls);
  };

  return (
    <section id="coder" style={{ scrollMarginTop: 64 }}>
      <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', flexWrap: 'wrap', gap: 'var(--space-2)' }}>
        <h3 style={mintSectionTitleStyle}>Coder</h3>
        <div style={{ display: 'flex', alignItems: 'center', gap: 6 }}>
          <span style={{ fontFamily: 'var(--font-mono)', fontSize: 'var(--fs-mono-sm)', color: 'var(--text-400)', textTransform: 'uppercase', letterSpacing: 'var(--ls-label)' }}>
            Language
          </span>
          <select
            value={language}
            onChange={e => setLanguage(e.target.value)}
            style={{ background: 'var(--bg-elevated)', border: '1px solid var(--border)', borderRadius: 'var(--radius-sm)', color: 'var(--text-body)', padding: '4px 8px', fontSize: 'var(--fs-sm)' }}
          >
            <option value="all">All languages</option>
            {MINT_LANGUAGES.map(l => <option key={l} value={l}>{l}</option>)}
          </select>
          {failureClassFilter !== 'all' && (
            <button
              type="button"
              onClick={() => setFailureClassFilter('all')}
              style={{ fontFamily: 'var(--font-mono)', fontSize: 'var(--fs-mono-sm)', color: 'var(--accent-bright)', background: 'var(--bg-elevated)', border: '1px solid var(--border)', borderRadius: 'var(--radius-sm)', padding: '3px 8px', cursor: 'pointer' }}
            >
              failure_class={failureClassFilter} ✕
            </button>
          )}
        </div>
      </div>

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
          controls={<TableViewControls view={paretoView.view} onChange={paretoView.setView} />}
        >
          <TableView view={paretoView.view} columns={paretoColumns} rows={points} rowKey={(r, i) => `${r.model}-${i}`}>
            <ScatterChart
              data={paretoSeries}
              height={CHART_HEIGHT - 40}
              xScaleType="log"
              yScaleType="linear"
              frontPoints={paretoFrontData}
              onPointClick={handleParetoPointClick}
            />
          </TableView>
        </ChartCard>

        <ChartCard
          title="Latency box plots"
          subtitle="horizontal · single hue · outliers ringed · n<5 renders a beeswarm strip"
          height={CHART_HEIGHT}
          loading={box.loading && !box.data}
          isRefetching={box.loading && !!box.data}
          degraded={box.degraded}
          empty={!box.loading && boxGroups.length === 0}
          emptyMessage="No latency data for this filter"
          controls={
            <div style={{ display: 'flex', alignItems: 'center', gap: 8 }}>
              <label style={{ display: 'flex', alignItems: 'center', gap: 4, fontSize: 'var(--fs-mono-sm)', fontFamily: 'var(--font-mono)', color: 'var(--text-muted)', cursor: 'pointer' }}>
                <input type="checkbox" checked={logScale} onChange={e => setLogScale(e.target.checked)} />
                log scale
              </label>
              <TableViewControls view={boxView.view} onChange={boxView.setView} />
            </div>
          }
        >
          <TableView view={boxView.view} columns={boxColumns} rows={boxGroups} rowKey={(r, i) => `${r.model}-${i}`}>
            <BoxPlotChart groups={boxGroups} height={CHART_HEIGHT - 40} logScale={logScale} />
          </TableView>
        </ChartCard>

        <ChartCard
          title="Score beeswarm"
          subtitle="per-run judge scores · hollow dots = failed runs · click a dot to inspect its run row"
          height={CHART_HEIGHT}
          loading={runs.loading && !runs.data}
          isRefetching={runs.loading && !!runs.data}
          degraded={runs.degraded}
          empty={!runs.loading && allRuns.length === 0}
          emptyMessage="No runs for this filter"
          controls={<TableViewControls view={swarmView.view} onChange={swarmView.setView} />}
        >
          <TableView view={swarmView.view} columns={swarmColumns} rows={allRuns} rowKey={(r, i) => `${r.run_id}-${i}`} highlightRowKey={highlightedRunId}>
            <SwarmPlotChart
              runs={allRuns}
              lanes={swarmLanes}
              height={CHART_HEIGHT - 40}
              onDotClick={handleDotClick}
              onLaneHeaderClick={handleLaneHeaderClick}
            />
          </TableView>
        </ChartCard>

        <ChartCard
          title="Failure-class bars"
          subtitle="top-4 classes fleet-wide + Other · 'none' excluded · segment click filters the beeswarm"
          height={CHART_HEIGHT}
          loading={failures.loading && !failures.data}
          isRefetching={failures.loading && !!failures.data}
          degraded={failures.degraded}
          empty={allNoneEpoch}
          emptyMessage="No failures this epoch"
          controls={<TableViewControls view={barsView.view} onChange={barsView.setView} />}
        >
          <TableView
            view={barsView.view}
            columns={[
              { key: 'model', header: 'Model', render: r => r.model },
              { key: 'class', header: 'Failure class', render: r => r.cls },
              { key: 'count', header: 'Count', align: 'right', render: r => String(r.count) },
              { key: 'pct', header: '% of runs', align: 'right', render: r => r.total_runs > 0 ? `${Math.round((r.count / r.total_runs) * 100)}%` : '—' },
            ]}
            rows={failureModels.flatMap(m => failureClasses.map(cls => ({ model: m.model, cls, count: m.counts[cls] ?? 0, total_runs: m.total_runs })))}
            rowKey={(r, i) => `${r.model}-${r.cls}-${i}`}
          >
            <FailureBarsChart classes={failureClasses} models={failureModels} height={CHART_HEIGHT - 40} onSegmentClick={handleSegmentClick} />
          </TableView>
        </ChartCard>
      </div>
    </section>
  );
}
