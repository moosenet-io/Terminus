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

// ── Mock data for the Model Library module (CONST-21 API; CONST-22 builds its UI against
// these shapes -- spec §6/§8, docs/constellation/CONST-GUI-SPEC.md). Types in
// src/types/models.ts mirror these EXACTLY so live binding is a no-op once CONST-21 merges. ──

import type {
  ModelListItem,
  ModelsListParams,
  ModelsListResponse,
  ModelDetailResponse,
  MintDimensionsResponse,
} from '../types/models';

const MINT_DIMENSIONS = ['code', 'reasoning', 'writing', 'math', 'tool_use', 'agentic', 'safety', 'knowledge'] as const;

/** Every mock model's full detail record, keyed by `model_name`. The list endpoint's rows are
 *  derived (projected) from this so the two endpoints never drift out of sync. */
const MOCK_MODEL_DETAILS: Record<string, ModelDetailResponse & { _list: Omit<ModelListItem, 'model_name'> }> = {
  'qwen3-coder:30b': {
    identity: {
      model_name: 'qwen3-coder:30b',
      family: 'Qwen3', params_b: 30,
      quants: [
        { quant: 'Q4_K_M', vram_gb: 18.2, quality_penalty: 0.04 },
        { quant: 'Q8_0', vram_gb: 32.1, quality_penalty: 0.01 },
      ],
      best_for: ['long-context coding', 'agentic tool-use', 'refactors'],
      avoid_for: ['creative writing'],
      notes: 'Primary production coder — pinned on <host> lemonade-coder.service.',
    },
    brochure: {
      hf_repo: 'https://huggingface.co/Qwen/Qwen3-Coder-30B',
      category: 'coder',
      status: 'adopted',
      timeline: [
        { status: 'discovered', at: '2026-05-02T00:00:00Z' },
        { status: 'evaluating', at: '2026-05-04T00:00:00Z' },
        { status: 'shortlisted', at: '2026-05-09T00:00:00Z' },
        { status: 'adopted', at: '2026-05-14T00:00:00Z', note: 'promoted to permanent <host> serve' },
      ],
      discovery_score: 0.93,
      rationale: 'Best coder/agentic balance at this VRAM budget in the sweep.',
    },
    serving: [
      { backend_tag: '<host>-lemonade', best_runtime: 'llama.cpp', tok_s: 42.5, vram_peak_gb: 19.8, cold_load_s: 11, keep_warm: true, exclusion_reason: 'none' },
    ],
    operational: { max_context_safe: 96_000, max_context_absolute: 128_000, degradation_point: 110_000, throughput_strip: [44, 43, 41, 38, 32, 24], tier: 'hot' },
    catalog: { card: { best_pass_rate: 0.91, last_run_at: '2026-07-18T09:00:00Z' }, cells: [] },
    _list: { family: 'Qwen3', params_b: 30, quant: 'Q4_K_M', category: 'coder', brochure_status: 'adopted', in_current_fleet: true, discovery_score: 0.93, vram_gb: 19.8, size_b: 30, serving_now: true, coverage: { coder: 'covered', assistant: 'partial', serving: 'covered', agent: 'covered' }, best_pass_rate: 0.91, last_run_at: '2026-07-18T09:00:00Z' },
  },
  'claude-sonnet-5': {
    identity: {
      model_name: 'claude-sonnet-5', family: 'Claude', params_b: undefined,
      quants: [],
      best_for: ['assistant', 'agentic orchestration', 'review'],
      avoid_for: [],
      notes: 'Cloud provider — no local VRAM footprint.',
    },
    brochure: null,
    serving: [
      { backend_tag: 'anthropic-api', best_runtime: 'api', tok_s: 60, vram_peak_gb: 0, cold_load_s: 0, keep_warm: true, exclusion_reason: 'none' },
    ],
    operational: { max_context_safe: 190_000, max_context_absolute: 200_000, degradation_point: undefined, throughput_strip: [60, 60, 59, 58, 57, 55], tier: 'hot' },
    catalog: { card: { best_pass_rate: 0.97, last_run_at: '2026-07-19T02:00:00Z' }, cells: [] },
    _list: { family: 'Claude', quant: null, category: 'assistant', in_current_fleet: true, vram_gb: 0, serving_now: true, coverage: { coder: 'covered', assistant: 'covered', serving: 'covered', agent: 'covered' }, best_pass_rate: 0.97, last_run_at: '2026-07-19T02:00:00Z' },
  },
  'llama3.1:8b': {
    identity: {
      model_name: 'llama3.1:8b', family: 'Llama 3.1', params_b: 8,
      quants: [{ quant: 'Q4_K_M', vram_gb: 5.4, quality_penalty: 0.06 }],
      best_for: ['fast local fallback'], avoid_for: ['long agentic loops'],
      notes: 'Cold-tier fallback, rarely served warm.',
    },
    brochure: {
      hf_repo: 'https://huggingface.co/meta-llama/Meta-Llama-3.1-8B',
      category: 'assistant', status: 'evaluated',
      timeline: [
        { status: 'discovered', at: '2026-03-01T00:00:00Z' },
        { status: 'evaluating', at: '2026-03-03T00:00:00Z' },
        { status: 'evaluated', at: '2026-03-10T00:00:00Z' },
      ],
      discovery_score: 0.61,
      rationale: 'Solid general baseline but outclassed by adopted models at this size.',
    },
    serving: [
      { backend_tag: '<host>-ollama-cpu', best_runtime: 'ollama', tok_s: 14.2, vram_peak_gb: 5.4, cold_load_s: 4, keep_warm: false, exclusion_reason: 'none' },
    ],
    operational: { max_context_safe: 28_000, max_context_absolute: 32_000, degradation_point: 30_000, throughput_strip: [15, 14, 13, 10, 6, 3], tier: 'cold' },
    catalog: { card: { best_pass_rate: 0.68, last_run_at: '2026-06-30T14:00:00Z' }, cells: [] },
    _list: { family: 'Llama 3.1', params_b: 8, quant: 'Q4_K_M', category: 'assistant', brochure_status: 'evaluated', in_current_fleet: true, discovery_score: 0.61, vram_gb: 5.4, size_b: 8, serving_now: false, coverage: { coder: 'partial', assistant: 'covered', serving: 'partial', agent: 'none' }, best_pass_rate: 0.68, last_run_at: '2026-06-30T14:00:00Z' },
  },
  'gemma2:9b-agent': {
    identity: {
      model_name: 'gemma2:9b-agent', family: 'Gemma 2', params_b: 9,
      quants: [{ quant: 'Q5_K_M', vram_gb: 6.8, quality_penalty: 0.05 }],
      best_for: ['tool-use', 'agentic loops'], avoid_for: ['very long context'],
      notes: 'Shortlisted candidate, not yet promoted.',
    },
    brochure: {
      hf_repo: 'https://huggingface.co/google/gemma-2-9b',
      category: 'agent', status: 'shortlisted',
      timeline: [
        { status: 'discovered', at: '2026-06-20T00:00:00Z' },
        { status: 'evaluating', at: '2026-06-22T00:00:00Z' },
        { status: 'shortlisted', at: '2026-06-28T00:00:00Z' },
      ],
      discovery_score: 0.78,
      rationale: 'Best agentic pass-rate under 10B in the current sweep; awaiting production trial.',
    },
    serving: null,
    operational: null,
    catalog: { card: { best_pass_rate: 0.74, last_run_at: '2026-07-10T00:00:00Z' }, cells: [] },
    _list: { family: 'Gemma 2', params_b: 9, quant: 'Q5_K_M', category: 'agent', brochure_status: 'shortlisted', in_current_fleet: false, discovery_score: 0.78, vram_gb: 6.8, size_b: 9, serving_now: false, coverage: { coder: 'none', assistant: 'partial', serving: 'none', agent: 'covered' }, best_pass_rate: 0.74, last_run_at: '2026-07-10T00:00:00Z' },
  },
  'mixtral:8x22b': {
    identity: {
      model_name: 'mixtral:8x22b', family: 'Mixtral', params_b: 141,
      quants: [{ quant: 'Q4_K_M', vram_gb: 74, quality_penalty: 0.05 }],
      best_for: ['reasoning', 'long-form writing'], avoid_for: ['low-VRAM hosts'],
      notes: 'Evicted from the fleet after S118 disk pressure -- catalog-but-evicted example.',
    },
    brochure: {
      hf_repo: 'https://huggingface.co/mistralai/Mixtral-8x22B-v0.1',
      category: 'reasoning', status: 'deprecated',
      timeline: [
        { status: 'discovered', at: '2026-01-10T00:00:00Z' },
        { status: 'adopted', at: '2026-01-20T00:00:00Z' },
        { status: 'deprecated', at: '2026-07-16T00:00:00Z', note: 'evicted for disk pressure (S118)' },
      ],
      discovery_score: 0.82,
      rationale: 'Strong reasoning score, but VRAM/disk cost no longer justified post-S118.',
    },
    serving: [
      { backend_tag: '<host>-ollama-gpu', best_runtime: 'ollama', tok_s: 22, vram_peak_gb: 74, cold_load_s: 38, keep_warm: false, exclusion_reason: 'evicted: disk pressure S118' },
    ],
    operational: { max_context_safe: 60_000, max_context_absolute: 64_000, degradation_point: 58_000, throughput_strip: [22, 21, 19, 15, 9, 4], tier: 'cold' },
    catalog: { card: { best_pass_rate: 0.85, last_run_at: '2026-07-14T00:00:00Z' }, cells: [] },
    _list: { family: 'Mixtral', params_b: 141, quant: 'Q4_K_M', category: 'reasoning', brochure_status: 'deprecated', in_current_fleet: true, discovery_score: 0.82, vram_gb: 74, size_b: 141, serving_now: false, coverage: { coder: 'partial', assistant: 'covered', serving: 'partial', agent: 'partial' }, best_pass_rate: 0.85, last_run_at: '2026-07-14T00:00:00Z' },
  },
  'hf-candidate/deepseek-v3-lite': {
    identity: {
      model_name: 'hf-candidate/deepseek-v3-lite', family: 'DeepSeek V3', params_b: 16,
      quants: [{ quant: 'Q4_K_M', vram_gb: 10.5, quality_penalty: 0.08 }],
      best_for: ['coding'], avoid_for: [],
      notes: 'Brochure-only -- never profiled against the fleet. Identity+provenance render; deployment/MINT degrade.',
    },
    brochure: {
      hf_repo: 'https://huggingface.co/deepseek-ai/DeepSeek-V3-Lite',
      category: 'coder', status: 'discovered',
      timeline: [{ status: 'discovered', at: '2026-07-17T00:00:00Z' }],
      discovery_score: 0.71,
      rationale: 'model_discovery_refresh candidate, not yet evaluated.',
    },
    serving: null,
    operational: null,
    catalog: null,
    _list: { family: 'DeepSeek V3', params_b: 16, category: 'coder', brochure_status: 'discovered', in_current_fleet: false, discovery_score: 0.71, size_b: 16, serving_now: false, coverage: { coder: 'none', assistant: 'none', serving: 'none', agent: 'none' } },
  },
  'hf-candidate/phi-4-vision': {
    identity: {
      model_name: 'hf-candidate/phi-4-vision', family: 'Phi-4', params_b: 4,
      quants: [{ quant: 'Q6_K', vram_gb: 3.2, quality_penalty: 0.03 }],
      best_for: ['vision QA'], avoid_for: ['agentic tool-use'],
      notes: 'Brochure-only candidate, small enough for the always-warm tier if adopted.',
    },
    brochure: {
      hf_repo: 'https://huggingface.co/microsoft/Phi-4-vision',
      category: 'vision', status: 'evaluating',
      timeline: [
        { status: 'discovered', at: '2026-07-05T00:00:00Z' },
        { status: 'evaluating', at: '2026-07-12T00:00:00Z' },
      ],
      discovery_score: 0.66,
      rationale: 'Under evaluation for Muse still-frame matching (S119b) vision backend.',
    },
    serving: null,
    operational: null,
    catalog: null,
    _list: { family: 'Phi-4', params_b: 4, category: 'vision', brochure_status: 'evaluating', in_current_fleet: false, discovery_score: 0.66, size_b: 4, serving_now: false, coverage: { coder: 'none', assistant: 'none', serving: 'none', agent: 'none' } },
  },
  'nomic-embed-text': {
    identity: {
      model_name: 'nomic-embed-text', family: 'Nomic', params_b: 0.14,
      quants: [{ quant: 'F16', vram_gb: 0.4, quality_penalty: 0 }],
      best_for: ['embeddings', 'KG grounding'], avoid_for: ['generation'],
      notes: 'Always-warm embedding backend for Atlas KG.',
    },
    brochure: {
      hf_repo: 'https://huggingface.co/nomic-ai/nomic-embed-text-v1.5',
      category: 'embedding', status: 'adopted',
      timeline: [
        { status: 'discovered', at: '2026-02-01T00:00:00Z' },
        { status: 'adopted', at: '2026-02-05T00:00:00Z' },
      ],
      discovery_score: 0.88,
      rationale: 'Standard embedding backend, adopted fleet-wide.',
    },
    serving: [
      { backend_tag: '<host>-ollama-gpu', best_runtime: 'ollama', tok_s: 900, vram_peak_gb: 0.4, cold_load_s: 1, keep_warm: true, exclusion_reason: 'none' },
    ],
    operational: { max_context_safe: 8000, max_context_absolute: 8192, degradation_point: undefined, throughput_strip: [900, 900, 895, 890, 880, 860], tier: 'hot' },
    catalog: { card: { best_pass_rate: 0.99, last_run_at: '2026-07-19T04:00:00Z' }, cells: [] },
    _list: { family: 'Nomic', params_b: 0.14, quant: 'F16', category: 'embedding', brochure_status: 'adopted', in_current_fleet: true, discovery_score: 0.88, vram_gb: 0.4, size_b: 0.14, serving_now: true, coverage: { coder: 'none', assistant: 'none', serving: 'covered', agent: 'none' }, best_pass_rate: 0.99, last_run_at: '2026-07-19T04:00:00Z' },
  },
  'hf-candidate/tiny-storyteller-1b': {
    identity: {
      model_name: 'hf-candidate/tiny-storyteller-1b', family: 'TinyStoryteller', params_b: 1,
      quants: [{ quant: 'Q4_0', vram_gb: 0.8, quality_penalty: 0.12 }],
      best_for: ['creative writing'], avoid_for: ['coding', 'reasoning'],
      notes: 'Rejected -- quality penalty too high for any adopted use-case.',
    },
    brochure: {
      hf_repo: 'https://huggingface.co/example-org/tiny-storyteller-1b',
      category: 'creative', status: 'rejected',
      timeline: [
        { status: 'discovered', at: '2026-04-01T00:00:00Z' },
        { status: 'evaluating', at: '2026-04-03T00:00:00Z' },
        { status: 'rejected', at: '2026-04-06T00:00:00Z', note: 'quality penalty too high' },
      ],
      discovery_score: 0.22,
      rationale: 'Below the adoption threshold on every evaluated dimension.',
    },
    serving: null,
    operational: null,
    catalog: null,
    _list: { family: 'TinyStoryteller', params_b: 1, category: 'creative', brochure_status: 'rejected', in_current_fleet: false, discovery_score: 0.22, size_b: 1, serving_now: false, coverage: { coder: 'none', assistant: 'none', serving: 'none', agent: 'none' } },
  },
};

/** MINT per-model dimension scores, keyed by `model_id` — only fleet-profiled models get one
 *  (brochure-only candidates have no MINT profile: the MINT-profile section degrades to
 *  ChartEmpty for them, per §6.1's "each degrading independently" rule). `n<=1`/`low_confidence`
 *  rows below deliberately include a case so the ⚠ affordance is exercised on mocks. */
const MOCK_MINT_SCORES: Record<string, number[]> = {
  'qwen3-coder:30b': [0.91, 0.78, 0.62, 0.7, 0.88, 0.85, 0.8, 0.74],
  'claude-sonnet-5': [0.95, 0.93, 0.9, 0.89, 0.94, 0.92, 0.91, 0.9],
  'llama3.1:8b': [0.6, 0.55, 0.58, 0.5, 0.4, 0.35, 0.62, 0.57],
  'gemma2:9b-agent': [0.65, 0.6, 0.5, 0.45, 0.79, 0.74, 0.6, 0.55],
  'mixtral:8x22b': [0.72, 0.86, 0.8, 0.83, 0.55, 0.5, 0.7, 0.75],
};
const MOCK_MINT_FLEET_MEDIAN = [0.74, 0.72, 0.68, 0.67, 0.7, 0.68, 0.71, 0.69];
/** Models whose MINT scores are backed by n<=1 samples -- always render the ⚠ affordance. */
const MOCK_MINT_LOW_CONFIDENCE: Record<string, Set<string>> = {
  'gemma2:9b-agent': new Set(['agentic', 'safety']),
};

function sizeBucketOf(sizeB: number | undefined): import('../types/models').SizeBucket | undefined {
  if (sizeB == null) return undefined;
  if (sizeB < 4) return '<4B';
  if (sizeB < 10) return '4-10B';
  if (sizeB < 35) return '10-35B';
  return '>35B';
}

function mockModelsList(params: URLSearchParams): ModelsListResponse {
  const scope = (params.get('scope') ?? 'all') as ModelsListParams['scope'];
  const q = (params.get('q') ?? '').toLowerCase();
  const category = params.get('category');
  const brochureStatus = params.get('brochure_status') ?? params.get('status');
  const sizeBucket = params.get('size_bucket');
  const coverage = params.get('coverage');
  const servingOnly = params.get('serving') === 'true';
  const limit = Math.min(500, Math.max(1, Number(params.get('limit') ?? 50) || 50));
  const offset = Math.max(0, Number(params.get('offset') ?? 0) || 0);

  let rows: ModelListItem[] = Object.entries(MOCK_MODEL_DETAILS).map(([model_name, d]) => ({
    model_name,
    ...d._list,
  }));

  if (scope === 'fleet') rows = rows.filter(r => r.in_current_fleet);
  else if (scope === 'brochure') rows = rows.filter(r => r.brochure_status != null);

  if (q) rows = rows.filter(r => r.model_name.toLowerCase().includes(q) || r.family?.toLowerCase().includes(q));
  if (category) rows = rows.filter(r => r.category === category);
  if (brochureStatus) rows = rows.filter(r => r.brochure_status === brochureStatus);
  if (sizeBucket) rows = rows.filter(r => sizeBucketOf(r.size_b) === sizeBucket);
  if (coverage) rows = rows.filter(r => r.coverage[coverage as keyof typeof r.coverage] === 'covered');
  if (servingOnly) rows = rows.filter(r => r.serving_now);

  const total = rows.length;
  const page = rows.slice(offset, offset + limit);
  return { total, refreshed_at: '2026-07-19T06:00:00Z', models: page };
}

function mockModelDetail(name: string): ModelDetailResponse | null {
  const rec = MOCK_MODEL_DETAILS[name];
  if (!rec) return null;
  const { _list: _unused, ...detail } = rec;
  return detail;
}

function mockMintDimensions(modelsParam: string | null): MintDimensionsResponse {
  const ids = (modelsParam ?? '').split(',').map(s => s.trim()).filter(Boolean);
  const wanted = ids.length > 0 ? ids : Object.keys(MOCK_MINT_SCORES);
  return {
    dimensions: [...MINT_DIMENSIONS],
    models: wanted
      .filter(id => MOCK_MINT_SCORES[id])
      .map(id => ({
        model_id: id,
        scores: MINT_DIMENSIONS.map((dimension, i) => ({
          dimension,
          norm: MOCK_MINT_SCORES[id][i],
          raw: MOCK_MINT_SCORES[id][i] * 100,
          metric: 'pass_rate',
          std_dev: 0.05,
          n: MOCK_MINT_LOW_CONFIDENCE[id]?.has(dimension) ? 1 : 24,
          low_confidence: MOCK_MINT_LOW_CONFIDENCE[id]?.has(dimension) ?? false,
        })),
      })),
    fleet_median: MOCK_MINT_FLEET_MEDIAN,
  };
}

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

function mockGetFor(system: SystemId, pathname: string, search: string): unknown {
  const key = `${system} ${pathname}`;
  if (key in MOCK_GET) return MOCK_GET[key];
  if (system === 'harmony' && pathname.startsWith('/tree/')) {
    return { ...MOCK_TREE, project: decodeURIComponent(pathname.slice('/tree/'.length)) };
  }
  if (system === 'muse' && pathname.startsWith('/api/channels/') && pathname.endsWith('/lineup')) {
    return { channel_id: pathname.split('/')[3], lineup: [] };
  }
  // CONST-22: Model Library reads (§8) — query-bearing, so handled here rather than the
  // static MOCK_GET table above (which only keys on pathname).
  if (system === 'terminus' && pathname === '/models') {
    return mockModelsList(new URLSearchParams(search));
  }
  if (system === 'terminus' && pathname.startsWith('/models/')) {
    const name = decodeURIComponent(pathname.slice('/models/'.length));
    return mockModelDetail(name); // null -> 404 territory; httpAdapter throws on !res.ok, mock just returns null
  }
  if (system === 'terminus' && pathname === '/mint/dimensions') {
    return mockMintDimensions(new URLSearchParams(search).get('models'));
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
  const qIdx = path.indexOf('?');
  const pathname = qIdx === -1 ? path : path.slice(0, qIdx);
  const search = qIdx === -1 ? '' : path.slice(qIdx + 1);
  const value = method === 'GET'
    ? mockGetFor(system, pathname, search)
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
//   terminus (CONST-21; CONST-22 builds the Model Library UI against these -- see
//            src/types/models.ts for the exact shapes, spec §8):
//            GET /models?scope=&q=&category=&brochure_status=&size_bucket=&coverage=&serving=&limit=&offset=
//            GET /models/{name} (name is the URL-encoded full registry key)
//            GET /mint/dimensions?models=&epoch= (comma-separated model ids)

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
