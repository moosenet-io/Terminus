// CONST-22: Model Library UI — shared types for the `models` module (spec §6/§8).
//
// These mirror CONST-21's read-API response shapes (§8 of docs/constellation/CONST-GUI-SPEC.md)
// EXACTLY, so binding to the live API once CONST-21 merges is a no-op — only the mock fixtures
// in aggregationClient.ts change (mock -> http adapter selection), never these types or the
// panels/hooks that consume them.
//
// Two fields go beyond the literal §8 endpoint sketch (`size_bucket`, `coverage` as *request*
// query params on `GET /api/terminus/models`): §6.1's filter row explicitly requires both as
// filters, but the §8 sketch's query-param list only names `scope/q/category/status/serving/
// limit/offset`. Treated here as an additive, backward-compatible extension of that contract
// (plain optional string params) rather than a deviation from it — flagged in the PR description
// per the acceptance criteria ("spot-check list").

/** Brochure state-machine status (contracts-to-confirm #4: full enum resolved at CONST-21
 *  build time; this is the assumed 8-stage set the mock/UI builds against). */
export type BrochureStatus =
  | 'discovered'
  | 'evaluating'
  | 'evaluated'
  | 'shortlisted'
  | 'adopted'
  | 'deprecated'
  | 'rejected'
  | 'archived';

/** The model-category 8-value enum (§6.1 filter row, §8 contracts-to-confirm #4). */
export type ModelCategory =
  | 'coder'
  | 'assistant'
  | 'agent'
  | 'reasoning'
  | 'vision'
  | 'embedding'
  | 'creative'
  | 'tool-use';

export type SizeBucket = '<4B' | '4-10B' | '10-35B' | '>35B';

export type CoverageState = 'covered' | 'partial' | 'none';

export interface CoverageCells {
  coder: CoverageState;
  assistant: CoverageState;
  serving: CoverageState;
  agent: CoverageState;
}

export interface BrochureTimelineEntry {
  status: BrochureStatus;
  at: string;
  note?: string;
}

/** One row of `models.browse`'s DataTable (§6.1) — `GET /api/terminus/models` list item. */
export interface ModelListItem {
  model_name: string;
  family?: string;
  params_b?: number;
  quant?: string | null;
  category?: ModelCategory;
  brochure_status?: BrochureStatus;
  in_current_fleet: boolean;
  discovery_score?: number;
  vram_gb?: number;
  size_b?: number;
  serving_now: boolean;
  coverage: CoverageCells;
  best_pass_rate?: number;
  last_run_at?: string;
}

export interface ModelsListResponse {
  total: number;
  refreshed_at: string;
  models: ModelListItem[];
}

export type ModelsScope = 'fleet' | 'brochure' | 'all';

export interface ModelsListParams {
  scope?: ModelsScope;
  q?: string;
  category?: ModelCategory;
  brochure_status?: BrochureStatus;
  size_bucket?: SizeBucket;
  coverage?: keyof CoverageCells;
  serving?: boolean;
  limit?: number;
  offset?: number;
}

// ── `models.detail` (`GET /api/terminus/models/{name}`) ─────────────────────

export interface QuantRow {
  quant: string;
  vram_gb: number;
  quality_penalty: number;
}

export interface ModelIdentity {
  model_name: string;
  family?: string;
  params_b?: number;
  quants: QuantRow[];
  best_for: string[];
  avoid_for: string[];
  notes?: string;
}

export interface ModelBrochure {
  hf_repo?: string;
  category?: ModelCategory;
  status: BrochureStatus;
  timeline: BrochureTimelineEntry[];
  discovery_score?: number;
  rationale?: string;
}

export interface ServingProfile {
  backend_tag: string;
  best_runtime: string;
  tok_s: number;
  vram_peak_gb: number;
  cold_load_s: number;
  keep_warm: boolean;
  /** 'none' when not excluded; any other value renders the exclusion-reason status badge. */
  exclusion_reason: string;
}

export type Tier = 'hot' | 'warm' | 'cold';

export interface OperationalProfile {
  max_context_safe: number;
  max_context_absolute: number;
  degradation_point?: number;
  throughput_strip: number[];
  tier: Tier;
}

export interface CatalogCard {
  best_pass_rate?: number;
  last_run_at?: string;
}

export interface ModelDetailResponse {
  /** Absent sources are `null` — every section below degrades independently (§6.1). */
  identity: ModelIdentity | null;
  brochure: ModelBrochure | null;
  serving: ServingProfile[] | null;
  operational: OperationalProfile | null;
  catalog: { card: CatalogCard; cells: MintMatrixCell[] } | null;
}

// ── MINT read-API slices used by the Models module (§8) ─────────────────────

export interface MintDimensionScore {
  dimension: string;
  norm: number;
  raw: number;
  metric: string;
  std_dev: number;
  n: number;
  low_confidence: boolean;
}

export interface MintDimensionsModel {
  model_id: string;
  scores: MintDimensionScore[];
}

export interface MintDimensionsResponse {
  dimensions: string[];
  models: MintDimensionsModel[];
  /** norm values, aligned index-for-index with `dimensions`. */
  fleet_median: number[];
}

export interface MintMatrixCell {
  model: string;
  col: { test_type: string; task_category: string };
  status: string;
  pass_rate: number;
  n_samples: number;
  score_stddev: number;
  low_confidence: boolean;
  last_run_at?: string;
  harness_version?: string;
}
