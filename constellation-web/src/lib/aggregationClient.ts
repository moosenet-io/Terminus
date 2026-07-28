// CONST-04: The aggregation client is the ONLY module in this app allowed to talk to the
// backend. Every other module (hooks, panels, components) goes through the exported
// `client` singleton below — never call `fetch` or read `window.location` directly
// elsewhere in the app (that's an acceptance-criterion grep check, keep it true).
//
// Two implementations of the same typed interface:
//   - httpAdapter: real same-origin fetch against `/api/{system}/...`, cookie-based session auth.
//   - mockAdapter: canned in-memory data, no network — for explicit offline/dev use only.
//
// S127 TGUI2 (Part B) — adapter selection, http-default + runtime-selectable (see resolveMode
// at the bottom of this file). The old build-time-only switch defaulted to `mock`, so a
// production build that forgot `VITE_AGG_MODE=http` shipped the ENTIRE app as fixtures (the
// "4-5 items / smoke-and-mirrors" defect). The default is now INVERTED: any build served to a
// browser talks to the real backend (http); `mock` must be explicitly opted into. So an
// unconfigured build degrades to real-backend-with-empty-states, never silent fake data.
// This is deliberately the *only* seam CONST-02 (the real Terminus-side aggregation layer)
// needs to fill in — the httpAdapter below defines exactly the endpoints/shapes it must serve.

// CGUI-08 (TERM #531): the Models/MINT data-client method group consumes the CONST-21 +
// CGUI-07 read API (`crate::constellation::models_api`). Response shapes live in
// `../types/mint` (typed 1:1 against each handler's `json!({…})`).
import type {
  MintCategory,
  MintCategoryAlias,
  ModelsListQuery,
  ModelsListResponse,
  ModelDetailResponse,
  MintSummaryResponse,
  MintDimensionsResponse,
  MintMatrixResponse,
  MintRunsQuery,
  MintRunsResponse,
  MintBoxResponse,
  MintLanguageStatsResponse,
  MintFailuresResponse,
  MintContextProfilesResponse,
  MintActivityResponse,
  MintCategorySummaryResponse,
  MintCategoryDimensionsResponse,
  MintCategoryMatrixResponse,
  MintCategoryBoxResponse,
  MintCategoryFailuresResponse,
} from '../types/mint';

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
  /** CONST-28: additive — count of registered tool names under this module's
   *  `{module}_` prefix. Absent only if talking to a pre-CONST-28 backend. */
  toolCount?: number;
  /** CONST-28: additive — the module's full, sorted tool names. */
  tools?: string[];
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

// ── CONST-28 compat layer over the CONST-26 activity contract ───────────────
/** Alias — CONST-28's panels were built against this name; the canonical entry shape is
 *  CONST-26's [`ActivityEntry`]. */
export type TerminusActivityEntry = ActivityEntry;
/** CONST-28: degrade-aware response — `available:false` (never a throw) when the endpoint
 *  404/501s or the request fails, so ActivityPanel renders a "not live" empty state. A
 *  superset of [`ActivityFeedResponse`]; CONST-26 consumers keep reading `.entries`. */
export interface TerminusActivityResponse extends ActivityFeedResponse {
  available: boolean;
  detail?: string;
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
    /** CONST-26 contract (`GET /api/terminus/activity?limit=`), CONST-28 degrade semantics:
     *  never throws — `available:false` signals the endpoint is unreachable/not live, and the
     *  Overview feed/bell + ActivityPanel each render their own empty/degraded state. */
    activity(limit?: number): Promise<TerminusActivityResponse>;
  };
  /** CGUI-08: the fleet Models read surface (`/api/terminus/models*`, CONST-21).
   *  Consumed by CGUI-09 (Models module). */
  models: ModelsClient;
  /** CGUI-08: the MINT profiling read surface (`/api/terminus/mint/*`, CONST-21 +
   *  the CGUI-07 per-category endpoints). Consumed by CGUI-10 (MINT module). */
  mint: MintClient;
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

// ── Models / MINT data-client seam (CGUI-08, TERM #531) ──────────────────────
// The typed method group over `crate::constellation::models_api` (CONST-21 + CGUI-07). Every
// method is a read (GET) — none mutate, so none flow through `withMutationResultEvent`. Both
// adapters implement this identically-shaped contract: the httpAdapter fetches same-origin
// `/api/terminus/...`, the mockAdapter returns canned fixtures so CGUI-09/10 can build offline
// (VITE_AGG_MODE default is mock). Category methods accept the 8 canonical categories or the
// two friendly aliases (`vision_qa`, `stt`); an unknown category is a 400 from the backend.

/** Any category value the backend's `newcat_task_category` resolver accepts. */
export type MintCategoryKey = MintCategory | MintCategoryAlias;

export interface ModelsClient {
  /** `GET /api/terminus/models` — the unified fleet ⋈ brochure ⋈ serving ⋈ advisor list. */
  list(query?: ModelsListQuery): Promise<ModelsListResponse>;
  /** `GET /api/terminus/models/:name` — one model's full identity/brochure/serving/
   *  operational/catalog detail. `name` is URL-encoded (an HF repo id's `/` → `%2F`).
   *  Throws on a `404` (name unknown in every source), matching the backend contract. */
  model(name: string): Promise<ModelDetailResponse>;
}

/** Optional filters for `mint.box()` / `mint.languageStats()` etc. — mirrors the backend
 *  query structs; every field optional. */
export interface MintBoxQuery {
  /** `total_time_ms` (default) | `code_quality_score` — validated server-side (400 otherwise). */
  metric?: 'total_time_ms' | 'code_quality_score';
  model?: string;
  task_category?: string;
  language?: string;
  failure_class?: string;
  epoch?: string;
}

export interface MintClient {
  // Legacy (CONST-21) MINT views ---------------------------------------------
  summary(epoch?: string): Promise<MintSummaryResponse>;
  dimensions(params?: { models?: string[]; epoch?: string }): Promise<MintDimensionsResponse>;
  matrix(epoch?: string): Promise<MintMatrixResponse>;
  runs(query?: MintRunsQuery): Promise<MintRunsResponse>;
  box(query?: MintBoxQuery): Promise<MintBoxResponse>;
  languageStats(params?: { language?: string; epoch?: string }): Promise<MintLanguageStatsResponse>;
  failures(params?: { epoch?: string; task_category?: string }): Promise<MintFailuresResponse>;
  contextProfiles(models?: string[]): Promise<MintContextProfilesResponse>;
  activity(range?: '30d' | '90d' | 'all'): Promise<MintActivityResponse>;
  // CGUI-07 per-category views (the 8 new MINT task-categories) ---------------
  categorySummary(category: MintCategoryKey, epoch?: string): Promise<MintCategorySummaryResponse>;
  categoryDimensions(category: MintCategoryKey, epoch?: string): Promise<MintCategoryDimensionsResponse>;
  categoryMatrix(category: MintCategoryKey, epoch?: string): Promise<MintCategoryMatrixResponse>;
  categoryBox(category: MintCategoryKey, metric?: string, epoch?: string): Promise<MintCategoryBoxResponse>;
  categoryFailures(category: MintCategoryKey, epoch?: string): Promise<MintCategoryFailuresResponse>;
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

/** The keys the prefs seam will store — all non-secret UI state. `core` (S127) is the operator's
 *  last-selected Overview core tab (Lumina/Chord/Terminus/Harmony/Muse), persisted so the shell
 *  reopens on the same core. */
export type PrefsKey = 'layout' | 'density' | 'core';

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

const PREFS_ALLOWLIST: readonly PrefsKey[] = ['layout', 'density', 'core'];
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

/** Mock tool catalog per module — `plane` is padded out to 34 tools so ToolsPanel's DataTable
 *  paging has something real to page through (§ edge case: huge tool catalog). */
function toolNames(prefix: string, actions: string[]): string[] {
  return actions.map(a => `${prefix}_${a}`).sort();
}

const PLANE_ACTIONS = [
  'create_work_item', 'update_work_item', 'delete_work_item', 'list_work_items', 'get_work_item',
  'create_comment', 'list_comments', 'update_comment', 'delete_comment', 'list_states',
  'create_state', 'update_state', 'list_projects', 'get_project', 'create_project',
  'update_project', 'list_cycles', 'create_cycle', 'update_cycle', 'list_modules',
  'create_module', 'update_module', 'list_labels', 'create_label', 'assign_label',
  'list_members', 'add_member', 'remove_member', 'search_work_items', 'bulk_update',
  'list_attachments', 'add_attachment', 'get_activity', 'export_project',
];

const MOCK_TERMINUS_MODULE_TOOLS: Record<string, string[]> = {
  gitea: toolNames('gitea', ['list_repos', 'create_repo', 'get_file', 'create_pr', 'merge_pr']),
  plane: toolNames('plane', PLANE_ACTIONS),
  github: toolNames('github', ['list_repos', 'create_issue', 'list_issues']),
  nexus: [],
  commute: [],
};

const MOCK_TERMINUS_CONFIG: TerminusConfigSummary = {
  modules: [
    { name: 'gitea', enabled: true, version: '0.4.0', toolCount: MOCK_TERMINUS_MODULE_TOOLS.gitea.length, tools: MOCK_TERMINUS_MODULE_TOOLS.gitea },
    { name: 'plane', enabled: true, version: '0.4.0', toolCount: MOCK_TERMINUS_MODULE_TOOLS.plane.length, tools: MOCK_TERMINUS_MODULE_TOOLS.plane },
    { name: 'github', enabled: true, version: '0.4.0', toolCount: MOCK_TERMINUS_MODULE_TOOLS.github.length, tools: MOCK_TERMINUS_MODULE_TOOLS.github },
    { name: 'nexus', enabled: false, toolCount: 0, tools: [] },
    { name: 'commute', enabled: false, toolCount: 0, tools: [] },
  ],
  workerCount: 3,
};

// CONST-28: mock activity fixture, per the §8 contract shape. Real data lands with CONST-26's
// endpoint — this is a canned fixture only, timestamps relative to "now" so it always reads
// as recent in a live demo.
// OLDEST-FIRST, matching the real endpoint's file-order contract (CONST-26): index 0 is the
// oldest entry, the last element is the most recent — so `slice(-limit)` in the mock
// `activity()` returns the most-recent TAIL exactly like the server does (review fix: the
// previous newest-first generation inverted the shared contract for every consumer).
const MOCK_ACTIVITY_ENTRIES: TerminusActivityEntry[] = Array.from({ length: 24 }, (_, i) => {
  const systems: SystemId[] = ['harmony', 'chord', 'lumina', 'terminus'];
  const methods = ['GET', 'POST', 'PUT'];
  const paths = ['/status', '/agents/activity', '/models', '/config', '/health', '/mode'];
  const principals = ['operator', 'mock-user', 'ci-bot'];
  const age = 23 - i; // i=23 -> now (most recent, last); i=0 -> oldest
  return {
    ts: new Date(Date.now() - age * 45_000).toISOString(),
    method: methods[i % methods.length],
    path: paths[i % paths.length],
    principal: principals[i % principals.length],
    system: systems[i % systems.length],
  };
});

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

// CONST-20: past-dated entry included deliberately -- spec §5.4/edge cases requires the
// Premieres list to sort by release_date and render past-dated entries dimmed, not hidden.
const MOCK_MUSE_PREMIERE = {
  items: [
    { id: 'md-3', title: 'Example Upcoming Release', release_date: new Date(Date.now() + 5 * 86400000).toISOString(), rsvp_count: 0 },
    { id: 'md-4', title: 'Example Recent Premiere', release_date: new Date(Date.now() - 3 * 86400000).toISOString(), rsvp_count: 4 },
    { id: 'md-5', title: 'Example Far-Out Release', release_date: new Date(Date.now() + 30 * 86400000).toISOString(), rsvp_count: 1 },
  ],
};

const MOCK_MUSE_GAPS = {
  gaps: [
    { id: 'gap-1', title: 'Example Series — missing S2', kind: 'series', detail: 'season 2 not in library' },
    { id: 'gap-2', title: 'Example Collection — missing entry 3', kind: 'collection', detail: 'entry 3 of 5 missing' },
  ],
  total: 2,
};

// CONST-20: dashboard MetricCards row (library size, active channels, pending items, last
// ingest) has no dedicated endpoint in the §5.4 route list as written -- this mock/`GET
// /stats` extends the mock adapter (per this item's own instructions: "extend the mocks if
// the panels need shapes the canned data lacks, keep shapes consistent with the §5.4 endpoint
// list"). It's a plain GET like every other muse route, so it degrades through the exact same
// 404/501-to-ChartEmpty path if the real muse backend hasn't wired it -- see the DashboardPanel
// deviation note.
const MOCK_MUSE_STATS = {
  library_size: 1842,
  active_channels: 2,
  pending_items: 2,
  last_ingest_at: new Date(Date.now() - 45 * 60000).toISOString(),
};

const MOCK_MUSE_CHANNELS = {
  channels: [
    { id: 'ch-1', name: 'Mock Channel One', item_count: 12 },
    { id: 'ch-2', name: 'Mock Channel Two', item_count: 5 },
  ],
};

const MOCK_MUSE_LINEUP: Record<string, { channel_id: string; lineup: Array<{ id: string; title: string; position: number }> }> = {
  'ch-1': {
    channel_id: 'ch-1',
    lineup: [
      { id: 'md-1', title: 'Example Feature Film', position: 1 },
      { id: 'md-2', title: 'Example Series S1E4', position: 2 },
    ],
  },
  'ch-2': {
    channel_id: 'ch-2',
    lineup: [
      { id: 'md-3', title: 'Example Upcoming Release', position: 1 },
    ],
  },
};

const MOCK_MUSE_GUIDE = {
  entries: [
    { channel_id: 'ch-1', title: 'Example Feature Film', start: new Date().toISOString(), end: new Date(Date.now() + 2 * 3600000).toISOString() },
    { channel_id: 'ch-1', title: 'Example Series S1E4', start: new Date(Date.now() + 2 * 3600000).toISOString(), end: new Date(Date.now() + 3 * 3600000).toISOString() },
    { channel_id: 'ch-2', title: 'Example Upcoming Release', start: new Date().toISOString(), end: new Date(Date.now() + 90 * 60000).toISOString() },
  ],
};

// CONST-20: 5 clusters deliberately -- exercises the ">4 clusters fold to Other" rule (spec
// §5.4/§4.2 ALL_PAIRS_CEILING) with real mock data instead of only being provable by editing
// the mock in a manual check.
const MOCK_MUSE_TASTE_CLUSTERS = {
  clusters: [
    { cluster_id: 0, label: 'prestige-drama', points: [{ x: 0.12, y: 0.22, model: 'md-1' }, { x: 0.18, y: 0.30, model: 'md-6' }] },
    { cluster_id: 1, label: 'action-blockbuster', points: [{ x: 0.62, y: 0.41, model: 'md-2' }, { x: 0.58, y: 0.48, model: 'md-7' }] },
    { cluster_id: 2, label: 'animated-family', points: [{ x: 0.35, y: 0.75, model: 'md-8' }] },
    { cluster_id: 3, label: 'documentary', points: [{ x: 0.80, y: 0.20, model: 'md-9' }] },
    { cluster_id: 4, label: 'indie-comedy', points: [{ x: 0.50, y: 0.55, model: 'md-10' }] },
  ],
};

const MOCK_MUSE_WATCH_HISTORY = {
  series: [
    { date: '2026-07-01', 'prestige-drama': 3, 'action-blockbuster': 1, 'animated-family': 0 },
    { date: '2026-07-08', 'prestige-drama': 2, 'action-blockbuster': 2, 'animated-family': 1 },
    { date: '2026-07-15', 'prestige-drama': 4, 'action-blockbuster': 1, 'animated-family': 2 },
  ],
};

const MOCK_MUSE_GROUP_DYNAMICS = {
  rows: [
    { participant: 'household-a', watched_together_pct: 62, favorite_genre: 'prestige-drama', sessions: 14 },
    { participant: 'household-b', watched_together_pct: 38, favorite_genre: 'action-blockbuster', sessions: 9 },
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
  // CONST-20: not in the §5.4 route list as written -- see the MOCK_MUSE_STATS comment above
  // for why the dashboard MetricCards row calls this anyway (mock-adapter extension, same
  // GET-and-degrade shape as every other muse route).
  'muse /stats': MOCK_MUSE_STATS,
  'muse /api/channels': MOCK_MUSE_CHANNELS,
  'muse /api/graph/taste-clusters': MOCK_MUSE_TASTE_CLUSTERS,
  'muse /api/graph/watch-history': MOCK_MUSE_WATCH_HISTORY,
  'muse /api/graph/group-dynamics': MOCK_MUSE_GROUP_DYNAMICS,
  'muse /guide': MOCK_MUSE_GUIDE,
};

function mockGetFor(system: SystemId, pathname: string): unknown {
  const key = `${system} ${pathname}`;
  if (key in MOCK_GET) return MOCK_GET[key];
  if (system === 'harmony' && pathname.startsWith('/tree/')) {
    return { ...MOCK_TREE, project: decodeURIComponent(pathname.slice('/tree/'.length)) };
  }
  if (system === 'muse' && pathname.startsWith('/api/channels/') && pathname.endsWith('/lineup')) {
    const channelId = pathname.split('/')[3];
    return MOCK_MUSE_LINEUP[channelId] ?? { channel_id: channelId, lineup: [] };
  }
  return null;
}

// ── Mock data for the Lumina Conversations panel (LGUI-07; spec §3.2/§7) ────
// The real endpoint is lumina's pre-existing, NON-streaming `POST /v1/chat/completions`
// (spec §0.2), reached here as `lumina /v1/chat/completions`. Response shapes mirror the
// OpenAI-style success envelope and the constellation-wide `{error:{message,type}}` shape
// (spec §7). Body content drives which canned fixture comes back, so the panel's error-type
// mapping, XSS-inertness, and long-reply scroll behavior are all reviewable on mocks alone —
// type a trigger phrase (case-insensitive substring match) into the composer:
//   "trigger:ratelimit" -> rate_limit_error envelope ("Daily turn budget reached")
//   "trigger:upstream"  -> upstream_error envelope ("Chord unreachable")
//   "trigger:other"     -> an error.type the panel doesn't special-case (generic inline+retry)
//   "trigger:xss"       -> assistant reply containing a literal <script> tag (XSS-inert proof,
//                          see ChatBubble.tsx's render-path doc comment + chatMarkdown.test.ts)
//   "trigger:long"      -> a 4000+ char reply (scrollable-bubble proof)
//   anything else       -> a short canned reply exercising **bold**/`code`/a link/a fenced block
function mockLuminaChatReply(body: string | undefined): unknown {
  let lastUserContent = '';
  try {
    const parsed = body ? JSON.parse(body) as { messages?: Array<{ role: string; content: string }> } : undefined;
    const messages = parsed?.messages ?? [];
    for (let i = messages.length - 1; i >= 0; i -= 1) {
      if (messages[i].role === 'user') { lastUserContent = messages[i].content; break; }
    }
  } catch {
    // Malformed body in mock mode — fall through to the default reply rather than throwing.
  }
  const lower = lastUserContent.toLowerCase();

  if (lower.includes('trigger:ratelimit')) {
    return { error: { message: 'Daily turn budget reached for this user.', type: 'rate_limit_error' } };
  }
  if (lower.includes('trigger:upstream')) {
    return { error: { message: 'Chord proxy unreachable.', type: 'upstream_error' } };
  }
  if (lower.includes('trigger:other')) {
    return { error: { message: 'An unexpected error occurred.', type: 'internal_error' } };
  }
  if (lower.includes('trigger:xss')) {
    return {
      choices: [{
        message: {
          role: 'assistant',
          content: 'Here is something odd: <script>alert(1)</script> — it should render as inert text, never execute.',
        },
      }],
    };
  }
  if (lower.includes('trigger:long')) {
    const paragraph = 'This is a long mock reply used to prove the chat bubble scrolls instead of overflowing the panel. ';
    const content = paragraph.repeat(Math.ceil(4200 / paragraph.length));
    return { choices: [{ message: { role: 'assistant', content } }] };
  }
  const routed = lower.startsWith('/deep ') ? ' (routed: deep)' : lower.startsWith('/quick ') ? ' (routed: quick)' : '';
  return {
    choices: [{
      message: {
        role: 'assistant',
        content:
          `(mock adapter — no live Chord backend) Got it${routed}. Here's a bit of **bold**, ` +
          'some `inline code`, a [link](https://example.com), and a fenced block:\n\n' +
          '```\nfn hello() {\n    println!("hi from mock Lumina");\n}\n```',
      },
    }],
  };
}

/** POST/PUT-style mock acks — every write in the mock world just succeeds with a canned shape. */
function mockWriteFor(system: SystemId, pathname: string, body?: string): unknown {
  if (system === 'lumina' && pathname === '/v1/chat/completions') {
    return mockLuminaChatReply(body);
  }
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
  // CONST-20: Muse channel compose/maintenance actions -- not in the §5.4 route list as
  // written (only the read routes are spec'd), inferred from the spec's own description of
  // these as "compose/maintenance actions" gated behind RoleGate+ConfirmDialog (§5.4). Kept
  // to the same REST shape as the read routes (`/api/channels/{id}/...`) pending the real
  // muse backend confirming its exact mutation contract.
  const composeMatch = system === 'muse' && /^\/api\/channels\/([^/]+)\/compose$/.exec(pathname);
  if (composeMatch) {
    return { ok: true, channel_id: composeMatch[1], status: 'queued' };
  }
  const maintenanceMatch = system === 'muse' && /^\/api\/channels\/([^/]+)\/maintenance$/.exec(pathname);
  if (maintenanceMatch) {
    return { ok: true, channel_id: maintenanceMatch[1], status: 'queued' };
  }
  return { ok: true };
}

function mockRequest<T>(system: SystemId, path: string, init?: RequestInit): Promise<T> {
  const method = (init?.method ?? 'GET').toUpperCase();
  const pathname = path.split('?')[0];
  const value = method === 'GET'
    ? mockGetFor(system, pathname)
    : mockWriteFor(system, pathname, typeof init?.body === 'string' ? init.body : undefined);
  // LGUI-07: the chat round trip gets a deliberately longer canned delay than the default
  // 120ms — enough for the composer's "thinking" StatusPill to be visibly reviewable, still
  // fast enough not to be annoying. Every other mock write keeps the default.
  const ms = system === 'lumina' && pathname === '/v1/chat/completions' ? 650 : undefined;
  return delay(value as T, ms);
}

/** Mock WS: reports "connected" immediately, never emits events (mock has no live daemon). */
function mockWsConnect(handlers: WsHandlers): WsConnection {
  const id = setTimeout(() => handlers.onOpen?.(), 50);
  return {
    send() { /* no-op in mock mode */ },
    close() { clearTimeout(id); handlers.onClose?.(); },
  };
}

// ── Mock data for the Models/MINT surface (CGUI-08, TERM #531) ───────────────
// Canned fixtures so the Models (CGUI-09) and MINT (CGUI-10) modules can be built,
// typechecked, and demoed with zero backend. Shapes are 1:1 with `models_api.rs` — the same
// contract the httpAdapter's real fetches consume. Values are plausible, not real.

/** Fixed base epoch for every CGUI-08 mock timestamp — deterministic, evaluated once at
 *  module load (never `Date.now()` at request time), so repeated calls and snapshot tests
 *  see byte-identical payloads. 2026-07-21T12:00:00Z. */
const MOCK_NOW_MS = Date.UTC(2026, 6, 21, 12, 0, 0);

/** The 8 assistant-suite radar axes (mirrors `ASSISTANT_DIMENSIONS` in models_api.rs). */
const MOCK_ASSISTANT_DIMENSIONS = [
  'conversation_depth', 'tool_chaining', 'memory_integration', 'personality_latent',
  'personality_prompted', 'embeddings', 'yarn_context_depth', 'fleet_membership',
] as const;

/** The metrics each new MINT category reports (plausible per-suite metric names). */
const MOCK_MINT_CATEGORY_METRICS: Record<MintCategory, string[]> = {
  embedding_retrieval: ['ndcg_at_10', 'mrr', 'recall_at_10'],
  reranking: ['ndcg_at_10', 'map'],
  image_parsing: ['accuracy', 'description_quality'],
  document_parsing: ['cer', 'layout_f1'],
  image_generation: ['clip_score', 'aesthetic_score'],
  voice_transcription: ['wer', 'cer'],
  tts: ['mos', 'wer'],
  tool_routing: ['accuracy', 'f1'],
};

/** Two plausible models per category, so every mock view has ≥2 rows to render. */
const MOCK_MINT_CATEGORY_MODELS: Record<MintCategory, string[]> = {
  embedding_retrieval: ['bge-m3', 'nomic-embed-text-v1.5'],
  reranking: ['bge-reranker-v2-m3', 'jina-reranker-v2'],
  image_parsing: ['qwen2.5-vl:7b', 'llava:13b'],
  document_parsing: ['got-ocr2', 'docling-layout'],
  image_generation: ['sdxl-turbo', 'flux.1-schnell'],
  voice_transcription: ['whisper-large-v3', 'parakeet-tdt'],
  tts: ['kokoro-82m', 'xtts-v2'],
  tool_routing: ['qwen2.5:7b', 'hermes-3-llama-3.1-8b'],
};

/** A plausible baseline for a metric (higher-is-better ~0.6–0.95; error rates ~0.05–0.25;
 *  MOS ~3.5–4.5; clip/aesthetic on their own scales). Deterministic — no RNG in fixtures. */
function mockMetricBase(metric: string): number {
  switch (metric) {
    case 'wer': case 'cer': return 0.11;
    case 'mos': return 4.1;
    case 'clip_score': return 0.31;
    case 'aesthetic_score': return 6.2;
    default: return 0.82; // ndcg/mrr/recall/map/accuracy/f1/layout_f1/description_quality
  }
}

/** Deterministic per-(model-index, metric-index) jitter around the metric baseline. */
function mockMetricValue(metric: string, modelIdx: number, metricIdx: number): number {
  const base = mockMetricBase(metric);
  const delta = base * (0.04 * modelIdx + 0.02 * metricIdx);
  const raw = metric === 'wer' || metric === 'cer' ? base + delta : base - delta;
  return Math.round(raw * 1000) / 1000;
}

function mockCategoryLastRun(modelIdx: number, metricIdx: number): string {
  return new Date(MOCK_NOW_MS - (modelIdx * 3600_000 + metricIdx * 600_000)).toISOString();
}

/** The legacy MINT suites (their own dedicated readers), accepted by `mint.runs` alongside the
 *  8 new categories — mirrors the backend allowlist in `mint_runs`. */
const MOCK_LEGACY_SUITES = new Set<string>(['code', 'context', 'agent']);

/** Resolve a category key (canonical or alias) to its canonical form, mirroring the backend's
 *  `newcat_task_category`. Returns null for an unknown key — the mock adapter treats that like
 *  the backend's `400`. */
function mockResolveCategory(key: string): MintCategory | null {
  if (key in MOCK_MINT_CATEGORY_METRICS) return key as MintCategory;
  if (key === 'vision_qa') return 'image_parsing';
  if (key === 'stt' || key === 'asr' || key === 'asr_transcription') return 'voice_transcription';
  return null;
}

function mockCategoryOr400(key: string): MintCategory {
  const cat = mockResolveCategory(key);
  if (!cat) {
    throw new Error(
      `HTTP 400 — unrecognized category '${key}' (expected one of: ${Object.keys(MOCK_MINT_CATEGORY_METRICS).join(', ')})`,
    );
  }
  return cat;
}

function mockCategorySummary(cat: MintCategory): MintCategorySummaryResponse {
  const metrics = MOCK_MINT_CATEGORY_METRICS[cat];
  return {
    models: MOCK_MINT_CATEGORY_MODELS[cat].map((model_id, mi) => ({
      model_id,
      metrics: metrics.map((metric, i) => ({
        dimension: cat,
        metric,
        value: mockMetricValue(metric, mi, i),
        std_dev: Math.round((0.02 + 0.01 * i) * 1000) / 1000,
        low_confidence: mi === 1 && i === 0,
        backend_tag: 'gpu',
        last_run_at: mockCategoryLastRun(mi, i),
      })),
    })),
  };
}

function mockCategoryDimensions(cat: MintCategory): MintCategoryDimensionsResponse {
  return {
    dimensions: MOCK_MINT_CATEGORY_METRICS[cat].map(metric => ({ dimension: cat, metric })),
  };
}

function mockCategoryMatrix(cat: MintCategory): MintCategoryMatrixResponse {
  const metrics = MOCK_MINT_CATEGORY_METRICS[cat];
  const models = MOCK_MINT_CATEGORY_MODELS[cat];
  const cells = models.flatMap((model, mi) =>
    metrics.map((metric, i) => ({
      model,
      metric,
      dimension: cat,
      mean: mockMetricValue(metric, mi, i),
      n: 5 - i,
      low_confidence: mi === 1 && i === 0,
      last_run_at: mockCategoryLastRun(mi, i),
    })),
  );
  return { models, columns: metrics, cells };
}

function mockCategoryBox(cat: MintCategory, metric?: string): MintCategoryBoxResponse {
  const chosen = metric ?? MOCK_MINT_CATEGORY_METRICS[cat][0];
  if (!MOCK_MINT_CATEGORY_METRICS[cat].includes(chosen)) {
    // fail-open to empty groups, exactly like `shape_newcat_box`
    return { metric: chosen, groups: [] };
  }
  const groups = MOCK_MINT_CATEGORY_MODELS[cat].map((model, mi) => {
    const mid = mockMetricValue(chosen, mi, 0);
    const spread = Math.max(Math.abs(mid) * 0.08, 0.01);
    return {
      model,
      min: Math.round((mid - 2 * spread) * 1000) / 1000,
      q1: Math.round((mid - spread) * 1000) / 1000,
      median: mid,
      q3: Math.round((mid + spread) * 1000) / 1000,
      max: Math.round((mid + 2 * spread) * 1000) / 1000,
      n: 8 - mi,
      low_n: 8 - mi < 5,
      outliers: mi === 0
        ? [{ run_id: `${cat}-${model}-out`, value: Math.round((mid + 4 * spread) * 1000) / 1000, low_confidence: false }]
        : [],
    };
  });
  return { metric: chosen, groups };
}

function mockCategoryFailures(cat: MintCategory): MintCategoryFailuresResponse {
  return {
    classes: ['low_confidence', 'ok'],
    models: MOCK_MINT_CATEGORY_MODELS[cat].map((model, mi) => {
      const total = 10 - mi * 2;
      const low = mi; // model 0 all-ok, later models a couple low-conf
      return { model, counts: { low_confidence: low, ok: total - low }, total_runs: total };
    }),
  };
}

const MOCK_MODELS_LIST: ModelsListResponse = {
  total: 3,
  refreshed_at: new Date(MOCK_NOW_MS - 30 * 60000).toISOString(),
  models: [
    {
      model_name: 'qwen2.5-coder:32b', family: 'qwen2.5-coder', params_b: 32, quant: 'Q4_K_M',
      category: 'code', brochure_status: 'in_fleet', in_current_fleet: true, discovery_score: 0.91,
      vram_gb: 21.5, size_b: 32, serving_now: true,
      coverage: { coder: true, assistant: true, serving: true, agent: false },
      best_pass_rate: 0.78, last_run_at: new Date(MOCK_NOW_MS - 2 * 3600_000).toISOString(),
    },
    {
      model_name: 'bge-m3', family: 'bge', params_b: 0.57, quant: null,
      category: 'embedding_retrieval', brochure_status: 'in_fleet', in_current_fleet: true,
      discovery_score: 0.84, vram_gb: 2.1, size_b: 0.57, serving_now: false,
      coverage: { coder: false, assistant: true, serving: true, agent: false },
      best_pass_rate: null, last_run_at: new Date(MOCK_NOW_MS - 26 * 3600_000).toISOString(),
    },
    {
      model_name: 'flux.1-schnell', family: 'flux', params_b: 12, quant: 'fp8',
      category: 'image_generation', brochure_status: 'candidate', in_current_fleet: false,
      discovery_score: 0.66, vram_gb: 16.0, size_b: 12, serving_now: false,
      coverage: { coder: false, assistant: false, serving: false, agent: false },
      best_pass_rate: null, last_run_at: null,
    },
  ],
};

function mockModelDetail(name: string): ModelDetailResponse {
  const entry = MOCK_MODELS_LIST.models.find(m => m.model_name === name);
  if (!entry) {
    // Mirror the backend's real 404 for a name absent from every source.
    throw new Error(`HTTP 404 for /api/terminus/models/${encodeURIComponent(name)}`);
  }
  return {
    identity: {
      family: entry.family ?? name, params_b: entry.params_b, active_b: entry.params_b,
      architecture: 'transformer',
      quants: entry.quant ? { [entry.quant]: { vram_gb: entry.vram_gb, quality_penalty: 0.02 } } : {},
      quality: 'good', best_for: ['general'], avoid_for: [],
      ollama_name: entry.model_name, notes: 'mock adapter — canned identity',
    },
    brochure: {
      hf_repo: `mock/${entry.model_name}`, category: entry.category ?? 'unknown',
      status: entry.brochure_status ?? 'candidate', gfx1151_class: 'green',
      size_b: entry.size_b, vram_footprint_gb: entry.vram_gb, discovery_source: 'mock',
      discovery_score: entry.discovery_score,
      discovered_at: new Date(MOCK_NOW_MS - 10 * 86400_000).toISOString(),
      last_seen_at: new Date(MOCK_NOW_MS - 86400_000).toISOString(),
      fetched_at: new Date(MOCK_NOW_MS - 86400_000).toISOString(),
      marked_for_fleet_at: entry.in_current_fleet ? new Date(MOCK_NOW_MS - 5 * 86400_000).toISOString() : null,
      evicted_at: null, rationale: 'mock candidate',
    },
    serving: entry.serving_now
      ? [{
          backend_tag: 'gpu', best_runtime: 'llama.cpp', tok_s: 63.4, vram_or_ram_peak_gb: entry.vram_gb,
          cold_load_s: 4.2, keep_warm: true, fallback_runtime: 'vulkan', exclusion_reason: null,
          recheck_trigger: null, provenance: 'mock', updated_at: new Date(MOCK_NOW_MS).toISOString(),
        }]
      : [],
    operational: entry.coverage.coder
      ? {
          max_context_safe: 32768, max_context_absolute: 65536, quality_degradation_point: 40000,
          throughput_at_2k: 60, throughput_at_8k: 52, throughput_at_16k: 44, throughput_at_32k: 33,
          throughput_at_64k: 20, recommended_timeout_chat_sec: 60, recommended_timeout_build_sec: 300,
          recommended_timeout_deep_sec: 900, overall_tier: 'A',
        }
      : null,
    catalog: {
      card: {
        model_name: entry.model_name, quant: entry.quant, in_current_fleet: entry.in_current_fleet,
        serving: null, not_run_count: 1, stale_count: 0, refreshed_at: MOCK_MODELS_LIST.refreshed_at,
      },
      cells: [{
        test_type: 'coder', task_category: 'code_generation', quant: entry.quant, status: 'run',
        pass_rate: entry.best_pass_rate, n_samples: 40, score_stddev: 0.05,
        low_confidence: false, last_run_at: entry.last_run_at, harness_version: 'coder-v2',
      }],
    },
  };
}

const MOCK_MINT_SUMMARY: MintSummaryResponse = {
  models_profiled: 12,
  runs: { code: 5721, context: 340, agent: 128, total: 6189 },
  fleet_best_model: { model: 'qwen2.5-coder:32b', pass_hat_3: 0.81 },
  gpu_hours: 214.6,
  epoch: 'coder-v2',
  became_current_at: new Date(MOCK_NOW_MS - 20 * 86400_000).toISOString(),
};

const MOCK_MINT_DIMENSIONS: MintDimensionsResponse = {
  dimensions: [...MOCK_ASSISTANT_DIMENSIONS],
  models: ['qwen2.5-coder:32b', 'llama3.3:70b'].map((model_id, mi) => ({
    model_id,
    scores: MOCK_ASSISTANT_DIMENSIONS.map((dimension, i) => ({
      dimension,
      norm: Math.round((0.4 + 0.06 * i - 0.05 * mi) * 1000) / 1000,
      raw: Math.round((0.5 + 0.04 * i) * 1000) / 1000,
      metric: 'value',
      std_dev: 0.03,
      n: 6,
      low_confidence: i === 7,
    })),
  })),
  fleet_median: MOCK_ASSISTANT_DIMENSIONS.map((dimension, i) => ({
    dimension,
    norm: Math.round((0.45 + 0.04 * i) * 1000) / 1000,
  })),
};

const MOCK_MINT_MATRIX: MintMatrixResponse = {
  models: ['qwen2.5-coder:32b', 'bge-m3'],
  columns: [
    { test_type: 'coder', task_category: 'code_generation' },
    { test_type: 'assistant', task_category: 'embedding_retrieval' },
  ],
  cells: [
    {
      model: 'qwen2.5-coder:32b', col: { test_type: 'coder', task_category: 'code_generation' },
      status: 'run', pass_rate: 0.78, n_samples: 40, score_stddev: 0.05, low_confidence: false,
      last_run_at: new Date(MOCK_NOW_MS - 2 * 3600_000).toISOString(), harness_version: 'coder-v2',
    },
    {
      model: 'bge-m3', col: { test_type: 'assistant', task_category: 'embedding_retrieval' },
      status: 'run', pass_rate: 0.82, n_samples: 30, score_stddev: 0.03, low_confidence: false,
      last_run_at: new Date(MOCK_NOW_MS - 26 * 3600_000).toISOString(), harness_version: 'a1',
    },
    {
      model: 'bge-m3', col: { test_type: 'coder', task_category: 'code_generation' },
      status: 'not_run', pass_rate: null, n_samples: null, score_stddev: null,
      low_confidence: false, last_run_at: null, harness_version: null,
    },
  ],
};

const MOCK_MINT_RUNS: MintRunsResponse = {
  total: 2,
  runs: [
    {
      run_id: 'run-code-1', model: 'qwen2.5-coder:32b', metric: 'code_quality_score', value: 0.79,
      dimension: 'code_generation', backend_tag: 'gpu', judge: 'harness', low_confidence: false,
      created_at: new Date(MOCK_NOW_MS - 2 * 3600_000).toISOString(), harness_version: 'coder-v2',
    },
    {
      run_id: 'run-code-2', model: 'qwen2.5-coder:32b', metric: 'total_time_ms', value: 4200,
      dimension: 'code_generation', backend_tag: 'gpu', judge: 'harness', low_confidence: false,
      created_at: new Date(MOCK_NOW_MS - 3 * 3600_000).toISOString(), harness_version: 'coder-v2',
    },
  ],
};

const MOCK_MINT_BOX: MintBoxResponse = {
  groups: [
    {
      model: 'qwen2.5-coder:32b', min: 3200, q1: 3800, median: 4200, q3: 4700, max: 5400,
      n: 40, low_n: false,
      outliers: [{ run_id: 'run-code-slow', value: 9800, case_id: 'case-77', failure_class: 'timeout' }],
    },
    {
      model: 'llama3.3:70b', min: 5100, q1: 6000, median: 6800, q3: 7600, max: 8800,
      n: 3, low_n: true, outliers: [],
    },
  ],
};

const MOCK_MINT_LANGUAGE_STATS: MintLanguageStatsResponse = {
  rows: [
    {
      model: 'qwen2.5-coder:32b', language: 'python', n_scored: 120, mean_score: 0.81,
      stddev_score: 0.09, retry_lift: 0.06, mean_throughput: 58.2, mean_latency_ms: 1800,
      p95_latency_ms: 3400, total_gpu_seconds: 640.5, quality_per_gpu_second: 0.0013,
      pass_hat_3: 0.83, vram_gb: 21.5, point_size_px: 22.4,
    },
    {
      model: 'bge-m3', language: 'rust', n_scored: 40, mean_score: 0.72, stddev_score: 0.11,
      retry_lift: 0.03, mean_throughput: 44.0, mean_latency_ms: 2100, p95_latency_ms: 3900,
      total_gpu_seconds: 120.0, quality_per_gpu_second: 0.006, pass_hat_3: 0.70, vram_gb: 2.1,
      point_size_px: 8.0,
    },
  ],
};

const MOCK_MINT_FAILURES: MintFailuresResponse = {
  classes: ['timeout', 'wrong_output', 'compile_error', 'refusal', 'oom', 'other'],
  models: [
    {
      model: 'qwen2.5-coder:32b',
      counts: { timeout: 3, wrong_output: 8, compile_error: 5, refusal: 1, oom: 0, other: 2 },
      total_runs: 40,
    },
    {
      model: 'llama3.3:70b',
      counts: { timeout: 6, wrong_output: 4, compile_error: 2, refusal: 0, oom: 3, other: 1 },
      total_runs: 32,
    },
  ],
};

const MOCK_MINT_CONTEXT_PROFILES: MintContextProfilesResponse = {
  models: ['qwen2.5-coder:32b', 'llama3.3:70b'].map((model, mi) => ({
    model,
    max_context_safe: 32768 - mi * 8192,
    tiers: [2000, 8000, 16000, 32000, 64000].map((context_tokens, i) => ({
      context_tokens,
      throughput_tok_per_sec: Math.round((60 - i * 8 - mi * 5) * 10) / 10,
      ttft_ms: 200 + i * 150 + mi * 100,
      recall_score: Math.round((0.98 - i * 0.06) * 1000) / 1000,
      memory_usage_mb: 12000 + i * 4000 + mi * 6000,
      oom: i === 4 && mi === 1,
    })),
  })),
};

const MOCK_MINT_ACTIVITY: MintActivityResponse = {
  days: Array.from({ length: 14 }, (_, i) => {
    const d = new Date(MOCK_NOW_MS - (13 - i) * 86400_000);
    return {
      date: d.toISOString().slice(0, 10),
      code: 20 + (i % 5) * 6,
      context: 2 + (i % 3),
      agent: i % 4,
    };
  }),
  epochs: [
    { epoch: 'coder-v2', became_current_at: new Date(MOCK_NOW_MS - 20 * 86400_000).toISOString(), note: 'v2 harness cutover' },
  ],
};

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
      const entries = limit != null ? MOCK_ACTIVITY_ENTRIES.slice(-limit) : MOCK_ACTIVITY_ENTRIES;
      return delay({ entries, available: true });
    },
  },
  models: {
    async list(query?: ModelsListQuery) {
      // A minimal offline filter so the mock behaves like the endpoint for the common
      // scope/q/serving filters the Models module will drive.
      let models = MOCK_MODELS_LIST.models;
      if (query?.scope === 'fleet') models = models.filter(m => m.in_current_fleet);
      else if (query?.scope === 'brochure') models = models.filter(m => m.brochure_status != null);
      if (query?.q) {
        const q = query.q.toLowerCase();
        models = models.filter(m =>
          m.model_name.toLowerCase().includes(q) || (m.family ?? '').toLowerCase().includes(q));
      }
      if (query?.category) models = models.filter(m => m.category === query.category);
      if (query?.status) models = models.filter(m => m.brochure_status === query.status);
      if (query?.serving != null) models = models.filter(m => m.serving_now === query.serving);
      // S127 (DATA-04): mirror the backend's paginate() — `total` is the FULL filtered count
      // (the true roster scale), `models` is only the requested page. offset/limit default to
      // 0 / 50 and the server clamps limit to [1, 500].
      const total = models.length;
      const offset = Math.max(0, query?.offset ?? 0);
      const limit = Math.min(500, Math.max(1, query?.limit ?? 50));
      const page = models.slice(offset, offset + limit);
      return delay({ total, refreshed_at: MOCK_MODELS_LIST.refreshed_at, models: page });
    },
    async model(name: string) {
      return delay(mockModelDetail(name));
    },
  },
  mint: {
    async summary() { return delay(MOCK_MINT_SUMMARY); },
    async dimensions() { return delay(MOCK_MINT_DIMENSIONS); },
    async matrix() { return delay(MOCK_MINT_MATRIX); },
    async runs(query?: MintRunsQuery) {
      // Validate the suite exactly like the backend's widened allowlist (models_api.rs
      // `mint_runs`): legacy `code|context|agent`, any of the 8 new categories, or a category
      // alias succeeds; a truly-unknown suite is a 400-equivalent throw (parity with the mock's
      // categorySummary guard) — never a silent fall-through to the canned legacy page.
      const suite = query?.suite ?? 'code';
      const cat = mockResolveCategory(suite);
      if (!cat && !MOCK_LEGACY_SUITES.has(suite)) {
        throw new Error(
          `HTTP 400 — unrecognized suite '${suite}' (expected one of: code, context, agent, ` +
          `${Object.keys(MOCK_MINT_CATEGORY_METRICS).join(', ')} (category aliases: vision_qa, stt))`,
        );
      }
      // A new-category suite reads the category's rows via the summary fixture, shaped as runs;
      // legacy code/context/agent return the canned run page.
      if (cat) {
        const summary = mockCategorySummary(cat);
        const runs: MintRunsResponse['runs'] = summary.models.flatMap(m =>
          m.metrics
            .filter(mt => !query?.metric || mt.metric === query.metric)
            .filter(() => !query?.model || m.model_id === query.model)
            .map(mt => ({
              run_id: `${cat}-${m.model_id}-${mt.metric}`, model: m.model_id, backend_tag: mt.backend_tag,
              dimension: mt.dimension, metric: mt.metric, value: mt.value, std_dev: mt.std_dev,
              judge: 'harness', low_confidence: mt.low_confidence, created_at: mt.last_run_at,
              harness_version: 'a1',
            })));
        return delay({ total: runs.length, runs });
      }
      return delay(MOCK_MINT_RUNS);
    },
    async box() { return delay(MOCK_MINT_BOX); },
    async languageStats() { return delay(MOCK_MINT_LANGUAGE_STATS); },
    async failures() { return delay(MOCK_MINT_FAILURES); },
    async contextProfiles() { return delay(MOCK_MINT_CONTEXT_PROFILES); },
    async activity() { return delay(MOCK_MINT_ACTIVITY); },
    async categorySummary(category: MintCategoryKey) {
      return delay(mockCategorySummary(mockCategoryOr400(category)));
    },
    async categoryDimensions(category: MintCategoryKey) {
      return delay(mockCategoryDimensions(mockCategoryOr400(category)));
    },
    async categoryMatrix(category: MintCategoryKey) {
      return delay(mockCategoryMatrix(mockCategoryOr400(category)));
    },
    async categoryBox(category: MintCategoryKey, metric?: string) {
      return delay(mockCategoryBox(mockCategoryOr400(category), metric));
    },
    async categoryFailures(category: MintCategoryKey) {
      return delay(mockCategoryFailures(mockCategoryOr400(category)));
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
//   GET  /api/terminus/config    -> TerminusConfigSummary (CONST-28: modules[].toolCount/tools additive)
//   GET  /api/terminus/activity?limit=N -> ActivityFeedResponse (CONST-26; never body content;
//                                    CONST-28 client degrades to {available:false} on 404/501/error)
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
//            CONST-20 additions (not in the original §5.4 route list -- see aggregationClient's
//            MOCK_MUSE_STATS/compose/maintenance comments for why): GET /stats (dashboard
//            MetricCards row), POST /api/channels/{id}/compose, POST /api/channels/{id}/
//            maintenance (both operator-gated + confirmed client-side, §5.4). All three are
//            plain passthrough paths under the existing `proxy_muse` arm -- no proxy.rs change
//            needed, they degrade exactly like every other unwired muse route (404/501 ->
//            ChartEmpty "not yet wired") until the real muse backend implements them.

function baseUrl(): string {
  // Same-origin only — never a hardcoded host/port. This is the one place in the app
  // permitted to read window.location.
  return window.location.origin;
}

// The single-auth invariant, enforced structurally: Content-Type is always JSON and
// authoritative; no caller-supplied auth-bearing header is ever forwarded to the backend.
//
// LGUI-05: `x-lumina-user` joins this strip list -- it's not itself a credential, but it's an
// identity-spoofing vector for Lumina's admin-gated routes (spec §7 C-1), and the browser has
// no business setting it either way: `crate::constellation::proxy::proxy_lumina` derives it
// server-side from the VERIFIED session cookie, never from a request header. This is a
// defense-in-depth door only -- the Rust proxy independently never reads ANY inbound header
// but `content-type` to build its own outbound `Authorization`/`X-Lumina-User` (see that
// module's doc), so a caller-supplied value here couldn't reach Lumina even if this stripped
// nothing at all.
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
      if (lk === 'authorization' || lk === 'cookie' || lk === 'content-type' || lk === 'x-lumina-user') continue;
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

/** Build a `?a=1&b=2` query string from a params object, dropping `undefined`/`null`/`''`
 *  values and encoding the rest. Returns `''` when nothing to add. Used by the CGUI-08
 *  Models/MINT reads (their only same-origin query-carrying GETs). */
function buildQuery(params: Record<string, string | number | boolean | undefined | null>): string {
  const parts: string[] = [];
  for (const [k, v] of Object.entries(params)) {
    if (v === undefined || v === null || v === '') continue;
    parts.push(`${encodeURIComponent(k)}=${encodeURIComponent(String(v))}`);
  }
  return parts.length ? `?${parts.join('&')}` : '';
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
      // CONST-28/§8: degrade gracefully (available:false) rather than throw — 404/501 on a
      // deploy without the endpoint, or any transient failure. Both the Overview feed/bell
      // (CONST-26) and ActivityPanel read `.entries`; the flag is additive. `limit` stays
      // OPTIONAL (review fix): omitted ⇒ no query param ⇒ the server's own configured cap
      // applies, exactly as the CONST-26 contract documents.
      try {
        const query = limit != null ? `?limit=${encodeURIComponent(String(limit))}` : '';
        const res = await httpJson<ActivityFeedResponse>(`/api/terminus/activity${query}`);
        return { entries: res.entries, available: true };
      } catch (e) {
        return { entries: [], available: false, detail: e instanceof Error ? e.message : 'unavailable' };
      }
    },
  },
  models: {
    async list(query?: ModelsListQuery) {
      const q = buildQuery({
        scope: query?.scope, q: query?.q, category: query?.category, status: query?.status,
        serving: query?.serving, limit: query?.limit, offset: query?.offset,
      });
      return httpJson<ModelsListResponse>(`/api/terminus/models${q}`);
    },
    async model(name: string) {
      // Encode the whole name as ONE path segment — an HF repo id's `/` becomes `%2F`, which
      // the backend's `Path<String>` extractor decodes back within this single segment.
      return httpJson<ModelDetailResponse>(`/api/terminus/models/${encodeURIComponent(name)}`);
    },
  },
  mint: {
    async summary(epoch?: string) {
      return httpJson<MintSummaryResponse>(`/api/terminus/mint/summary${buildQuery({ epoch })}`);
    },
    async dimensions(params?: { models?: string[]; epoch?: string }) {
      const q = buildQuery({ models: params?.models?.join(','), epoch: params?.epoch });
      return httpJson<MintDimensionsResponse>(`/api/terminus/mint/dimensions${q}`);
    },
    async matrix(epoch?: string) {
      return httpJson<MintMatrixResponse>(`/api/terminus/mint/matrix${buildQuery({ epoch })}`);
    },
    async runs(query?: MintRunsQuery) {
      const q = buildQuery({
        suite: query?.suite, model: query?.model, task_category: query?.task_category,
        language: query?.language, failure_class: query?.failure_class, metric: query?.metric,
        epoch: query?.epoch, limit: query?.limit, offset: query?.offset,
      });
      return httpJson<MintRunsResponse>(`/api/terminus/mint/runs${q}`);
    },
    async box(query?: MintBoxQuery) {
      const q = buildQuery({
        metric: query?.metric, model: query?.model, task_category: query?.task_category,
        language: query?.language, failure_class: query?.failure_class, epoch: query?.epoch,
      });
      return httpJson<MintBoxResponse>(`/api/terminus/mint/box${q}`);
    },
    async languageStats(params?: { language?: string; epoch?: string }) {
      const q = buildQuery({ language: params?.language, epoch: params?.epoch });
      return httpJson<MintLanguageStatsResponse>(`/api/terminus/mint/language-stats${q}`);
    },
    async failures(params?: { epoch?: string; task_category?: string }) {
      const q = buildQuery({ epoch: params?.epoch, task_category: params?.task_category });
      return httpJson<MintFailuresResponse>(`/api/terminus/mint/failures${q}`);
    },
    async contextProfiles(models?: string[]) {
      const q = buildQuery({ models: models?.join(',') });
      return httpJson<MintContextProfilesResponse>(`/api/terminus/mint/context-profiles${q}`);
    },
    async activity(range?: '30d' | '90d' | 'all') {
      return httpJson<MintActivityResponse>(`/api/terminus/mint/activity${buildQuery({ range })}`);
    },
    async categorySummary(category: MintCategoryKey, epoch?: string) {
      return httpJson<MintCategorySummaryResponse>(
        `/api/terminus/mint/category/${encodeURIComponent(category)}/summary${buildQuery({ epoch })}`);
    },
    async categoryDimensions(category: MintCategoryKey, epoch?: string) {
      return httpJson<MintCategoryDimensionsResponse>(
        `/api/terminus/mint/category/${encodeURIComponent(category)}/dimensions${buildQuery({ epoch })}`);
    },
    async categoryMatrix(category: MintCategoryKey, epoch?: string) {
      return httpJson<MintCategoryMatrixResponse>(
        `/api/terminus/mint/category/${encodeURIComponent(category)}/matrix${buildQuery({ epoch })}`);
    },
    async categoryBox(category: MintCategoryKey, metric?: string, epoch?: string) {
      return httpJson<MintCategoryBoxResponse>(
        `/api/terminus/mint/category/${encodeURIComponent(category)}/box${buildQuery({ metric, epoch })}`);
    },
    async categoryFailures(category: MintCategoryKey, epoch?: string) {
      return httpJson<MintCategoryFailuresResponse>(
        `/api/terminus/mint/category/${encodeURIComponent(category)}/failures${buildQuery({ epoch })}`);
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
//
// S127 TGUI2 (Part B / DATA-01+02): http-DEFAULT + runtime-selectable. Precedence, highest first:
//   1. Build-time `import.meta.env.VITE_AGG_MODE` === 'http' | 'mock' — an explicit build wins.
//   2. Runtime `window.__AGG_MODE__` === 'http' | 'mock' — the server MAY inject this into
//      index.html to force a mode on a single embedded bundle without a rebuild.
//   3. Runtime opt-IN to mock only: `?mock` in the URL, or `localStorage['constellation.aggMode']
//      === 'mock'` — for offline/dev against a bundle that would otherwise go to the real backend.
//   4. Any other browser context → 'http' (the SPA is served same-origin by the real terminus
//      binary in production, so the real backend is right there — INVERTED from the old default).
//   5. No `window` at all (unit tests / SSR) → 'mock', so tests stay offline + deterministic.
// A mock bundle can therefore never ship silently: mock is only ever reached by an explicit
// build flag, an explicit server injection, or an explicit per-session opt-in.
export function resolveMode(): 'mock' | 'http' {
  const buildMode = (import.meta as unknown as { env?: Record<string, string | undefined> }).env
    ?.VITE_AGG_MODE;
  if (buildMode === 'http') return 'http';
  if (buildMode === 'mock') return 'mock';

  // Non-browser (vitest node env, SSR): offline mock, deterministic.
  if (typeof window === 'undefined') return 'mock';

  const injected = (window as unknown as { __AGG_MODE__?: string }).__AGG_MODE__;
  if (injected === 'http') return 'http';
  if (injected === 'mock') return 'mock';

  try {
    if (new URLSearchParams(window.location.search).has('mock')) return 'mock';
    if (window.localStorage.getItem('constellation.aggMode') === 'mock') return 'mock';
  } catch {
    // URL/storage unavailable (private mode etc.) — fall through to the real-backend default.
  }

  // Served same-origin by the real backend → talk to it. Unreachable panels fail-open to a
  // clean empty/loading state (each adapter method degrades), never to fake data.
  return 'http';
}

let cached: AggregationClient | null = null;

/** The single aggregation client instance for the app. Mode chosen once, at first use. */
export function getAggregationClient(): AggregationClient {
  if (!cached) {
    cached = resolveMode() === 'http' ? httpAdapter : mockAdapter;
    if (typeof console !== 'undefined' && resolveMode() === 'mock' && typeof window !== 'undefined') {
      // Visible-in-devtools signal that this session is on fixtures, so a mock bundle is never
      // mistaken for real data (the S127 "smoke-and-mirrors" trap). Harmless in production
      // (production defaults to http, so this never fires there).
      console.warn('[constellation] aggregation adapter = MOCK (fixtures) — not live backend data.');
    }
  }
  return cached;
}

// Exported for tests / explicit overrides only — app code should use getAggregationClient().
export { mockAdapter, httpAdapter };
