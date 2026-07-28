// CGUI-08 (TERM #531): TypeScript response shapes for the Models/MINT read API
// (`crate::constellation::models_api`, wired in CONST-21 + CGUI-07). These are the
// exact JSON shapes the backend handlers return — the data-client method group in
// `aggregationClient.ts` is typed against them so CGUI-09 (Models module) and CGUI-10
// (MINT module) build on a checked contract rather than `unknown`.
//
// Source of truth: `src/constellation/models_api.rs` at repo root (every `json!({…})`
// there maps to a shape below). Nullable Rust `Option<T>` / `Value::Null` fields are
// `T | null` here; RFC-3339 timestamps are `string`.

// ── The 8 new MINT task-categories (CGUI-07 / TERM #530) ─────────────────────
// The canonical `category` path/`suite` values the category endpoints accept, plus the
// two friendly aliases the backend resolves (`vision_qa`→image_parsing, `stt`→
// voice_transcription). See `newcat_task_category` in models_api.rs.
export type MintCategory =
  | 'embedding_retrieval'
  | 'reranking'
  | 'image_parsing'
  | 'document_parsing'
  | 'image_generation'
  | 'voice_transcription'
  | 'tts'
  | 'tool_routing';

/** Friendly aliases the frontend/spec use for two categories whose display name differs
 *  from their stored `task_category`. Accepted anywhere a {@link MintCategory} is. */
export type MintCategoryAlias = 'vision_qa' | 'stt' | 'asr' | 'asr_transcription';

/** The full ordered list of the 8 new MINT categories (display order, matching
 *  `NEWCAT_TASK_CATEGORIES` in models_api.rs). */
export const MINT_CATEGORIES: readonly MintCategory[] = [
  'embedding_retrieval',
  'reranking',
  'image_parsing',
  'document_parsing',
  'image_generation',
  'voice_transcription',
  'tts',
  'tool_routing',
] as const;

// ── GET /api/terminus/models ─────────────────────────────────────────────────

export interface ModelsListQuery {
  /** `all` (default) | `fleet` | `brochure`. */
  scope?: 'all' | 'fleet' | 'brochure';
  /** Free-text match against model name/family. */
  q?: string;
  /** Discovery-brochure category filter (one of `FleetCategory::ALL`). */
  category?: string;
  /** Discovery-brochure status filter. */
  status?: string;
  /** Keep-warm (serving-now) filter. */
  serving?: boolean;
  /** Page size, default 50, clamped `[1, 500]`. */
  limit?: number;
  offset?: number;
}

export interface ModelCoverage {
  coder: boolean;
  assistant: boolean;
  serving: boolean;
  agent: boolean;
}

export interface ModelListEntry {
  model_name: string;
  family: string | null;
  params_b: number | null;
  quant: string | null;
  category: string | null;
  brochure_status: string | null;
  in_current_fleet: boolean;
  discovery_score: number | null;
  vram_gb: number | null;
  size_b: number | null;
  serving_now: boolean;
  coverage: ModelCoverage;
  best_pass_rate: number | null;
  last_run_at: string | null;
}

export interface ModelsListResponse {
  total: number;
  refreshed_at: string | null;
  models: ModelListEntry[];
}

// ── GET /api/terminus/models/:name ───────────────────────────────────────────

export interface ModelQuantInfo {
  vram_gb: number | null;
  quality_penalty: number | null;
}

export interface ModelIdentity {
  family: string;
  params_b: number | null;
  active_b: number | null;
  architecture: string | null;
  quants: Record<string, ModelQuantInfo>;
  quality: unknown;
  best_for: unknown;
  avoid_for: unknown;
  ollama_name: string | null;
  notes: string | null;
}

export interface ModelBrochureDetail {
  hf_repo: string | null;
  category: string;
  status: string;
  gfx1151_class: string | null;
  size_b: number | null;
  vram_footprint_gb: number | null;
  discovery_source: string | null;
  discovery_score: number | null;
  discovered_at: string | null;
  last_seen_at: string | null;
  fetched_at: string | null;
  marked_for_fleet_at: string | null;
  evicted_at: string | null;
  rationale: string | null;
}

export interface ModelServingRow {
  backend_tag: string;
  best_runtime: string | null;
  tok_s: number | null;
  vram_or_ram_peak_gb: number | null;
  cold_load_s: number | null;
  keep_warm: boolean;
  fallback_runtime: string | null;
  exclusion_reason: string | null;
  recheck_trigger: string | null;
  provenance: string | null;
  updated_at: string | null;
}

export interface ModelOperationalProfile {
  max_context_safe: number | null;
  max_context_absolute: number | null;
  quality_degradation_point: number | null;
  throughput_at_2k: number | null;
  throughput_at_8k: number | null;
  throughput_at_16k: number | null;
  throughput_at_32k: number | null;
  throughput_at_64k: number | null;
  recommended_timeout_chat_sec: number | null;
  recommended_timeout_build_sec: number | null;
  recommended_timeout_deep_sec: number | null;
  overall_tier: string | null;
}

export interface ModelCatalogCell {
  test_type: string;
  task_category: string;
  quant: string | null;
  status: string;
  pass_rate: number | null;
  n_samples: number | null;
  score_stddev: number | null;
  low_confidence: boolean;
  last_run_at: string | null;
  harness_version: string | null;
}

export interface ModelCatalogDetail {
  card: {
    model_name: string;
    quant: string | null;
    in_current_fleet: boolean;
    serving: unknown;
    not_run_count: number;
    stale_count: number;
    refreshed_at: string | null;
  };
  cells: ModelCatalogCell[];
}

export interface ModelDetailResponse {
  identity: ModelIdentity | null;
  brochure: ModelBrochureDetail | null;
  serving: ModelServingRow[];
  operational: ModelOperationalProfile | null;
  catalog: ModelCatalogDetail | null;
  /** Present only on the DB-unreachable degraded branch (static identity only). */
  note?: string;
}

// ── GET /api/terminus/mint/summary ───────────────────────────────────────────

export interface MintSummaryResponse {
  models_profiled: number;
  runs: { code: number; context: number; agent: number; total: number };
  fleet_best_model: { model: string; pass_hat_3: number } | null;
  gpu_hours: number;
  epoch: string;
  became_current_at: string | null;
}

// ── GET /api/terminus/mint/dimensions ────────────────────────────────────────

export interface MintDimensionScore {
  dimension: string;
  norm: number | null;
  raw: number | null;
  metric: string;
  std_dev: number | null;
  n: number;
  low_confidence: boolean;
}

export interface MintDimensionsResponse {
  dimensions: string[];
  models: Array<{ model_id: string; scores: MintDimensionScore[] }>;
  fleet_median: Array<{ dimension: string; norm: number | null }>;
}

// ── GET /api/terminus/mint/matrix ────────────────────────────────────────────

export interface MintMatrixColumn {
  test_type: string;
  task_category: string;
}

export interface MintMatrixCell {
  model: string;
  col: MintMatrixColumn;
  status: string;
  pass_rate: number | null;
  n_samples: number | null;
  score_stddev: number | null;
  low_confidence: boolean;
  last_run_at: string | null;
  harness_version: string | null;
}

export interface MintMatrixResponse {
  models: string[];
  columns: MintMatrixColumn[];
  cells: MintMatrixCell[];
}

// ── GET /api/terminus/mint/runs?suite= ───────────────────────────────────────

export interface MintRunsQuery {
  /** `code` | `context` | `agent` | a {@link MintCategory} | {@link MintCategoryAlias}. */
  suite?: 'code' | 'context' | 'agent' | MintCategory | MintCategoryAlias;
  model?: string;
  task_category?: string;
  language?: string;
  failure_class?: string;
  /** Exact-metric filter, only meaningful for a new-MINT-category `suite`. */
  metric?: string;
  epoch?: string;
  limit?: number;
  offset?: number;
}

/** One raw run row. Fields vary by suite (code/context/agent readers vs. the widened
 *  new-category reader over `assistant_dimension_score`) — every field is therefore
 *  optional; a consumer keys on `suite` to know which are populated. The new-category
 *  row shape (see `newcat_row_json`) carries `run_id`/`model`/`metric`/`value`/…. */
export interface MintRunRow {
  run_id?: string;
  model?: string;
  backend_tag?: string;
  dimension?: string;
  metric?: string;
  value?: number;
  std_dev?: number | null;
  judge?: string;
  low_confidence?: boolean;
  created_at?: string;
  harness_version?: string | null;
  [key: string]: unknown;
}

export interface MintRunsResponse {
  total: number;
  runs: MintRunRow[];
}

// ── GET /api/terminus/mint/box ───────────────────────────────────────────────

export interface MintBoxOutlier {
  run_id: string | number;
  value: number;
  case_id?: string | null;
  failure_class?: string | null;
}

export interface MintBoxGroup {
  model: string;
  min: number;
  q1: number;
  median: number;
  q3: number;
  max: number;
  n: number;
  low_n: boolean;
  outliers: MintBoxOutlier[];
}

export interface MintBoxResponse {
  groups: MintBoxGroup[];
}

// ── GET /api/terminus/mint/language-stats ────────────────────────────────────

export interface MintLanguageStatRow {
  model: string;
  language: string;
  n_scored: number | null;
  mean_score: number | null;
  stddev_score: number | null;
  retry_lift: number | null;
  mean_throughput: number | null;
  mean_latency_ms: number | null;
  p95_latency_ms: number | null;
  total_gpu_seconds: number | null;
  quality_per_gpu_second: number | null;
  pass_hat_3: number | null;
  vram_gb: number | null;
  /** Server-computed √-scaled 8-to-24 px point size for the C4 Pareto scatter. */
  point_size_px: number;
}

export interface MintLanguageStatsResponse {
  rows: MintLanguageStatRow[];
}

// ── GET /api/terminus/mint/failures ──────────────────────────────────────────

export interface MintFailureModel {
  model: string;
  counts: Record<string, number>;
  total_runs: number;
}

export interface MintFailuresResponse {
  /** top-5 failure classes fleet-wide + a trailing `"other"` bucket. */
  classes: string[];
  models: MintFailureModel[];
}

// ── GET /api/terminus/mint/context-profiles ──────────────────────────────────

export interface MintContextTier {
  context_tokens: number;
  throughput_tok_per_sec: number | null;
  ttft_ms: number | null;
  recall_score: number | null;
  memory_usage_mb: number | null;
  oom: boolean;
}

export interface MintContextProfileModel {
  model: string;
  max_context_safe: number | null;
  tiers: MintContextTier[];
}

export interface MintContextProfilesResponse {
  models: MintContextProfileModel[];
}

// ── GET /api/terminus/mint/activity ──────────────────────────────────────────

export interface MintActivityDay {
  date: string;
  code: number;
  context: number;
  agent: number;
}

export interface MintActivityEpoch {
  epoch: string;
  became_current_at: string | null;
  note: string | null;
}

export interface MintActivityResponse {
  days: MintActivityDay[];
  epochs: MintActivityEpoch[];
}

// ── GET /api/terminus/mint/category/:category/summary ─────────────────────────

export interface MintCategoryMetric {
  dimension: string;
  metric: string;
  value: number;
  std_dev: number | null;
  low_confidence: boolean;
  backend_tag: string;
  last_run_at: string;
}

export interface MintCategorySummaryResponse {
  models: Array<{ model_id: string; metrics: MintCategoryMetric[] }>;
}

// ── GET /api/terminus/mint/category/:category/dimensions ──────────────────────

export interface MintCategoryDimensionsResponse {
  dimensions: Array<{ dimension: string; metric: string }>;
}

// ── GET /api/terminus/mint/category/:category/matrix ──────────────────────────

export interface MintCategoryMatrixCell {
  model: string;
  metric: string;
  dimension: string;
  mean: number;
  n: number;
  low_confidence: boolean;
  last_run_at: string;
}

export interface MintCategoryMatrixResponse {
  models: string[];
  columns: string[];
  cells: MintCategoryMatrixCell[];
}

// ── GET /api/terminus/mint/category/:category/box ─────────────────────────────

export interface MintCategoryBoxOutlier {
  run_id: string;
  value: number;
  low_confidence: boolean;
}

export interface MintCategoryBoxGroup {
  model: string;
  min: number;
  q1: number;
  median: number;
  q3: number;
  max: number;
  n: number;
  low_n: boolean;
  outliers: MintCategoryBoxOutlier[];
}

export interface MintCategoryBoxResponse {
  metric: string | null;
  groups: MintCategoryBoxGroup[];
}

// ── GET /api/terminus/mint/category/:category/failures ────────────────────────

export interface MintCategoryFailureModel {
  model: string;
  counts: { low_confidence: number; ok: number };
  total_runs: number;
}

export interface MintCategoryFailuresResponse {
  classes: string[];
  models: MintCategoryFailureModel[];
}
