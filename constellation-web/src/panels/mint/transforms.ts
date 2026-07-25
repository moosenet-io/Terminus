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
      quality: metricUnitScore(c.metric, c.mean),
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

/** Heatmap from the fleet-wide legacy matrix (model × test_type/task_category, colored by
 *  pass_rate which is already a 0–1 capability score). */
export function heatmapFromLegacyMatrix(matrix: MintMatrixResponse | null): HeatmapVM {
  const models = matrix?.models ?? [];
  const metrics = (matrix?.columns ?? []).map(c => `${c.test_type}/${c.task_category}`);
  const cell: HeatmapVM['cell'] = {};
  for (const m of models) cell[m] = {};
  for (const c of matrix?.cells ?? []) {
    const key = `${c.col.test_type}/${c.col.task_category}`;
    if (!cell[c.model]) cell[c.model] = {};
    cell[c.model][key] = {
      value: c.pass_rate,
      quality: c.pass_rate,
      n: c.n_samples,
      lowConfidence: c.low_confidence,
    };
  }
  return { models, metrics, cell };
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
