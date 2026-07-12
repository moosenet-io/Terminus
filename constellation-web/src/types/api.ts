// SGUI-02: TypeScript types for API responses

export interface StatusResponse {
  engine_state: 'STOPPED' | 'EXECUTING' | 'EXECUTING/VECTOR' | 'CONTINUOUS';
  workers: number;
  projects: Project[];
  /** True when projects data is served from cache (TUI state was empty). */
  cached?: boolean;
  /** Seconds since cached data was last refreshed from live Plane data. */
  cached_ago_secs?: number;
  /**
   * LIVE-03: True when there is no live data AND no cache — show skeleton rows,
   * never show 0/0. When this is true, projects will be an empty array.
   */
  loading?: boolean;
  inference_mix: number;
  /** RENAME-04: current operating mode name (local|assisted|hybrid|cloud) */
  mode?: string;
  uptime_seconds: number;
  verify_score: string;
  executor?: ExecutorState;
  vector?: VectorState;
}

export interface Project {
  identifier: string;
  name: string;
  progress_pct: number;
  enrichment_pct: number;
  counts: {
    todo: number;
    in_progress: number;
    done: number;
    enriched: number;
    enrichable: number;
  };
}

export interface ExecutorState {
  active: boolean;
  workers: Worker[];
}

export interface Worker {
  id: string;
  provider: string;
  task_id?: string;
  task_title?: string;
  status: 'working' | 'waiting' | 'idle' | 'stalled' | 'failed';
  elapsed_ms?: number;
  tier?: string;
  model?: string;
}

export interface VectorState {
  active: boolean;
  phase?: string;
  task_id?: string;
  task_title?: string;
  iteration?: number;
  max_iterations?: number;
}

export interface InferenceMixResponse {
  ok: boolean;
  inference_mix: number;
}

// ACARD-02: Agent activity card types
export interface AgentStep {
  name: string;
  state: 'done' | 'active' | 'pending' | 'failed';
  detail: string | null;
}

export interface AgentLoopState {
  phase: string;
  iteration: number;
  max_iterations: number;
  steps: AgentStep[];
}

export interface AgentTask {
  id: string;
  title: string;
}

export interface AgentActivity {
  agent_id: string;
  /** Provider codename: "local", "claude", "codex", "gemini", "llama" */
  provider?: string;
  /** Human-readable label: "claude", "claude-2" for duplicate providers */
  display_name?: string;
  model: string;
  tier: string;
  status: 'active' | 'idle' | 'cooldown';
  elapsed_seconds: number;
  task: AgentTask | null;
  loop_state: AgentLoopState | null;
  active_providers: string[];
}

export interface AgentActivityResponse {
  agents: AgentActivity[];
}

export interface CommandResponse {
  ok: boolean;
  command: string;
}
