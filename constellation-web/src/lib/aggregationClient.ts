// CONST-04: The aggregation client is the ONLY module in this app allowed to talk to the
// backend. Every other module (hooks, panels, components) goes through the exported
// `client` singleton below — never call `fetch` or read `window.location` directly
// elsewhere in the app (that's an acceptance-criterion grep check, keep it true).
//
// Two implementations of the same typed interface:
//   - mockAdapter: canned in-memory data, no network. Default — lets the app build/run/typecheck
//     with no backend present.
//   - httpAdapter: real same-origin fetch against `/api/{system}/...`, cookie-based session auth.
//
// Selection is via `import.meta.env.VITE_AGG_MODE` ('mock' | 'http'), default 'mock'.
// This is deliberately the *only* seam CONST-02 (the real Terminus-side aggregation layer)
// needs to fill in — the httpAdapter below defines exactly the endpoints/shapes it must serve.

// ── Shared types ────────────────────────────────────────────────────────────

/** The systems the control plane aggregates. Mirrors CONST-01's nav grouping.
 *  `muse` added by CONST-19 (the fourth namespaced proxy arm; UI panels land in CONST-20). */
export type SystemId = 'harmony' | 'chord' | 'lumina' | 'muse' | 'terminus';

/** CONST-27 (§3.4): a session's access tier. `null` when unauthenticated. The UI's `RoleGate`
 *  reads this to disable mutating controls for a viewer — cosmetic only; the server enforces
 *  the same rule structurally (`enforce_viewer_role_gate` — 403 on every mutating method). */
export type AuthRole = 'operator' | 'viewer' | null;

export interface AuthMeResponse {
  authenticated: boolean;
  username: string | null;
  role: AuthRole;
}

export interface HealthStatus {
  system: SystemId;
  available: boolean;
  /** Short human-readable status, e.g. "reachable" | "not deployed" | "error: timeout". */
  detail?: string;
}

export interface TerminusModuleInfo {
  name: string;
  enabled: boolean;
  version?: string;
}

export interface TerminusConfigSummary {
  modules: TerminusModuleInfo[];
  workerCount: number;
}

/** CONST-26: one line of the constellation aggregation layer's mutating-request audit trail,
 *  as surfaced by `GET /api/terminus/activity` — never body content, see that endpoint's doc. */
export interface ActivityEntry {
  /** RFC 3339 UTC timestamp. */
  ts: string;
  method: string;
  path: string;
  principal: string | null;
  system: SystemId | 'auth';
}

export interface ActivityFeedResponse {
  entries: ActivityEntry[];
}

// ── Mutation-result event seam (CONST-26, §3.3) ──────────────────────────────
// `request<T>()` is the ONE call-site every panel/hook already routes a mutating
// (POST/PUT/PATCH/DELETE) backend call through (see the doc comment on `AggregationClient`
// above + this file's grep-gated "single path to the backend" rule) — so this is where the
// activity-feed/toast layer observes "a mutation happened and here's whether it succeeded"
// WITHOUT every existing call site needing to change. Fired by both adapters below, after the
// underlying request settles either way.

export interface MutationResultEvent {
  system: SystemId;
  method: string;
  path: string;
  ok: boolean;
  /** Present only when `ok` is false — a short message suitable for a toast, never a raw
   *  response body (this seam only ever sees success/failure, not payloads). */
  error?: string;
}

type MutationResultListener = (event: MutationResultEvent) => void;

const mutationResultListeners = new Set<MutationResultListener>();

/** Subscribe to every mutating `request<T>()` call's outcome, across BOTH adapters. Returns an
 *  unsubscribe function. Intended for exactly one caller: the toast layer
 *  (`components/Toast.tsx`) — but deliberately a plain subscribe seam (not hardwired to that
 *  module) so this file stays free of a UI-layer import. */
export function onMutationResult(listener: MutationResultListener): () => void {
  mutationResultListeners.add(listener);
  return () => mutationResultListeners.delete(listener);
}

function emitMutationResult(event: MutationResultEvent): void {
  mutationResultListeners.forEach(listener => listener(event));
}

const MUTATING_METHODS = new Set(['POST', 'PUT', 'PATCH', 'DELETE']);

/** Wraps a `request<T>()` implementation so every mutating call emits a
 *  [`MutationResultEvent`] on completion (success or failure), regardless of which adapter
 *  (mock/http) is active. Non-mutating (`GET`, default) calls pass through untouched — the
 *  activity feed cares about "what changed", not every read. */
async function withMutationResultEvent<T>(
  system: SystemId,
  path: string,
  init: RequestInit | undefined,
  run: () => Promise<T>,
): Promise<T> {
  const method = (init?.method ?? 'GET').toUpperCase();
  if (!MUTATING_METHODS.has(method)) {
    return run();
  }
  try {
    const result = await run();
    emitMutationResult({ system, method, path, ok: true });
    return result;
  } catch (e) {
    emitMutationResult({ system, method, path, ok: false, error: e instanceof Error ? e.message : String(e) });
    throw e;
  }
}

/**
 * The single typed entry point for `/api/{harmony,chord,lumina,muse,terminus}/*`.
 * All request/response shapes an adapter must implement.
 */
export interface AggregationClient {
  auth: {
    me(): Promise<AuthMeResponse>;
    login(username: string, password: string): Promise<AuthMeResponse>;
    logout(): Promise<void>;
  };
  health: {
    /** One entry per known system; used to drive module-registry availability + StatusStrip. */
    list(): Promise<HealthStatus[]>;
  };
  terminus: {
    configSummary(): Promise<TerminusConfigSummary>;
    /** CONST-26: the last `limit` (default server-side cap when omitted) mutating-request
     *  activity entries — feeds the Overview activity feed / notification bell (§3.3). */
    activity(limit?: number): Promise<ActivityFeedResponse>;
  };
  /**
   * Generic escape hatch for panel-specific reads that don't yet have a typed method above.
   * Still routed through this client so the "single path to the backend" rule holds even as
   * new panels (CONST-05..12) land ahead of their typed methods being added here.
   */
  request<T>(system: SystemId, path: string, init?: RequestInit): Promise<T>;
  /**
   * CONST-04: The one permitted WebSocket entry point. harmony-web's daemon pushes live
   * engine/ralph-loop/log events over a single same-origin `/ws` socket; this wraps that so
   * no hook or component ever constructs a `WebSocket`/reads `window.location` itself.
   */
  ws: {
    connect(handlers: WsHandlers): WsConnection;
  };
  /** Allowlisted, non-secret localStorage seam — see `PrefsClient` above. Shared by both
   *  adapters: prefs are always browser-local, they never depend on mock vs. http mode. */
  prefs: PrefsClient;
}

export interface WsHandlers {
  onEvent: (event: unknown) => void;
  onOpen?: () => void;
  onClose?: () => void;
}

export interface WsConnection {
  send(data: unknown): void;
  close(): void;
}

// ── Prefs seam (CONST-16, §3.1) ──────────────────────────────────────────────
// The one and only place browser storage may appear in this app (grep-gated). Backs the
// Overview card canvas' persisted layout/density — deliberately NOT a general key-value store:
// only the two allowlisted, non-secret keys below may ever be read or written. Any other key
// (including via a loosely-typed caller) throws rather than silently writing an unreviewed key.

/** The only two keys the prefs seam will ever store — both non-secret UI state. */
export type PrefsKey = 'layout' | 'density';

export interface PrefsClient {
  /** Returns the stored value for an allowlisted key, or `null` if unset/unparsable. */
  get<T>(key: PrefsKey): T | null;
  /** Stores a value for an allowlisted key. Silently no-ops if storage is unavailable
   *  (private-mode/quota) — prefs are a convenience, never load-bearing for correctness. */
  set<T>(key: PrefsKey, value: T): void;
}

// ── Prefs seam implementation ────────────────────────────────────────────────
// Defined here (ahead of both adapters) since each adapter's object literal references
// `prefsClient` directly.

const PREFS_ALLOWLIST: readonly PrefsKey[] = ['layout', 'density'];
const PREFS_STORAGE_PREFIX = 'constellation.prefs.';

function assertAllowedPrefsKey(key: string): asserts key is PrefsKey {
  if (!(PREFS_ALLOWLIST as readonly string[]).includes(key)) {
    throw new Error(
      `prefs: key "${key}" is not allowlisted — only ${PREFS_ALLOWLIST.join(', ')} may be stored`,
    );
  }
}

/** The one `PrefsClient` implementation — shared by mock and http adapters (see the seam
 *  doc comment above). `localStorage` appears nowhere else in this file or the app. */
const prefsClient: PrefsClient = {
  get<T>(key: PrefsKey): T | null {
    assertAllowedPrefsKey(key);
    try {
      const raw = window.localStorage.getItem(`${PREFS_STORAGE_PREFIX}${key}`);
      return raw === null ? null : (JSON.parse(raw) as T);
    } catch {
      return null;
    }
  },
  set<T>(key: PrefsKey, value: T): void {
    assertAllowedPrefsKey(key);
    try {
      window.localStorage.setItem(`${PREFS_STORAGE_PREFIX}${key}`, JSON.stringify(value));
    } catch {
      // Storage unavailable (private mode / quota) — prefs just don't persist this time.
    }
  },
};

// ── Mock adapter ─────────────────────────────────────────────────────────────
// Canned data so the shell builds, runs, and is reviewable with zero backend.

function delay<T>(value: T, ms = 120): Promise<T> {
  return new Promise(resolve => setTimeout(() => resolve(value), ms));
}

const MOCK_HEALTH: HealthStatus[] = [
  { system: 'harmony', available: true, detail: 'mock: reachable' },
  { system: 'chord', available: true, detail: 'mock: reachable' },
  { system: 'lumina', available: true, detail: 'mock: reachable' },
  { system: 'muse', available: true, detail: 'mock: reachable' },
  { system: 'terminus', available: true, detail: 'mock: reachable' },
];

/** CONST-26: canned activity entries so the Overview feed/bell are reviewable with zero
 *  backend — a small, fixed, already-ordered (oldest first, matching the real endpoint's
 *  file-order contract) sample. */
const MOCK_ACTIVITY: ActivityFeedResponse = {
  entries: [
    { ts: new Date(Date.now() - 5 * 60_000).toISOString(), method: 'POST', path: '/api/harmony/engine/restart', principal: 'mock-user', system: 'harmony' },
    { ts: new Date(Date.now() - 2 * 60_000).toISOString(), method: 'PUT', path: '/api/harmony/mode', principal: 'mock-user', system: 'harmony' },
    { ts: new Date(Date.now() - 30_000).toISOString(), method: 'POST', path: '/api/auth/login', principal: 'mock-user', system: 'auth' },
  ],
};

const MOCK_TERMINUS_CONFIG: TerminusConfigSummary = {
  modules: [
    { name: 'gitea', enabled: true, version: '0.4.0' },
    { name: 'plane', enabled: true, version: '0.4.0' },
    { name: 'github', enabled: true, version: '0.4.0' },
    { name: 'nexus', enabled: false },
    { name: 'commute', enabled: false },
  ],
  workerCount: 3,
};

// ── Mock data for the ported harmony-web / Chord surface (CONST-04) ──────────
// Keyed by `${system} ${METHOD} ${pathname}` (query string stripped, dynamic
// segments handled by prefix match below). This is the canned-data contract
// CONST-02's real `/api/{harmony,chord}/*` aggregation endpoints must satisfy.

const MOCK_STATUS = {
  engine_state: 'STOPPED',
  workers: 0,
  projects: [
    {
      identifier: 'LUM', name: 'Lumina Constellation', progress_pct: 62, enrichment_pct: 80,
      counts: { todo: 4, in_progress: 2, done: 9, enriched: 9, enrichable: 11 },
    },
    {
      identifier: 'CHRD', name: 'Chord', progress_pct: 40, enrichment_pct: 55,
      counts: { todo: 6, in_progress: 1, done: 4, enriched: 5, enrichable: 9 },
    },
  ],
  cached: false, cached_ago_secs: 0, loading: false,
  inference_mix: 50, mode: 'local', uptime_seconds: 3600, verify_score: 'N/A',
};

const MOCK_AGENTS = {
  agents: [
    {
      agent_id: 'local-1', provider: 'local', display_name: 'local', model: 'qwen3-coder:30b',
      tier: 'standard', status: 'idle', elapsed_seconds: 0, task: null, loop_state: null,
      active_providers: ['local'],
    },
    {
      agent_id: 'claude-1', provider: 'claude', display_name: 'claude', model: 'sonnet',
      tier: 'standard', status: 'idle', elapsed_seconds: 0, task: null, loop_state: null,
      active_providers: ['claude'],
    },
  ],
};

const MOCK_ESCALATION = {
  total_tasks: 0,
  pass_rate_by_tier: {},
  failure_mode_counts: {},
  complexity_distribution: {},
  enrichment_quality: {},
  problem_specs: [],
};

const MOCK_MODE = {
  mode: 'local', display_name: 'Local', cost: '$0/day', limited: false,
  updated_at: new Date().toISOString(),
};

const MOCK_TREE = { project: '', specs: [], stale: false };

const MOCK_CHORD_HEALTH = {
  engines: [],
  vram: { total_mb: 0, used_mb: 0, free_mb: 0, allocations: [] },
  timestamp: new Date().toISOString(),
};

const MOCK_PROFILES = { profiles: {}, total_outcomes: 0, window_days: 30 };

// ── MINT module types (CONST-23; §8 data contracts) ─────────────────────────
// The real endpoints (`models_api.rs`) are CONST-21's job and not merged yet (per the item
// brief) — these types + the MOCK_MINT_* fixtures below are the exact contract CONST-21 must
// satisfy. All mint reads go through `client.request('terminus', '/mint/...')` (system stays
// 'terminus' — 'mint' is a ModuleId, not a SystemId/proxy namespace: it has no independent
// backend, it's server-side aggregation inside Terminus itself, same as 'models').

export interface MintSummary {
  models_profiled: number;
  runs_this_epoch: number;
  fleet_best: { model: string; pass_hat_3: number } | null;
  gpu_hours: number;
  epoch: string;
}

export interface MintDimensionScore {
  dimension: string;
  norm: number; // 0..1, normalized for radar plotting
  raw: number;
  metric: string;
  std_dev: number;
  n: number;
  low_confidence: boolean;
}

export interface MintModelDimensions {
  model_id: string;
  scores: MintDimensionScore[]; // may be < dimensions.length -> missing dims render at 0, hollow vertex
}

export interface MintDimensionsResponse {
  dimensions: string[]; // 8 fixed assistant dimensions
  models: MintModelDimensions[];
  fleet_median: MintDimensionScore[];
}

export interface MintMatrixColumn {
  key: string; // `${test_type}:${task_category}`
  test_type: string;
  task_category: string;
}

export type MintCellStatus = 'ok' | 'not_run' | 'stale' | 'non_viable';

export interface MintMatrixCell {
  model: string;
  col: string; // MintMatrixColumn.key
  status: MintCellStatus;
  pass_rate: number | null;
  n_samples: number;
  score_stddev: number | null;
  low_confidence: boolean;
  last_run_at: string | null;
  harness_version: string | null;
}

export interface MintMatrixResponse {
  models: string[]; // sorted by mean pass_rate desc
  columns: MintMatrixColumn[];
  cells: MintMatrixCell[];
  /** CONST-23 edge case: code/agent columns are all not_run until INTAKE_CORPUS_V2_DIR is
   *  provisioned — surfaced so the UI can render the truthful copy instead of implying a bug. */
  corpus_dir_unset: boolean;
}

export interface MintContextTierPoint {
  context_tokens: number;
  throughput: number | null; // null once OOM
  recall_score: number | null;
  ttft_ms: number | null;
  memory_usage_mb: number | null;
  oom: boolean;
}

export interface MintContextProfile {
  model: string;
  tiers: MintContextTierPoint[];
  max_context_safe: number;
}

export interface MintContextProfilesResponse {
  profiles: MintContextProfile[];
}

export interface MintActivityDay {
  date: string;
  code: number;
  context: number;
  agent: number;
}

export interface MintEpochMarker {
  epoch: string;
  date: string;
  label: string;
}

export interface MintActivityResponse {
  days: MintActivityDay[];
  epochs: MintEpochMarker[];
}

export interface MintParetoPoint {
  model: string;
  mean_latency_ms: number;
  mean_score: number;
  vram_gb: number;
  p95_latency_ms: number;
  score_stddev: number;
  quality_per_gpu_second: number;
}

export interface MintParetoResponse {
  points: MintParetoPoint[];
}

// ── Mock data for the MINT module (CONST-23) ────────────────────────────────
// Fixture models deliberately cover the required variants (item brief): 'qwen3-coder:30b' is
// the full-data/full-coverage reference model; 'llama3.1:70b' is sparse (< 8 assistant
// dimensions -> missing-axis hollow-vertex case); 'mixtral:8x22b' carries stale + non_viable
// matrix cells; 'phi4:14b' is the low-n/low-confidence case; 'gemma2:27b' exists only to prove
// the categorical ceiling / "Other" fold and single-model selection.

const MINT_MODEL_IDS = ['qwen3-coder:30b', 'llama3.1:70b', 'mixtral:8x22b', 'phi4:14b', 'gemma2:27b'] as const;

/** Exposed for the MINT filter row's model multi-select. NOTE (deviation, see PR description):
 *  CONST-21/22's `/api/terminus/models` list endpoint is the eventual source for this, but it
 *  isn't built yet — this item hardcodes the mock fixture's own model set so the filter has
 *  something to select from; swap for a `models.browse`-sourced list once CONST-22 lands. */
export const MINT_MODEL_CATALOG: readonly string[] = MINT_MODEL_IDS;

const MINT_DIMENSIONS = [
  'reasoning', 'code_quality', 'instruction_following', 'tool_use',
  'context_retention', 'safety', 'latency_efficiency', 'creativity',
] as const;

function buildMintDimensionScore(dimension: string, base: number, n: number, lowConf = false): MintDimensionScore {
  return {
    dimension,
    norm: Math.max(0, Math.min(1, base)),
    raw: Math.round(base * 5 * 100) / 100,
    metric: 'mean_judge_score',
    std_dev: Math.round((0.05 + (1 - base) * 0.15) * 100) / 100,
    n,
    low_confidence: lowConf || n <= 1,
  };
}

const MOCK_MINT_SUMMARY: MintSummary = {
  models_profiled: MINT_MODEL_IDS.length,
  runs_this_epoch: 1842,
  fleet_best: { model: 'qwen3-coder:30b', pass_hat_3: 0.91 },
  gpu_hours: 214.6,
  epoch: 'S119',
};

const MOCK_MINT_SUMMARY_SPARSE: MintSummary = {
  models_profiled: 1,
  runs_this_epoch: 12,
  fleet_best: { model: 'phi4:14b', pass_hat_3: 0.42 },
  gpu_hours: 1.1,
  epoch: 'S110',
};

const MOCK_MINT_DIMENSIONS: MintDimensionsResponse = {
  dimensions: [...MINT_DIMENSIONS],
  models: [
    {
      model_id: 'qwen3-coder:30b',
      scores: MINT_DIMENSIONS.map((d, i) => buildMintDimensionScore(d, 0.7 + i * 0.02, 40)),
    },
    {
      // sparse: only 5 of 8 dimensions profiled -> the other 3 render at 0, hollow vertex + caveat
      model_id: 'llama3.1:70b',
      scores: MINT_DIMENSIONS.slice(0, 5).map((d, i) => buildMintDimensionScore(d, 0.55 + i * 0.03, 22)),
    },
    {
      model_id: 'mixtral:8x22b',
      scores: MINT_DIMENSIONS.map((d, i) => buildMintDimensionScore(d, 0.6 + (i % 3) * 0.05, 18)),
    },
    {
      // low-n / low-confidence case
      model_id: 'phi4:14b',
      scores: MINT_DIMENSIONS.map(d => buildMintDimensionScore(d, 0.4, 1, true)),
    },
  ],
  fleet_median: MINT_DIMENSIONS.map((d, i) => buildMintDimensionScore(d, 0.62 + i * 0.01, 120)),
};

const MOCK_MINT_DIMENSIONS_SOLO: MintDimensionsResponse = {
  dimensions: [...MINT_DIMENSIONS],
  models: [
    {
      model_id: 'phi4:14b',
      scores: MINT_DIMENSIONS.map(d => buildMintDimensionScore(d, 0.4, 1, true)),
    },
  ],
  fleet_median: MINT_DIMENSIONS.map((d, i) => buildMintDimensionScore(d, 0.62 + i * 0.01, 120)),
};

const MINT_TEST_TYPES = ['unit', 'integration', 'e2e'] as const;
const MINT_TASK_CATEGORIES = ['code', 'assistant', 'agent'] as const;

function mintMatrixColumns(): MintMatrixColumn[] {
  const cols: MintMatrixColumn[] = [];
  for (const tt of MINT_TEST_TYPES) {
    for (const tc of MINT_TASK_CATEGORIES) {
      cols.push({ key: `${tt}:${tc}`, test_type: tt, task_category: tc });
    }
  }
  return cols;
}

function buildMintMatrix(): MintMatrixResponse {
  const columns = mintMatrixColumns();
  const cells: MintMatrixCell[] = [];
  for (const model of MINT_MODEL_IDS) {
    for (const col of columns) {
      // Truthful edge case (contracts-to-confirm #3): 'agent' task_category cells are all
      // not_run — INTAKE_CORPUS_V2_DIR isn't provisioned yet, this is not a data gap/bug.
      if (col.task_category === 'agent') {
        cells.push({
          model, col: col.key, status: 'not_run', pass_rate: null, n_samples: 0,
          score_stddev: null, low_confidence: false, last_run_at: null, harness_version: null,
        });
        continue;
      }
      if (model === 'gemma2:27b') {
        // not yet profiled at all beyond one column -> mostly not_run
        cells.push({
          model, col: col.key, status: 'not_run', pass_rate: null, n_samples: 0,
          score_stddev: null, low_confidence: false, last_run_at: null, harness_version: null,
        });
        continue;
      }
      if (model === 'mixtral:8x22b' && col.test_type === 'e2e') {
        cells.push({
          model, col: col.key, status: 'stale', pass_rate: 0.58, n_samples: 6,
          score_stddev: 0.11, low_confidence: false,
          last_run_at: '2026-06-02T10:00:00Z', harness_version: 'v3.4',
        });
        continue;
      }
      if (model === 'phi4:14b' && col.test_type === 'integration') {
        cells.push({
          model, col: col.key, status: 'non_viable', pass_rate: null, n_samples: 0,
          score_stddev: null, low_confidence: false, last_run_at: null, harness_version: 'v3.4',
        });
        continue;
      }
      const base = model === 'qwen3-coder:30b' ? 0.86 : model === 'llama3.1:70b' ? 0.71 : 0.5;
      const n = model === 'phi4:14b' ? 1 : 24;
      cells.push({
        model, col: col.key, status: 'ok',
        pass_rate: Math.round((base + Math.random() * 0.06 - 0.03) * 100) / 100,
        n_samples: n, score_stddev: 0.08, low_confidence: n <= 1,
        last_run_at: '2026-07-15T08:00:00Z', harness_version: 'v3.5',
      });
    }
  }
  return { models: [...MINT_MODEL_IDS], columns, cells, corpus_dir_unset: true };
}

const MOCK_MINT_MATRIX = buildMintMatrix();

const MOCK_MINT_MATRIX_NOT_RUN_ONLY: MintMatrixResponse = {
  models: ['phi4:14b'],
  columns: mintMatrixColumns(),
  cells: mintMatrixColumns().map(col => ({
    model: 'phi4:14b', col: col.key, status: 'not_run' as const, pass_rate: null, n_samples: 0,
    score_stddev: null, low_confidence: false, last_run_at: null, harness_version: null,
  })),
  corpus_dir_unset: true,
};

function buildContextTiers(topThroughput: number, maxSafe: number): MintContextTierPoint[] {
  const tiers = [2048, 4096, 8192, 16384, 32768, 65536];
  return tiers.map(ctx => {
    const oom = ctx > maxSafe;
    const decay = Math.max(0.15, 1 - ctx / (maxSafe * 1.6));
    return {
      context_tokens: ctx,
      throughput: oom ? null : Math.round(topThroughput * decay),
      recall_score: oom ? null : Math.round((0.5 + decay * 0.45) * 100) / 100,
      ttft_ms: oom ? null : Math.round(120 + ctx / 40),
      memory_usage_mb: oom ? null : Math.round(4000 + ctx * 0.6),
      oom,
    };
  });
}

const MOCK_MINT_CONTEXT: MintContextProfilesResponse = {
  profiles: [
    { model: 'qwen3-coder:30b', tiers: buildContextTiers(62, 32768), max_context_safe: 32768 },
    { model: 'llama3.1:70b', tiers: buildContextTiers(38, 16384), max_context_safe: 16384 },
    { model: 'mixtral:8x22b', tiers: buildContextTiers(45, 8192), max_context_safe: 8192 },
    { model: 'phi4:14b', tiers: buildContextTiers(70, 4096), max_context_safe: 4096 },
  ],
};

const MOCK_MINT_CONTEXT_SOLO: MintContextProfilesResponse = {
  profiles: [
    { model: 'phi4:14b', tiers: buildContextTiers(70, 4096), max_context_safe: 4096 },
  ],
};

function buildMintActivityDays(n: number): MintActivityDay[] {
  const days: MintActivityDay[] = [];
  const now = Date.now();
  for (let i = n - 1; i >= 0; i--) {
    const d = new Date(now - i * 86400000);
    days.push({
      date: d.toISOString().slice(0, 10),
      code: Math.max(0, Math.round(20 + 12 * Math.sin(i / 4))),
      context: Math.max(0, Math.round(8 + 5 * Math.sin(i / 6 + 1))),
      agent: Math.max(0, Math.round(3 + 2 * Math.sin(i / 3 + 2))),
    });
  }
  return days;
}

const MOCK_MINT_ACTIVITY: MintActivityResponse = {
  days: buildMintActivityDays(90),
  epochs: [
    { epoch: 'S117', date: buildMintActivityDays(90)[10].date, label: 'S117 compiler cutover' },
    { epoch: 'S118', date: buildMintActivityDays(90)[45].date, label: 'S118 MUSE live-fire' },
    { epoch: 'S119', date: buildMintActivityDays(90)[80].date, label: 'S119 current' },
  ],
};

const MOCK_MINT_ACTIVITY_SPARSE: MintActivityResponse = {
  days: buildMintActivityDays(30).map(d => ({ ...d, code: 0, context: 0, agent: d.agent > 3 ? 1 : 0 })),
  epochs: [{ epoch: 'S119', date: buildMintActivityDays(30)[25].date, label: 'S119 current' }],
};

function buildMintPareto(): MintParetoResponse {
  const raw: [string, number, number, number][] = [
    ['qwen3-coder:30b', 820, 4.4, 24],
    ['llama3.1:70b', 1450, 3.9, 40],
    ['mixtral:8x22b', 1100, 4.0, 34],
    ['phi4:14b', 410, 3.2, 8],
    ['gemma2:27b', 690, 3.6, 16],
  ];
  return {
    points: raw.map(([model, lat, score, vram]) => ({
      model,
      mean_latency_ms: lat,
      mean_score: score,
      vram_gb: vram,
      p95_latency_ms: Math.round(lat * 1.4),
      score_stddev: 0.2,
      quality_per_gpu_second: Math.round((score / (lat / 1000) / vram) * 1000) / 1000,
    })),
  };
}

const MOCK_MINT_PARETO = buildMintPareto();

const MOCK_MINT_PARETO_SOLO: MintParetoResponse = {
  points: [buildMintPareto().points.find(p => p.model === 'phi4:14b')!],
};

// ── Mock data for the Muse module (CONST-19 backend; CONST-20 builds its UI
// against these shapes -- verified routes per CONST-GUI-audit.md §4/spec §5.4) ─

const MOCK_MUSE_ON_DECK = {
  items: [
    { id: 'md-1', title: 'Example Feature Film', kind: 'movie', progress_pct: 40, poster_path: '/art/poster/md-1' },
    { id: 'md-2', title: 'Example Series S1E4', kind: 'episode', progress_pct: 80, poster_path: '/art/poster/md-2' },
  ],
};

const MOCK_MUSE_PREMIERE = {
  items: [
    { id: 'md-3', title: 'Example Upcoming Release', release_date: new Date().toISOString(), rsvp_count: 0 },
  ],
};

const MOCK_MUSE_GAPS = { gaps: [], total: 0 };

const MOCK_MUSE_CHANNELS = {
  channels: [
    { id: 'ch-1', name: 'Mock Channel One', item_count: 12 },
    { id: 'ch-2', name: 'Mock Channel Two', item_count: 5 },
  ],
};

const MOCK_MUSE_TASTE_CLUSTERS = {
  clusters: [
    { cluster_id: 0, label: 'mock-cluster-a', points: [{ x: 0.1, y: 0.2, model: 'md-1' }] },
    { cluster_id: 1, label: 'mock-cluster-b', points: [{ x: 0.6, y: 0.4, model: 'md-2' }] },
  ],
};

/** GET-style mock lookups, keyed by "{system} {pathname}" (pathname without query string). */
const MOCK_GET: Record<string, unknown> = {
  'harmony /status': MOCK_STATUS,
  'harmony /agents/activity': MOCK_AGENTS,
  'harmony /analytics/completion-rate': [],
  'harmony /analytics/provider-comparison': [],
  'harmony /analytics/cost-tracking': [],
  'harmony /analytics/build-duration': [],
  'harmony /analytics/quality-scores': [],
  'harmony /analytics/escalation': MOCK_ESCALATION,
  'harmony /state/analytics': {},
  'harmony /sessions': { sessions: [] },
  'harmony /prompts': { versions: [] },
  'harmony /mode': MOCK_MODE,
  'chord /health': MOCK_CHORD_HEALTH,
  'chord /models': [],
  'chord /models/aliases': {},
  'chord /storage': [],
  'chord /analytics/savings': null,
  'chord /analytics/cost': [],
  'chord /providers': [],
  'chord /providers/profiles': MOCK_PROFILES,
  'muse /on_deck': MOCK_MUSE_ON_DECK,
  'muse /premiere': MOCK_MUSE_PREMIERE,
  'muse /gaps': MOCK_MUSE_GAPS,
  'muse /api/channels': MOCK_MUSE_CHANNELS,
  'muse /api/graph/taste-clusters': MOCK_MUSE_TASTE_CLUSTERS,
  'muse /api/graph/watch-history': { series: [] },
  'muse /api/graph/group-dynamics': { rows: [] },
  'muse /guide': { entries: [] },
};

/** MINT mock routing (CONST-23) — query-aware, unlike the plain pathname table above, since
 *  every mint endpoint's mock *variant* (full/sparse/not_run-only/single-model) is selected by
 *  the caller's filter params (§8; item brief "mock variants" requirement). Not merged into
 *  MOCK_GET because that table is pathname-only. */
function mockMintGetFor(pathname: string, query: URLSearchParams): unknown {
  const models = query.getAll('models').flatMap(v => v.split(',')).filter(Boolean);
  const epoch = query.get('epoch') ?? 'current';
  const solo = models.length === 1;

  if (pathname === '/mint/summary') {
    return epoch === 'S110' ? MOCK_MINT_SUMMARY_SPARSE : MOCK_MINT_SUMMARY;
  }
  if (pathname === '/mint/dimensions') {
    if (solo) return MOCK_MINT_DIMENSIONS_SOLO;
    return MOCK_MINT_DIMENSIONS;
  }
  if (pathname === '/mint/matrix') {
    if (solo && models[0] === 'phi4:14b') return MOCK_MINT_MATRIX_NOT_RUN_ONLY;
    return MOCK_MINT_MATRIX;
  }
  if (pathname === '/mint/context-profiles') {
    if (solo) return MOCK_MINT_CONTEXT_SOLO;
    return MOCK_MINT_CONTEXT;
  }
  if (pathname === '/mint/activity') {
    return epoch === 'S110' ? MOCK_MINT_ACTIVITY_SPARSE : MOCK_MINT_ACTIVITY;
  }
  if (pathname === '/mint/pareto') {
    // NOTE (deviation, see PR description): §8's endpoint table doesn't enumerate a dedicated
    // pareto endpoint; C4 needs per-model {latency, score, vram} that fits nowhere else, so this
    // mock adds `GET /api/terminus/mint/pareto?models=` as an additive contract-to-confirm for
    // CONST-21 rather than silently overloading `/mint/runs` or `/models`.
    if (solo) return MOCK_MINT_PARETO_SOLO;
    return MOCK_MINT_PARETO;
  }
  return undefined;
}

function mockGetFor(system: SystemId, pathname: string, query: URLSearchParams): unknown {
  const key = `${system} ${pathname}`;
  if (key in MOCK_GET) return MOCK_GET[key];
  if (system === 'harmony' && pathname.startsWith('/tree/')) {
    return { ...MOCK_TREE, project: decodeURIComponent(pathname.slice('/tree/'.length)) };
  }
  if (system === 'muse' && pathname.startsWith('/api/channels/') && pathname.endsWith('/lineup')) {
    return { channel_id: pathname.split('/')[3], lineup: [] };
  }
  if (system === 'terminus' && pathname.startsWith('/mint/')) {
    const mint = mockMintGetFor(pathname, query);
    if (mint !== undefined) return mint;
  }
  return null;
}

/** POST/PUT-style mock acks — every write in the mock world just succeeds with a canned shape. */
function mockWriteFor(system: SystemId, pathname: string): unknown {
  if (system === 'harmony' && pathname === '/engine/stop') {
    return { state: 'stopped', pid: null, active_count: 0, uptime_secs: 0, stop_reason: 'mock', executor_active: false };
  }
  if (system === 'harmony' && pathname === '/engine/restart') {
    return { state: 'executing', pid: null, active_count: 0, uptime_secs: 0, stop_reason: null, executor_active: true };
  }
  if (system === 'harmony' && pathname === '/mode') {
    return MOCK_MODE;
  }
  if (system === 'harmony' && pathname === '/command') {
    return { ok: true, command: '' };
  }
  if (system === 'harmony' && pathname === '/commands/inference-mix') {
    return { ok: true, inference_mix: 50 };
  }
  if (system === 'harmony' && pathname === '/commands/compression-level') {
    return { ok: true };
  }
  if (system === 'chord' && pathname === '/playground/run') {
    return {
      response: '(mock adapter — no live model backend) This is a canned playground response.',
      tokens_in: 12, tokens_out: 18, latency_ms: 120, cost: 0, model: 'mock',
    };
  }
  return { ok: true };
}

function mockRequest<T>(system: SystemId, path: string, init?: RequestInit): Promise<T> {
  const method = (init?.method ?? 'GET').toUpperCase();
  const [pathname, search] = path.split('?');
  const query = new URLSearchParams(search ?? '');
  const value = method === 'GET'
    ? mockGetFor(system, pathname, query)
    : mockWriteFor(system, pathname);
  return delay(value as T);
}

/** Mock WS: reports "connected" immediately, never emits events (mock has no live daemon). */
function mockWsConnect(handlers: WsHandlers): WsConnection {
  const id = setTimeout(() => handlers.onOpen?.(), 50);
  return {
    send() { /* no-op in mock mode */ },
    close() { clearTimeout(id); handlers.onClose?.(); },
  };
}

const mockAdapter: AggregationClient = {
  auth: {
    async me() {
      // Mock mode is always an operator session — no real login flow to distinguish tiers
      // (CONST-27's viewer tier is exercised via the http adapter against a real backend).
      return delay({ authenticated: true, username: 'mock-user', role: 'operator' });
    },
    async login(username: string) {
      return delay({ authenticated: true, username, role: 'operator' });
    },
    async logout() {
      await delay(undefined, 40);
    },
  },
  health: {
    async list() {
      return delay(MOCK_HEALTH);
    },
  },
  terminus: {
    async configSummary() {
      return delay(MOCK_TERMINUS_CONFIG);
    },
    async activity(limit?: number) {
      const entries = limit != null ? MOCK_ACTIVITY.entries.slice(-limit) : MOCK_ACTIVITY.entries;
      return delay({ entries });
    },
  },
  async request<T>(system: SystemId, path: string, init?: RequestInit): Promise<T> {
    return withMutationResultEvent(system, path, init, () => mockRequest<T>(system, path, init));
  },
  ws: {
    connect: mockWsConnect,
  },
  prefs: prefsClient,
};

// ── HTTP adapter ─────────────────────────────────────────────────────────────
// Real same-origin fetch. Endpoints this expects CONST-02 to serve:
//   GET  /api/auth/me            -> AuthMeResponse
//   POST /api/auth/login         -> AuthMeResponse   (body: { username, password })
//   POST /api/auth/logout        -> 204/200
//   GET  /api/health             -> HealthStatus[]
//   GET  /api/terminus/config    -> TerminusConfigSummary
//   GET  /api/terminus/activity?limit=N -> ActivityFeedResponse (CONST-26; never body content)
//   *    /api/{system}/{path}    -> generic passthrough for `request<T>()`
//   WS   /ws                     -> same-origin, session-cookie-authenticated event stream
//                                    (engine/ralph-loop/log/tree_update events); see ws.connect()
//
// CONST-04: full harmony-web port. Endpoints the generic request<T>() passthrough now needs to
// serve (see MOCK_GET/mockWriteFor below for the exact mock shapes — that's the contract):
//   harmony: GET /status, GET /agents/activity,
//            GET /analytics/{completion-rate,provider-comparison,cost-tracking,build-duration,
//                             quality-scores,escalation}, GET /state/analytics, GET /sessions,
//            GET /prompts, GET /mode, PUT /mode, GET /tree/{project},
//            POST /engine/stop, POST /engine/restart, POST /command,
//            POST /commands/inference-mix, POST /commands/compression-level
//   chord:   GET /health, GET /models, GET /models/aliases, GET /storage,
//            GET /analytics/savings, GET /analytics/cost, GET /providers, GET /providers/profiles,
//            POST /playground/run
//   muse (CONST-19; CONST-20 builds its panels against these): GET /on_deck, GET /premiere,
//            GET /gaps, GET /api/channels, GET /api/channels/{id}/lineup, GET /guide,
//            GET /api/graph/{taste-clusters,watch-history,group-dynamics}, GET /art/{kind}/{id}
//            (binary passthrough -- see crate::constellation::proxy's module doc; this generic
//            request<T>() path is JSON-typed, art responses should be fetched by <img src> URL,
//            not through this method)
//   terminus/mint (CONST-21 backend not yet merged; CONST-23 builds its UI against these mock
//            shapes -- §8 of CONST-GUI-SPEC.md): GET /mint/summary?epoch=,
//            GET /mint/dimensions?models=&epoch=, GET /mint/matrix?epoch=,
//            GET /mint/context-profiles?models=, GET /mint/activity?range=,
//            GET /mint/pareto?models=&epoch= (additive — not in §8's table, see the mock
//            routing comment in mockMintGetFor for why C4 needed it)

function baseUrl(): string {
  // Same-origin only — never a hardcoded host/port. This is the one place in the app
  // permitted to read window.location.
  return window.location.origin;
}

// The single-auth invariant, enforced structurally: Content-Type is always JSON and
// authoritative; no caller-supplied auth-bearing header is ever forwarded to the backend.
function enforceHeaders(callerHeaders?: HeadersInit): Record<string, string> {
  const out: Record<string, string> = {};
  if (callerHeaders) {
    const entries = callerHeaders instanceof Headers
      ? Array.from(callerHeaders.entries())
      : Array.isArray(callerHeaders)
        ? callerHeaders
        : Object.entries(callerHeaders);
    for (const [k, v] of entries) {
      const lk = k.toLowerCase();
      if (lk === 'authorization' || lk === 'cookie' || lk === 'content-type') continue;
      out[k] = v as string;
    }
  }
  out['Content-Type'] = 'application/json';
  return out;
}

async function httpJson<T>(path: string, init?: RequestInit): Promise<T> {
  // Enforce the aggregation-client invariants so a caller can NEVER override them:
  //  - credentials:'include' — the session cookie is the only auth the browser holds.
  //  - Content-Type:'application/json' is authoritative (merged LAST, after caller headers).
  //  - auth-bearing headers are stripped: the browser holds no backend credentials, so an
  //    Authorization/Cookie header from a caller is meaningless and must never be sent.
  const res = await fetch(`${baseUrl()}${path}`, {
    ...init,
    credentials: 'include',
    headers: enforceHeaders(init?.headers),
  });
  if (!res.ok) {
    throw new Error(`HTTP ${res.status} for ${path}`);
  }
  if (res.status === 204) return undefined as unknown as T;
  return (await res.json()) as T;
}

const httpAdapter: AggregationClient = {
  auth: {
    async me() {
      try {
        return await httpJson<AuthMeResponse>('/api/auth/me');
      } catch {
        return { authenticated: false, username: null, role: null };
      }
    },
    async login(username: string, password: string) {
      return httpJson<AuthMeResponse>('/api/auth/login', {
        method: 'POST',
        body: JSON.stringify({ username, password }),
      });
    },
    async logout() {
      await httpJson<void>('/api/auth/logout', { method: 'POST' }).catch(() => {});
    },
  },
  health: {
    async list() {
      return httpJson<HealthStatus[]>('/api/health');
    },
  },
  terminus: {
    async configSummary() {
      return httpJson<TerminusConfigSummary>('/api/terminus/config');
    },
    async activity(limit?: number) {
      const query = limit != null ? `?limit=${encodeURIComponent(String(limit))}` : '';
      return httpJson<ActivityFeedResponse>(`/api/terminus/activity${query}`);
    },
  },
  async request<T>(system: SystemId, path: string, init?: RequestInit): Promise<T> {
    const normalized = path.startsWith('/') ? path : `/${path}`;
    return withMutationResultEvent(system, path, init, () => httpJson<T>(`/api/${system}${normalized}`, init));
  },
  ws: {
    connect(handlers: WsHandlers): WsConnection {
      // Same-origin only, derived from window.location — this is the one other spot (besides
      // baseUrl() above) permitted to touch it, and only inside this adapter.
      const proto = window.location.protocol === 'https:' ? 'wss:' : 'ws:';
      let ws: WebSocket | null = null;
      let closedByCaller = false;
      let attempt = 0;
      let reconnectTimer: ReturnType<typeof setTimeout> | null = null;

      const open = () => {
        ws = new WebSocket(`${proto}//${window.location.host}/ws`);
        ws.onopen = () => { attempt = 0; handlers.onOpen?.(); };
        ws.onmessage = (e) => {
          try {
            handlers.onEvent(JSON.parse(e.data as string));
          } catch { /* ignore malformed */ }
        };
        ws.onclose = () => {
          handlers.onClose?.();
          if (closedByCaller) return;
          const delayMs = Math.min(1000 * 2 ** attempt, 30000);
          attempt += 1;
          reconnectTimer = setTimeout(open, delayMs);
        };
        ws.onerror = () => { ws?.close(); };
      };
      open();

      return {
        send(data: unknown) {
          if (ws?.readyState === WebSocket.OPEN) ws.send(JSON.stringify(data));
        },
        close() {
          closedByCaller = true;
          if (reconnectTimer) clearTimeout(reconnectTimer);
          ws?.close();
        },
      };
    },
  },
  prefs: prefsClient,
};

// ── Selection ─────────────────────────────────────────────────────────────

function resolveMode(): 'mock' | 'http' {
  const raw = (import.meta as unknown as { env?: Record<string, string | undefined> }).env
    ?.VITE_AGG_MODE;
  return raw === 'http' ? 'http' : 'mock';
}

let cached: AggregationClient | null = null;

/** The single aggregation client instance for the app. Mode chosen once, at first use. */
export function getAggregationClient(): AggregationClient {
  if (!cached) {
    cached = resolveMode() === 'http' ? httpAdapter : mockAdapter;
  }
  return cached;
}

// Exported for tests / explicit overrides only — app code should use getAggregationClient().
export { mockAdapter, httpAdapter };
