// CGUI-10 (TERM #533): pure normalizers that fold each MINT read-API response shape into the
// small set of source-agnostic view-models the report's chart sections render. Keeping this
// pure (no React, no fetch) means the newcat vs. legacy vs. persona divergence is resolved in
// ONE tested place, and every chart component stays a dumb renderer of a normalized VM.
//
// Fail-open contract (matches the backend's empty-200 for an un-profiled category): every
// function tolerates empty / null-heavy inputs and returns an empty VM rather than throwing.
import type {
  MintCategorySummaryResponse,
  MintCategoryMatrixResponse,
  MintCategoryBoxResponse,
  MintCategoryFailuresResponse,
  MintDimensionsResponse,
  MintMatrixResponse,
  MintBoxResponse,
  MintFailuresResponse,
} from '../../types/mint';
import { metricUnitScore } from './categoryMeta';

// ── View-models ───────────────────────────────────────────────────────────────

export interface RadarVM {
  /** Axis ids (metrics or assistant-dimensions). */
  axes: string[];
  /** One entry per model; `values[i]` is the unit score [0,1] for `axes[i]`. */
  series: Array<{ model: string; values: number[] }>;
}

export interface HeatCell {
  /** Raw metric value (native units), or null when not run. */
  value: number | null;
  /** Unit capability score [0,1] used for the color ramp, or null when not run. */
  quality: number | null;
  n: number | null;
  lowConfidence: boolean;
}

export interface HeatmapVM {
  models: string[];
  metrics: string[];
  /** `cell[model][metric]`. */
  cell: Record<string, Record<string, HeatCell>>;
}

export interface BoxGroupVM {
  model: string;
  min: number;
  q1: number;
  median: number;
  q3: number;
  max: number;
  n: number;
  lowN: boolean;
  outliers: number[];
}

export interface BoxVM {
  metric: string | null;
  groups: BoxGroupVM[];
}

export interface RankRow {
  model: string;
  /** Primary-metric raw value. */
  value: number | null;
  /** Primary-metric unit score [0,1] the bar length encodes. */
  score: number;
}

export interface FailuresVM {
  classes: string[];
  models: Array<{ model: string; counts: Record<string, number>; total: number }>;
}

// ── newcat (per-category endpoints) ─────────────────────────────────────────────

export function radarFromCategory(summary: MintCategorySummaryResponse | null): RadarVM {
  const models = summary?.models ?? [];
  // Axis order = the metric order of the first model that has metrics (stable across models).
  const axes = models.find(m => m.metrics.length > 0)?.metrics.map(m => m.metric) ?? [];
  const series = models.map(m => ({
    model: m.model_id,
    values: axes.map(ax => {
      const met = m.metrics.find(x => x.metric === ax);
      return met ? metricUnitScore(met.metric, met.value) : 0;
    }),
  }));
  return { axes, series };
}

export function heatmapFromCategory(matrix: MintCategoryMatrixResponse | null): HeatmapVM {
  const models = matrix?.models ?? [];
  const metrics = matrix?.columns ?? [];
  const cell: HeatmapVM['cell'] = {};
  for (const m of models) cell[m] = {};
  for (const c of matrix?.cells ?? []) {
    if (!cell[c.model]) cell[c.model] = {};
    cell[c.model][c.metric] = {
      value: c.mean,
      // null mean = not run → quality null (rendered as an EMPTY cell, excluded from the
      // color scale) rather than 0 (which would paint a not-run cell as a bad score).
      quality: c.mean == null ? null : metricUnitScore(c.metric, c.mean),
      n: c.n,
      lowConfidence: c.low_confidence,
    };
  }
  return { models, metrics, cell };
}

export function boxFromCategory(box: MintCategoryBoxResponse | null): BoxVM {
  return {
    metric: box?.metric ?? null,
    groups: (box?.groups ?? []).map(g => ({
      model: g.model,
      min: g.min, q1: g.q1, median: g.median, q3: g.q3, max: g.max,
      n: g.n, lowN: g.low_n,
      outliers: g.outliers.map(o => o.value),
    })),
  };
}

export function failuresFromCategory(f: MintCategoryFailuresResponse | null): FailuresVM {
  return {
    classes: f?.classes ?? [],
    models: (f?.models ?? []).map(m => ({ model: m.model, counts: { ...m.counts }, total: m.total_runs })),
  };
}

/** Ranking rows for a category, sorted best-first by the primary metric's unit score. */
export function rankingFromCategory(summary: MintCategorySummaryResponse | null, primaryMetric: string | null): RankRow[] {
  const models = summary?.models ?? [];
  const metric = primaryMetric ?? models.find(m => m.metrics.length > 0)?.metrics[0]?.metric ?? null;
  const rows: RankRow[] = models.map(m => {
    const met = metric ? m.metrics.find(x => x.metric === metric) : undefined;
    const value = met ? met.value : null;
    return { model: m.model_id, value, score: met ? metricUnitScore(met.metric, met.value) : 0 };
  });
  return rows.sort((a, b) => b.score - a.score);
}

/** The metric ids available in a category summary (for the box/heatmap metric picker). */
export function metricsOfCategory(summary: MintCategorySummaryResponse | null): string[] {
  const first = (summary?.models ?? []).find(m => m.metrics.length > 0);
  return first?.metrics.map(m => m.metric) ?? [];
}

// ── legacy suites + persona (fleet-wide endpoints) ──────────────────────────────

/** Assistant capability radar from `mint.dimensions()` — uses the server-provided `norm`
 *  (already in [0,1]); null norms fall to 0 so the axis never breaks. */
export function radarFromDimensions(dims: MintDimensionsResponse | null): RadarVM {
  const axes = dims?.dimensions ?? [];
  const series = (dims?.models ?? []).map(m => ({
    model: m.model_id,
    values: axes.map(ax => {
      const s = m.scores.find(x => x.dimension === ax);
      return s && s.norm != null ? s.norm : 0;
    }),
  }));
  return { axes, series };
}

/**
 * Heatmap from the fleet-wide legacy matrix (model × test_type/task_category, colored by
 * pass_rate which is already a 0–1 capability score). When `testType` is given the matrix is
 * SCOPED to that test_type (`coder` for the code suite, `agent` for the agent suite) so each
 * legacy category tab shows genuinely suite-specific columns rather than the same fleet
 * aggregate. Columns are then labeled by their task_category alone (the test_type is implied by
 * the scoped tab). A null pass_rate is a not-run cell → quality null (empty), never a 0 score.
 */
export function heatmapFromLegacyMatrix(matrix: MintMatrixResponse | null, testType?: string): HeatmapVM {
  const cols = (matrix?.columns ?? []).filter(c => !testType || c.test_type === testType);
  const metrics = cols.map(c => (testType ? c.task_category : `${c.test_type}/${c.task_category}`));
  // Only models that have at least one cell in the scoped columns appear as rows.
  const scopedCells = (matrix?.cells ?? []).filter(c => !testType || c.col.test_type === testType);
  const models = Array.from(new Set(scopedCells.map(c => c.model)));
  const cell: HeatmapVM['cell'] = {};
  for (const m of models) cell[m] = {};
  for (const c of scopedCells) {
    const key = testType ? c.col.task_category : `${c.col.test_type}/${c.col.task_category}`;
    if (!cell[c.model]) cell[c.model] = {};
    cell[c.model][key] = {
      value: c.pass_rate,
      quality: c.pass_rate == null ? null : c.pass_rate,
      n: c.n_samples,
      lowConfidence: c.low_confidence,
    };
  }
  return { models, metrics, cell };
}

/** Capability radar for a legacy suite, scoped by test_type: axes = the suite's task_categories,
 *  each model's value = its pass_rate on that column (0–1, null → 0). Differs per suite. */
export function radarFromLegacyMatrix(matrix: MintMatrixResponse | null, testType: string): RadarVM {
  const cols = (matrix?.columns ?? []).filter(c => c.test_type === testType);
  const axes = cols.map(c => c.task_category);
  const scopedCells = (matrix?.cells ?? []).filter(c => c.col.test_type === testType);
  const models = Array.from(new Set(scopedCells.map(c => c.model)));
  const byModel = new Map<string, Map<string, number | null>>();
  for (const c of scopedCells) {
    if (!byModel.has(c.model)) byModel.set(c.model, new Map());
    byModel.get(c.model)!.set(c.col.task_category, c.pass_rate);
  }
  const series = models.map(model => ({
    model,
    values: axes.map(tc => {
      const v = byModel.get(model)?.get(tc);
      return v == null ? 0 : v;
    }),
  }));
  return { axes, series };
}

/** Ranking rows for a legacy suite, scoped by test_type: rank models by their MEAN pass_rate
 *  across the suite's columns (null cells excluded from the mean). Differs per suite. */
export function rankingFromLegacyMatrix(matrix: MintMatrixResponse | null, testType: string): RankRow[] {
  const scopedCells = (matrix?.cells ?? []).filter(c => c.col.test_type === testType);
  const byModel = new Map<string, number[]>();
  for (const c of scopedCells) {
    if (c.pass_rate == null) continue;
    if (!byModel.has(c.model)) byModel.set(c.model, []);
    byModel.get(c.model)!.push(c.pass_rate);
  }
  const rows: RankRow[] = Array.from(byModel.entries()).map(([model, vals]) => {
    const mean = vals.length ? vals.reduce((a, b) => a + b, 0) / vals.length : 0;
    return { model, value: Math.round(mean * 1000) / 1000, score: mean };
  });
  return rows.sort((a, b) => b.score - a.score);
}

export function boxFromLegacy(box: MintBoxResponse | null, metric: string | null): BoxVM {
  return {
    metric,
    groups: (box?.groups ?? []).map(g => ({
      model: g.model,
      min: g.min, q1: g.q1, median: g.median, q3: g.q3, max: g.max,
      n: g.n, lowN: g.low_n,
      outliers: g.outliers.map(o => o.value),
    })),
  };
}

export function failuresFromLegacy(f: MintFailuresResponse | null): FailuresVM {
  return {
    classes: f?.classes ?? [],
    models: (f?.models ?? []).map(m => ({ model: m.model, counts: { ...m.counts }, total: m.total_runs })),
  };
}

/** Ranking rows from the assistant radar: mean unit score across all dimensions per model. */
export function rankingFromDimensions(dims: MintDimensionsResponse | null): RankRow[] {
  const rows: RankRow[] = (dims?.models ?? []).map(m => {
    const norms = m.scores.map(s => (s.norm != null ? s.norm : 0));
    const mean = norms.length ? norms.reduce((a, b) => a + b, 0) / norms.length : 0;
    return { model: m.model_id, value: Math.round(mean * 1000) / 1000, score: mean };
  });
  return rows.sort((a, b) => b.score - a.score);
}
