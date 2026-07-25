// CGUI-10 (TERM #533): MINT — Category Reports. The heart of the module: a category picker
// (grouped tabs) over all 12 MINT categories — the 8 new task-categories, the 3 legacy suites,
// and the persona/assistant radar — each rendering a first-class, bespoke report driven ENTIRELY
// by live DB reads through the CGUI-08 data client (`client.mint.*`). No section hardcodes a
// score; an un-profiled category returns an empty 200 and every card fails open to a clean empty
// state (never a crash).
//
// Per-category report (five bespoke views, §2 of the item brief):
//   1. Capability radar   — per-model unit scores across the category's metrics/dimensions.
//   2. Coverage heatmap   — model × metric matrix, colored by capability, labeled with raw value.
//   3. Distribution box   — per-model 5-number summary + outliers for a chosen metric.
//   4. Ranking (Pareto)   — models ranked by the primary metric's capability score.
//   5. Failures + runs    — failure-class breakdown and the recent run history.
//
// Charts come exclusively from the viz kit (src/viz) — nivo radar/heatmap load lazily via the
// reserved `viz` chunk (React.lazy), the box plot is a bespoke SVG. Every chart has a table twin
// (§4.4). Deep-space/violet tokens only.
import { Suspense, lazy, useMemo, useState } from 'react';
import { PanelRoot } from '../../components/PanelRoot';
import { CardTitle } from '../../components/Card';
import { Badge } from '../../components/Badge';
import { DataTable } from '../../components/DataTable';
import type { DataTableColumn } from '../../components/DataTable';
import { ChartCard } from '../../viz/ChartCard';
import { ChartLegend } from '../../viz/ChartLegend';
import { ChartSkeleton } from '../../viz/ChartSkeleton';
import { ChartTooltip } from '../../viz/ChartTooltip';
import { TableView, TableViewControls, useTableView } from '../../viz/TableViewToggle';
import { BoxPlotChart } from '../../viz/BoxPlotChart';
import {
  BarChart, Bar, XAxis, YAxis, CartesianGrid, Tooltip, ResponsiveContainer, Cell,
} from '../../viz/recharts';
import { rechartsGridProps, rechartsTickStyle } from '../../viz/theme';
import { CATEGORICAL_HEX, CHART_CHROME, SlotAssigner } from '../../viz/palette';
import { useMintSection } from '../../hooks/useMint';
import {
  MINT_CATEGORY_META, categoryById, DEFAULT_CATEGORY_ID, metricLabel, formatMetricValue,
  legacyTestType,
  type CategoryMeta,
} from './categoryMeta';
import {
  radarFromCategory, radarFromDimensions, radarFromLegacyMatrix, heatmapFromCategory,
  heatmapFromLegacyMatrix, boxFromCategory, boxFromLegacy, failuresFromCategory, failuresFromLegacy,
  rankingFromCategory, rankingFromDimensions, rankingFromLegacyMatrix, metricsOfCategory,
  type RadarVM, type HeatmapVM, type RankRow, type FailuresVM,
} from './transforms';
import type { MintRunRow } from '../../types/mint';

// CGUI-10: the MINT multi-series radar (MintRadarChart), distinct from CGUI-09's single-series
// Models radar (viz/RadarChart.tsx). Both lazy-load into the reserved `viz` nivo chunk.
const MintRadarChart = lazy(() => import('../../viz/MintRadarChart'));
const HeatmapChart = lazy(() => import('../../viz/HeatmapChart'));

// ── Category picker ─────────────────────────────────────────────────────────────

const GROUP_ORDER: CategoryMeta['group'][] = ['Retrieval', 'Multimodal', 'Agentic', 'Code', 'Assistant'];

function CategoryPicker({ selected, onSelect }: { selected: string; onSelect: (id: string) => void }) {
  return (
    <div style={{ display: 'flex', flexDirection: 'column', gap: 'var(--space-2)' }}>
      {GROUP_ORDER.map(group => {
        const cats = MINT_CATEGORY_META.filter(c => c.group === group);
        if (cats.length === 0) return null;
        return (
          <div key={group} style={{ display: 'flex', alignItems: 'center', gap: 'var(--space-2)', flexWrap: 'wrap' }}>
            <span style={{ fontSize: 'var(--fs-mono-sm)', textTransform: 'uppercase', letterSpacing: 'var(--ls-label)', color: 'var(--text-400)', minWidth: 92 }}>
              {group}
            </span>
            {cats.map(c => {
              const active = c.id === selected;
              return (
                <button
                  key={c.id}
                  type="button"
                  onClick={() => onSelect(c.id)}
                  aria-pressed={active}
                  style={{
                    fontFamily: 'var(--font-mono)',
                    fontSize: 'var(--fs-mono-sm)',
                    padding: 'var(--space-1) var(--space-3)',
                    borderRadius: 'var(--radius-sm)',
                    border: '1px solid var(--border)',
                    cursor: 'pointer',
                    background: active ? 'var(--grad-accent)' : 'transparent',
                    color: active ? 'var(--accent-on)' : 'var(--text-body)',
                    boxShadow: active ? 'var(--glow-violet-soft)' : 'none',
                  }}
                >
                  {c.label}
                </button>
              );
            })}
          </div>
        );
      })}
    </div>
  );
}

// ── Section 1: capability radar ──────────────────────────────────────────────────

function RadarSection({ cat }: { cat: CategoryMeta }) {
  const testType = legacyTestType(cat);
  const summary = useMintSection(
    cat.kind === 'newcat' ? c => c.mint.categorySummary(cat.clientKey!) : null,
    `radar-sum:${cat.id}`,
  );
  const dims = useMintSection(
    cat.kind === 'persona' ? c => c.mint.dimensions() : null,
    `radar-dim:${cat.id}`,
  );
  const matrix = useMintSection(
    cat.kind === 'legacy' ? c => c.mint.matrix() : null,
    `radar-mat:${cat.id}`,
  );
  const { view, setView } = useTableView();

  const vm: RadarVM = useMemo(() => {
    if (cat.kind === 'newcat') return radarFromCategory(summary.data);
    if (cat.kind === 'persona') return radarFromDimensions(dims.data);
    return radarFromLegacyMatrix(matrix.data, testType ?? '');
  }, [cat.kind, summary.data, dims.data, matrix.data, testType]);
  const loading = summary.loading || dims.loading || matrix.loading;
  const degraded = summary.degraded || dims.degraded || matrix.degraded;
  const empty = !loading && !degraded && (vm.axes.length === 0 || vm.series.length === 0);

  const shownModels = vm.series.slice(0, 4);
  const legend = shownModels.map((s, i) => ({ id: s.model, label: s.model, color: CATEGORICAL_HEX[i] }));

  const columns: DataTableColumn<{ axis: string; i: number }>[] = [
    { key: 'axis', header: 'Dimension', render: r => metricLabel(vm.axes[r.i]) },
    ...shownModels.map((s, si) => ({
      key: s.model, header: s.model, align: 'right' as const,
      render: (r: { i: number }) => (s.values[r.i] ?? 0).toFixed(2),
    })),
  ];
  const tableRows = vm.axes.map((axis, i) => ({ axis, i }));

  const subtitle = cat.kind === 'newcat'
    ? 'Per-model unit scores across this category’s metrics (higher = better)'
    : cat.kind === 'persona'
      ? 'Assistant capability dimensions per model (fleet-normalized)'
      : `Per-model pass rate across the ${cat.legacySuite} suite’s task categories`;

  return (
    <ChartCard
      title="Capability Radar"
      subtitle={subtitle}
      controls={<TableViewControls view={view} onChange={setView} />}
      height={320}
      loading={loading}
      degraded={degraded}
      empty={empty}
      emptyMessage="No profiling data for this category yet"
      emptyHint={cat.kind === 'legacy' ? `The ${cat.legacySuite} suite has no catalog-matrix columns — see Recent Runs below for its suite-scoped data` : 'Radar builds up once MINT profiles models in this category'}
      footer={<ChartLegend entries={legend} />}
    >
      <TableView view={view} columns={columns} rows={tableRows} rowKey={r => r.axis}>
        <Suspense fallback={<ChartSkeleton height={320} />}>
          <MintRadarChart vm={vm} />
        </Suspense>
      </TableView>
    </ChartCard>
  );
}

// ── Section 2: coverage heatmap ──────────────────────────────────────────────────

function HeatmapSection({ cat }: { cat: CategoryMeta }) {
  const testType = legacyTestType(cat);
  const catMatrix = useMintSection(
    cat.kind === 'newcat' ? c => c.mint.categoryMatrix(cat.clientKey!) : null,
    `heat-cat:${cat.id}`,
  );
  const fleetMatrix = useMintSection(
    cat.kind === 'legacy' || cat.kind === 'persona' ? c => c.mint.matrix() : null,
    `heat-mat:${cat.id}`,
  );
  const { view, setView } = useTableView();

  const vm: HeatmapVM = useMemo(() => {
    if (cat.kind === 'newcat') return heatmapFromCategory(catMatrix.data);
    if (cat.kind === 'legacy') return heatmapFromLegacyMatrix(fleetMatrix.data, testType);
    return heatmapFromLegacyMatrix(fleetMatrix.data); // persona: full fleet capability matrix
  }, [cat.kind, catMatrix.data, fleetMatrix.data, testType]);

  const loading = catMatrix.loading || fleetMatrix.loading;
  const degraded = catMatrix.degraded || fleetMatrix.degraded;
  const empty = !loading && !degraded && (vm.models.length === 0 || vm.metrics.length === 0);

  const metricIdByLabel = useMemo(() => {
    const map: Record<string, string> = {};
    for (const m of vm.metrics) map[metricLabel(m)] = m;
    return map;
  }, [vm.metrics]);

  const columns: DataTableColumn<{ model: string }>[] = [
    { key: 'model', header: 'Model', render: r => r.model },
    ...vm.metrics.map(metric => ({
      key: metric, header: metricLabel(metric), align: 'right' as const,
      render: (r: { model: string }) => formatMetricValue(metric, vm.cell[r.model]?.[metric]?.value ?? null),
    })),
  ];

  const subtitle = cat.kind === 'newcat'
    ? 'Model × metric — color encodes capability, label shows the raw value'
    : cat.kind === 'legacy'
      ? `Model × task-category pass rate, scoped to the ${cat.legacySuite} suite`
      : 'Fleet capability matrix — model × (test type / task category) pass rate';

  return (
    <ChartCard
      title="Coverage Heatmap"
      subtitle={subtitle}
      controls={<TableViewControls view={view} onChange={setView} />}
      height={Math.max(200, vm.models.length * 42 + 96)}
      loading={loading}
      degraded={degraded}
      empty={empty}
      emptyMessage="No coverage matrix for this category yet"
      emptyHint={cat.kind === 'legacy' ? `The ${cat.legacySuite} suite has no catalog-matrix cells — its data is in Recent Runs below` : 'Each cell fills in as a model is profiled on that metric'}
    >
      <TableView view={view} columns={columns} rows={vm.models.map(model => ({ model }))} rowKey={r => r.model}>
        <Suspense fallback={<ChartSkeleton height={200} />}>
          <HeatmapChart vm={vm} metricIdByLabel={metricIdByLabel} />
        </Suspense>
      </TableView>
    </ChartCard>
  );
}

// ── Section 3: distribution box plot ─────────────────────────────────────────────

const LEGACY_BOX_METRICS = ['total_time_ms', 'code_quality_score'];

function DistributionSection({ cat }: { cat: CategoryMeta }) {
  const summary = useMintSection(
    cat.kind === 'newcat' ? c => c.mint.categorySummary(cat.clientKey!) : null,
    `dist-sum:${cat.id}`,
  );
  // The `mint.box` endpoint reads CODE runs only (server-side), so a legacy distribution is
  // meaningful ONLY for the code suite — never borrow code timings under context/agent.
  const boxApplies = cat.kind === 'newcat' || (cat.kind === 'legacy' && cat.legacySuite === 'code');
  const catMetrics = useMemo(() => metricsOfCategory(summary.data), [summary.data]);
  const metricOptions = cat.kind === 'newcat' ? catMetrics : (cat.kind === 'legacy' && cat.legacySuite === 'code') ? LEGACY_BOX_METRICS : [];
  const [metric, setMetric] = useState<string | null>(null);
  const activeMetric = metric ?? metricOptions[0] ?? null;

  const catBox = useMintSection(
    cat.kind === 'newcat' && activeMetric ? c => c.mint.categoryBox(cat.clientKey!, activeMetric) : null,
    `dist-catbox:${cat.id}:${activeMetric ?? ''}`,
  );
  const legacyBox = useMintSection(
    cat.kind === 'legacy' && cat.legacySuite === 'code'
      ? c => c.mint.box(activeMetric ? { metric: activeMetric as 'total_time_ms' | 'code_quality_score' } : undefined)
      : null,
    `dist-legbox:${cat.id}:${activeMetric ?? ''}`,
  );
  const { view, setView } = useTableView();

  const vm = useMemo(
    () => (cat.kind === 'newcat' ? boxFromCategory(catBox.data) : boxFromLegacy(legacyBox.data, activeMetric)),
    [cat.kind, catBox.data, legacyBox.data, activeMetric],
  );
  const box = { loading: catBox.loading || legacyBox.loading, degraded: catBox.degraded || legacyBox.degraded };
  const empty = !box.loading && !box.degraded && vm.groups.length === 0;
  const fmt = (v: number) => formatMetricValue(activeMetric ?? '', v);

  if (!boxApplies) {
    const why = cat.kind === 'persona'
      ? 'Persona is a capability radar, not a timed-run distribution'
      : `Timing distribution is recorded for the code suite only, not the ${cat.legacySuite} suite`;
    return (
      <ChartCard title="Distribution" height={96} empty emptyMessage="No distribution for this category" emptyHint={why}>
        <div />
      </ChartCard>
    );
  }

  const columns: DataTableColumn<typeof vm.groups[number]>[] = [
    { key: 'model', header: 'Model', render: r => r.model },
    { key: 'min', header: 'Min', align: 'right', render: r => fmt(r.min) },
    { key: 'q1', header: 'Q1', align: 'right', render: r => fmt(r.q1) },
    { key: 'median', header: 'Median', align: 'right', render: r => fmt(r.median) },
    { key: 'q3', header: 'Q3', align: 'right', render: r => fmt(r.q3) },
    { key: 'max', header: 'Max', align: 'right', render: r => fmt(r.max) },
    { key: 'n', header: 'n', align: 'right', render: r => String(r.n) },
  ];

  const controls = (
    <div style={{ display: 'flex', alignItems: 'center', gap: 'var(--space-2)' }}>
      {metricOptions.length > 1 && (
        <select
          value={activeMetric ?? ''}
          onChange={e => setMetric(e.target.value)}
          style={{
            fontFamily: 'var(--font-mono)', fontSize: 'var(--fs-mono-sm)',
            background: 'var(--space-800)', color: 'var(--text-body)',
            border: '1px solid var(--border)', borderRadius: 'var(--radius-sm)', padding: 'var(--space-1) var(--space-2)',
          }}
        >
          {metricOptions.map(m => <option key={m} value={m}>{metricLabel(m)}</option>)}
        </select>
      )}
      <TableViewControls view={view} onChange={setView} />
    </div>
  );

  return (
    <ChartCard
      title="Distribution"
      subtitle={activeMetric ? `Per-model spread of ${metricLabel(activeMetric)} (box = IQR, whiskers = min/max, dots = outliers)` : 'Per-model metric distribution'}
      controls={metricOptions.length > 0 ? controls : <TableViewControls view={view} onChange={setView} />}
      height={Math.max(180, vm.groups.length * 34 + 40)}
      loading={box.loading}
      degraded={box.degraded}
      empty={empty}
      emptyMessage="No distribution data for this metric yet"
      emptyHint="Needs multiple runs per model to summarize a spread"
    >
      <TableView view={view} columns={columns} rows={vm.groups} rowKey={r => r.model}>
        <BoxPlotChart vm={vm} formatValue={fmt} height={Math.max(140, vm.groups.length * 34 + 30)} />
      </TableView>
    </ChartCard>
  );
}

// ── Section 4: ranking (Pareto) ──────────────────────────────────────────────────

function RankingSection({ cat }: { cat: CategoryMeta }) {
  const testType = legacyTestType(cat);
  const summary = useMintSection(
    cat.kind === 'newcat' ? c => c.mint.categorySummary(cat.clientKey!) : null,
    `rank-sum:${cat.id}`,
  );
  const dims = useMintSection(
    cat.kind === 'persona' ? c => c.mint.dimensions() : null,
    `rank-dim:${cat.id}`,
  );
  const matrix = useMintSection(
    cat.kind === 'legacy' ? c => c.mint.matrix() : null,
    `rank-mat:${cat.id}`,
  );
  const { view, setView } = useTableView();

  const primaryMetric = cat.kind === 'newcat' ? metricsOfCategory(summary.data)[0] ?? null : null;
  const rows: RankRow[] = useMemo(() => {
    if (cat.kind === 'newcat') return rankingFromCategory(summary.data, primaryMetric);
    if (cat.kind === 'persona') return rankingFromDimensions(dims.data);
    return rankingFromLegacyMatrix(matrix.data, testType ?? '');
  }, [cat.kind, summary.data, dims.data, matrix.data, primaryMetric, testType]);
  const loading = summary.loading || dims.loading || matrix.loading;
  const degraded = summary.degraded || dims.degraded || matrix.degraded;
  const empty = !loading && !degraded && rows.length === 0;
  const tick = rechartsTickStyle();

  const subtitle = cat.kind === 'newcat' && primaryMetric
    ? `Models ranked by ${metricLabel(primaryMetric)} capability`
    : cat.kind === 'persona'
      ? 'Models ranked by mean assistant-capability score'
      : `Models ranked by mean ${cat.legacySuite}-suite pass rate`;

  const columns: DataTableColumn<RankRow>[] = [
    { key: 'rank', header: '#', align: 'right', render: r => String(rows.indexOf(r) + 1) },
    { key: 'model', header: 'Model', render: r => r.model },
    { key: 'value', header: primaryMetric ? metricLabel(primaryMetric) : 'Mean', align: 'right', render: r => formatMetricValue(primaryMetric ?? '', r.value) },
    { key: 'score', header: 'Capability', align: 'right', render: r => r.score.toFixed(3) },
  ];

  return (
    <ChartCard
      title="Ranking"
      subtitle={subtitle}
      controls={<TableViewControls view={view} onChange={setView} />}
      height={Math.max(180, rows.length * 34 + 48)}
      loading={loading}
      degraded={degraded}
      empty={empty}
      emptyMessage="No models to rank for this category yet"
      emptyHint="Ranking appears once at least one model is profiled"
    >
      <TableView view={view} columns={columns} rows={rows} rowKey={r => r.model}>
        <ResponsiveContainer width="100%" height={Math.max(160, rows.length * 34 + 30)}>
          <BarChart data={rows} layout="vertical" margin={{ top: 4, right: 48, bottom: 4, left: 8 }}>
            <CartesianGrid {...rechartsGridProps()} horizontal={false} />
            <XAxis type="number" domain={[0, 1]} tick={tick} />
            <YAxis type="category" dataKey="model" width={132} tick={tick} />
            <Tooltip
              cursor={{ fill: 'var(--accent-soft)' }}
              content={({ active, payload }) => {
                if (!active || !payload?.length) return null;
                const r = payload[0]?.payload as RankRow | undefined;
                if (!r) return null;
                return (
                  <ChartTooltip
                    title={r.model}
                    rows={[
                      { key: 'v', label: primaryMetric ? metricLabel(primaryMetric) : 'Mean', value: formatMetricValue(primaryMetric ?? '', r.value) },
                      { key: 's', label: 'Capability', value: r.score.toFixed(3) },
                    ]}
                  />
                );
              }}
            />
            <Bar dataKey="score" radius={[0, 3, 3, 0]} isAnimationActive={false}>
              {rows.map((r, i) => (
                <Cell key={r.model} fill={i < CATEGORICAL_HEX.length ? CATEGORICAL_HEX[i] : CHART_CHROME.deemphasis} />
              ))}
            </Bar>
          </BarChart>
        </ResponsiveContainer>
      </TableView>
    </ChartCard>
  );
}

// ── Section 5: failures + run history ────────────────────────────────────────────

function FailuresSection({ cat }: { cat: CategoryMeta }) {
  const catFailures = useMintSection(
    cat.kind === 'newcat' ? c => c.mint.categoryFailures(cat.clientKey!) : null,
    `fail-cat:${cat.id}`,
  );
  // `mint.failures()` counts CODE-run failure classes fleet-wide — it is not scoped per legacy
  // suite, so it is honest only for the code suite and the (explicitly aggregate) persona view.
  // Context/agent get a clear not-applicable state rather than borrowed code-failure counts.
  const failuresApplies = cat.kind === 'newcat' || cat.kind === 'persona' || (cat.kind === 'legacy' && cat.legacySuite === 'code');
  const legacyFailures = useMintSection(
    (cat.kind === 'persona' || (cat.kind === 'legacy' && cat.legacySuite === 'code')) ? c => c.mint.failures() : null,
    `fail-leg:${cat.id}`,
  );
  const { view, setView } = useTableView();

  const vm: FailuresVM = useMemo(
    () => (cat.kind === 'newcat' ? failuresFromCategory(catFailures.data) : failuresFromLegacy(legacyFailures.data)),
    [cat.kind, catFailures.data, legacyFailures.data],
  );
  const loading = catFailures.loading || legacyFailures.loading;
  const degraded = catFailures.degraded || legacyFailures.degraded;
  const empty = !loading && !degraded && vm.models.length === 0;
  const tick = rechartsTickStyle();

  // NB: all hooks must run unconditionally — keep this useMemo ABOVE the early return below.
  const classColors = useMemo(() => {
    const assigner = new SlotAssigner();
    const map: Record<string, string> = {};
    vm.classes.forEach(cls => { map[cls] = assigner.colorFor(cls); });
    return map;
  }, [vm.classes]);

  if (!failuresApplies) {
    return (
      <ChartCard title="Failure Classes" height={96} empty emptyMessage="No suite-scoped failure classes" emptyHint={`Failure-class counts are recorded for the code suite only, not the ${cat.legacySuite} suite — see Recent Runs below`}>
        <div />
      </ChartCard>
    );
  }

  const chartData = vm.models.map(m => ({ model: m.model, ...m.counts }));

  const columns: DataTableColumn<FailuresVM['models'][number]>[] = [
    { key: 'model', header: 'Model', render: r => r.model },
    ...vm.classes.map(cls => ({
      key: cls, header: cls, align: 'right' as const, render: (r: FailuresVM['models'][number]) => String(r.counts[cls] ?? 0),
    })),
    { key: 'total', header: 'Runs', align: 'right', render: r => String(r.total) },
  ];

  return (
    <ChartCard
      title="Failure Classes"
      subtitle={cat.kind === 'persona' ? 'Fleet-wide run outcomes by class (aggregate)' : 'Per-model run outcomes by class'}
      controls={<TableViewControls view={view} onChange={setView} />}
      height={Math.max(180, vm.models.length * 40 + 60)}
      loading={loading}
      degraded={degraded}
      empty={empty}
      emptyMessage="No failure data for this category yet"
      emptyHint="Outcome classes accumulate as runs complete"
      footer={<ChartLegend entries={vm.classes.map(cls => ({ id: cls, label: cls, color: classColors[cls] }))} />}
    >
      <TableView view={view} columns={columns} rows={vm.models} rowKey={r => r.model}>
        <ResponsiveContainer width="100%" height={Math.max(160, vm.models.length * 40 + 40)}>
          <BarChart data={chartData} layout="vertical" margin={{ top: 4, right: 24, bottom: 4, left: 8 }}>
            <CartesianGrid {...rechartsGridProps()} horizontal={false} />
            <XAxis type="number" tick={tick} />
            <YAxis type="category" dataKey="model" width={132} tick={tick} />
            <Tooltip
              cursor={{ fill: 'var(--accent-soft)' }}
              content={({ active, payload, label }) => {
                if (!active || !payload?.length) return null;
                return (
                  <ChartTooltip
                    title={String(label)}
                    rows={payload.map(p => ({
                      key: String(p.dataKey), label: String(p.dataKey), value: String(p.value),
                      color: typeof p.color === 'string' ? p.color : undefined,
                    }))}
                  />
                );
              }}
            />
            {vm.classes.map(cls => (
              <Bar key={cls} dataKey={cls} stackId="f" fill={classColors[cls]} isAnimationActive={false} />
            ))}
          </BarChart>
        </ResponsiveContainer>
      </TableView>
    </ChartCard>
  );
}

function RunsSection({ cat }: { cat: CategoryMeta }) {
  const suite = cat.kind === 'newcat' ? cat.clientKey! : cat.legacySuite;
  const runs = useMintSection(
    suite ? c => c.mint.runs({ suite }) : null,
    `runs:${cat.id}`,
  );
  const rows: MintRunRow[] = runs.data?.runs ?? [];
  const empty = !runs.loading && !runs.degraded && rows.length === 0;

  const columns: DataTableColumn<MintRunRow>[] = [
    { key: 'model', header: 'Model', render: r => String(r.model ?? '—') },
    { key: 'metric', header: 'Metric', render: r => (r.metric ? metricLabel(String(r.metric)) : '—') },
    { key: 'value', header: 'Value', align: 'right', render: r => (typeof r.value === 'number' ? formatMetricValue(String(r.metric ?? ''), r.value) : '—') },
    { key: 'backend', header: 'Backend', render: r => String(r.backend_tag ?? '—') },
    { key: 'conf', header: 'Conf.', render: r => (r.low_confidence ? <Badge tone="amber">low</Badge> : <Badge tone="green">ok</Badge>) },
    { key: 'when', header: 'When', render: r => (r.created_at ? new Date(String(r.created_at)).toLocaleString() : '—') },
  ];

  return (
    <ChartCard
      title="Recent Runs"
      subtitle={suite ? `Latest ${suite} run rows` : 'Run history'}
      height={260}
      loading={runs.loading}
      degraded={runs.degraded}
      empty={empty}
      emptyMessage="No runs recorded for this category yet"
      emptyHint="Individual profiling runs land here as they complete"
    >
      <DataTable columns={columns} rows={rows} rowKey={(r, i) => String(r.run_id ?? i)} emptyMessage="No runs yet" />
    </ChartCard>
  );
}

// ── Panel ─────────────────────────────────────────────────────────────────────

export function CategoryReportPanel() {
  const [selected, setSelected] = useState<string>(DEFAULT_CATEGORY_ID);
  const cat = categoryById(selected) ?? MINT_CATEGORY_META[0];

  return (
    <PanelRoot style={{ padding: 'var(--space-5)', display: 'flex', flexDirection: 'column', gap: 'var(--space-4)' }}>
      <CardTitle subtitle="Per-category benchmark reports — live MINT profiling from the fleet database">
        MINT — Category Reports
      </CardTitle>

      <CategoryPicker selected={selected} onSelect={setSelected} />

      <div style={{ display: 'flex', alignItems: 'baseline', gap: 'var(--space-2)', flexWrap: 'wrap' }}>
        <span style={{ fontSize: 'var(--fs-h3)', fontWeight: 'var(--fw-semibold)', color: 'var(--text-100)' }}>{cat.label}</span>
        <Badge tone={cat.kind === 'newcat' ? 'violet' : cat.kind === 'legacy' ? 'blue' : 'green'} mono>{cat.kind}</Badge>
        <span style={{ fontSize: 'var(--fs-sm)', color: 'var(--text-muted)' }}>{cat.blurb}</span>
      </div>

      {/* key on category id so every section refetches cleanly on a category switch */}
      <div key={cat.id} style={{ display: 'flex', flexDirection: 'column', gap: 'var(--space-4)' }}>
        <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(360px, 1fr))', gap: 'var(--space-4)' }}>
          <RadarSection cat={cat} />
          <HeatmapSection cat={cat} />
        </div>
        <RankingSection cat={cat} />
        <DistributionSection cat={cat} />
        <FailuresSection cat={cat} />
        <RunsSection cat={cat} />
      </div>
    </PanelRoot>
  );
}
