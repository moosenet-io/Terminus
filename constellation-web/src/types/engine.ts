// TRIAGE-06: TypeScript types for triage mode engine state.

export interface TriageStepOutcome {
  step: string;
  passed: boolean;
  failure_reason: string | null;
  elapsed_secs: number;
}

export interface TriageState {
  current_task_id: string;
  current_task_title: string;
  current_step: number;     // 1-5
  current_step_name: string;
  held_count: number;
  step_outcomes: TriageStepOutcome[];
  started_at: string;
}

export interface TriageEngineState {
  active: boolean;
  triage: TriageState | null;
}

export interface HeldTask {
  task_id: string;
  plane_issue_id: string;
  title: string;
  fail_count: number;
  last_failure_mode: string | null;
  held_at: string;
  cooldown_secs: number;
}

export interface HoldState {
  held_tasks: HeldTask[];
  dependency_gate_blocked: boolean;
  blocking_tasks: string[];
}

// The 5 escalation steps with their display metadata.
export const ESCALATION_STEPS = [
  { step: 1, name: 'Local + Context',  short: 'Local' },
  { step: 2, name: 'Local MAX VRAM',  short: 'MAX VRAM' },
  { step: 3, name: 'Cloud Standard',  short: 'Cloud Std' },
  { step: 4, name: 'Cloud Deep',      short: 'Cloud Deep' },
  { step: 5, name: 'Human Review',    short: 'Human' },
] as const;
