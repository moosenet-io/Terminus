// SGUI-02: WebSocket event types

export type WsEventType =
  | 'state_update'
  | 'state'
  | 'executor_update'
  | 'ralph_update'
  | 'log'
  | 'command_ok'
  | 'command_error'
  | 'enrichment_start'
  | 'enrichment_done';

export interface WsEvent {
  type: WsEventType;
  source?: string;
  data?: Record<string, unknown>;
  text?: string;
  message?: string;
  command?: string;
  project?: string;
  notch?: number;
  mode?: string;
  success?: boolean;
}

export interface RalphLoop {
  id: string;
  task_id: string;
  task_title: string;
  agent: string;
  tier: string;
  phase: 'plan' | 'execute' | 'test' | 'verify' | 'review' | 'pr' | 'done' | 'failed';
  elapsed_ms: number;
  retry_count: number;
}

export type RalphPhase = 'plan' | 'execute' | 'test' | 'verify' | 'review' | 'pr';
