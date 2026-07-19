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
      const entries = limit != null ? MOCK_ACTIVITY_ENTRIES.slice(-limit) : MOCK_ACTIVITY_ENTRIES;
      return delay({ entries, available: true });
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
