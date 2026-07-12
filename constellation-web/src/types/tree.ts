// GROW-05: TypeScript interfaces for the /api/tree/{project} response.
// LIVE-04: Added elapsed_secs, held, spec_title fields.

export interface TreeStage {
  name: string;
  status: 'pending' | 'active' | 'done';
}

export interface TreeItem {
  id: number;
  title: string;
  phase: string;
  iteration: number;
  worker_slot: number | null;
  provider: string | null;
  stages: TreeStage[];
  /** Seconds the active worker has been on this task (0 when idle). */
  elapsed_secs: number;
  /** True when this task is on the hold/triage list. */
  held: boolean;
}

export interface TreeSpec {
  prefix: string;
  /** Human-readable spec description (text after the colon in the spec title). */
  spec_title: string;
  items: TreeItem[];
}

export interface TreeResponse {
  project: string;
  specs: TreeSpec[];
  stale: boolean;
}
