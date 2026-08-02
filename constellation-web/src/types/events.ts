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
  | 'enrichment_done'
  // MACT-08: the Muse activity change-signal (`Terminus/src/constellation/ws.rs`'s
  // `activity_tick_message`). Carries only `{type, ts}` inside the envelope's `event` --
  // never a payload; see `useActivityFeedLive.ts`.
  | 'activity_tick';

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

/**
 * MACT-08: the real on-wire shape every `/ws` frame actually has --
 * `Terminus/src/constellation/ws.rs`'s `envelope_and_mask`/`activity_tick_message` both wrap
 * every frame as `{source, event}` before it ever reaches the browser (see that module's doc
 * comment). `WsEvent` above is the FLAT shape existing consumers (`useHarmonyStatus.ts` et al.)
 * happen to read fields off of directly today -- a pre-existing convention this item does not
 * change or fix, since none of MACT-08's `## FILES` touch that path. `useActivityFeedLive.ts`
 * reads the real envelope shape (this type) rather than perpetuating the flat-access
 * convention for a NEW consumer, so a tick is never missed because it was read off the wrong
 * field.
 */
export interface WsEnvelope {
  source?: string;
  event?: { type?: string; ts?: number; [key: string]: unknown };
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
