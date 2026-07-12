// VLLM-07: Engine lifecycle control bar for the Soma dashboard.
// Stop and Restart buttons with confirmation modal.
// Uses POST /api/engine/stop and /api/engine/restart from VLLM-06.
import { useState, useCallback } from 'react';
import { getAggregationClient } from '../lib/aggregationClient';

// ── Types ──────────────────────────────────────────────────────────────────

export type EngineLifecycleState = 'executing' | 'stopping' | 'stopped' | 'idle';

interface EngineStatusResponse {
  state: EngineLifecycleState;
  pid: number | null;
  active_count: number;
  uptime_secs: number;
  stop_reason: string | null;
  executor_active: boolean;
}

interface EngineControlsProps {
  /** Current engine state string from /api/status (engine_state field). */
  engineState?: string;
  /** Number of active workers from /api/status */
  activeWorkers?: number;
}

// ── Confirmation modal ─────────────────────────────────────────────────────

interface ConfirmModalProps {
  title: string;
  body: string;
  confirmLabel: string;
  confirmClass: string;
  onConfirm: () => void;
  onCancel: () => void;
}

function ConfirmModal({ title, body, confirmLabel, confirmClass, onConfirm, onCancel }: ConfirmModalProps) {
  return (
    <div style={{
      position: 'fixed', inset: 0, zIndex: 1000,
      background: 'rgba(0,0,0,0.6)',
      display: 'flex', alignItems: 'center', justifyContent: 'center',
    }}>
      <div className="h-card" style={{
        maxWidth: 440, width: '90%', padding: 24,
        border: '1px solid var(--border)',
        background: 'var(--bg-secondary)',
      }}>
        <div style={{ fontSize: 16, fontWeight: 700, marginBottom: 12 }}>{title}</div>
        <div style={{ color: 'var(--text-secondary)', fontSize: 13, marginBottom: 20, lineHeight: 1.5 }}>
          {body}
        </div>
        <div style={{ display: 'flex', gap: 8, justifyContent: 'flex-end' }}>
          <button className="h-btn h-btn-ghost" onClick={onCancel}>Cancel</button>
          <button className={`h-btn ${confirmClass}`} onClick={onConfirm}>{confirmLabel}</button>
        </div>
      </div>
    </div>
  );
}

// ── Engine state dot ───────────────────────────────────────────────────────

function StateIndicator({ state }: { state: EngineLifecycleState }) {
  const configs: Record<EngineLifecycleState, { color: string; label: string; dot: string }> = {
    executing: { color: 'var(--status-success)', label: 'EXECUTING', dot: '●' },
    stopping:  { color: 'var(--status-warning)', label: 'STOPPING',  dot: '◐' },
    stopped:   { color: 'var(--status-error)',   label: 'STOPPED',   dot: '○' },
    idle:      { color: 'var(--text-secondary)', label: 'IDLE',      dot: '○' },
  };
  const cfg = configs[state] ?? configs.idle;
  return (
    <span style={{ color: cfg.color, fontWeight: 700, fontSize: 13, display: 'flex', alignItems: 'center', gap: 6 }}>
      <span style={{ fontSize: 10 }}>{cfg.dot}</span>
      {cfg.label}
    </span>
  );
}

// ── Main component ─────────────────────────────────────────────────────────

export function EngineControls({ engineState, activeWorkers = 0 }: EngineControlsProps) {
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [modal, setModal] = useState<null | 'graceful-stop' | 'hard-stop' | 'restart'>(null);
  const [liveState, setLiveState] = useState<EngineLifecycleState | null>(null);

  // Derive the current lifecycle state from props + live override
  const rawState = engineState?.toLowerCase() ?? 'idle';
  const derivedState: EngineLifecycleState = liveState ?? (
    rawState.includes('executing') ? 'executing'
    : rawState.includes('stopping') ? 'stopping'
    : rawState.includes('stopped') ? 'stopped'
    : 'idle'
  );

  const callApi = useCallback(async (path: string, body: object) => {
    setLoading(true);
    setError(null);
    try {
      const data = await getAggregationClient().request<EngineStatusResponse>('harmony', path, {
        method: 'POST',
        body: JSON.stringify(body),
      });
      setLiveState(data.state ?? null);
      return data;
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Unknown error');
      return null;
    } finally {
      setLoading(false);
    }
  }, []);

  const handleGracefulStop = useCallback(async () => {
    setModal(null);
    setLiveState('stopping');
    await callApi('/engine/stop', { mode: 'graceful', reason: 'operator request' });
  }, [callApi]);

  const handleHardStop = useCallback(async () => {
    setModal(null);
    setLiveState('stopping');
    await callApi('/engine/stop', { mode: 'immediate', reason: 'operator hard stop' });
  }, [callApi]);

  const handleRestart = useCallback(async () => {
    setModal(null);
    setLiveState('stopping');
    await callApi('/engine/restart', { mode: 'graceful', reason: 'operator restart' });
  }, [callApi]);

  const workerText = activeWorkers === 1 ? '1 worker' : `${activeWorkers} workers`;
  const taskText = activeWorkers === 1 ? '1 task' : `${activeWorkers} tasks`;

  return (
    <>
      <div className="h-card" style={{
        padding: '10px 14px',
        display: 'flex',
        alignItems: 'center',
        justifyContent: 'space-between',
        gap: 12,
        borderLeft: `3px solid ${
          derivedState === 'executing' ? 'var(--status-success)' :
          derivedState === 'stopping'  ? 'var(--status-warning)' :
          derivedState === 'stopped'   ? 'var(--status-error)'   :
          'var(--border)'
        }`,
      }}>
        {/* Left: state + info */}
        <div style={{ display: 'flex', alignItems: 'center', gap: 16 }}>
          <StateIndicator state={derivedState} />
          <span style={{ color: 'var(--text-secondary)', fontSize: 12 }}>
            {derivedState === 'executing' && `${workerText} active`}
            {derivedState === 'stopping'  && `Draining ${workerText}…`}
            {derivedState === 'stopped'   && 'Engine stopped'}
            {derivedState === 'idle'      && 'Ready'}
          </span>
          {error && (
            <span style={{ color: 'var(--status-error)', fontSize: 11 }}>⚠ {error}</span>
          )}
        </div>

        {/* Right: context-sensitive buttons */}
        <div style={{ display: 'flex', gap: 8, alignItems: 'center' }}>
          {(derivedState === 'executing') && (
            <>
              <button
                className="h-btn"
                style={{ background: 'var(--status-warning)', color: '#fff', fontSize: 12, padding: '4px 10px' }}
                onClick={() => setModal('graceful-stop')}
                disabled={loading}
              >
                Graceful Stop
              </button>
              <button
                className="h-btn"
                style={{ background: 'var(--status-error)', color: '#fff', fontSize: 12, padding: '4px 10px' }}
                onClick={() => setModal('hard-stop')}
                disabled={loading}
              >
                Hard Stop
              </button>
              <button
                className="h-btn h-btn-ghost"
                style={{ fontSize: 12, padding: '4px 10px' }}
                onClick={() => setModal('restart')}
                disabled={loading}
                title="Restart engine"
              >
                ↺ Restart
              </button>
            </>
          )}
          {derivedState === 'stopping' && (
            <button
              className="h-btn"
              style={{ background: 'var(--status-error)', color: '#fff', fontSize: 12, padding: '4px 10px' }}
              onClick={handleHardStop}
              disabled={loading}
            >
              Force Stop Now
            </button>
          )}
          {(derivedState === 'stopped' || derivedState === 'idle') && (
            <span style={{ color: 'var(--text-secondary)', fontSize: 12 }}>
              Use "build &lt;PROJECT&gt;" to start
            </span>
          )}
          {loading && (
            <span style={{ color: 'var(--text-secondary)', fontSize: 11 }}>…</span>
          )}
        </div>
      </div>

      {/* Confirmation modals */}
      {modal === 'graceful-stop' && (
        <ConfirmModal
          title="Stop Harmony Engine?"
          body={`This will stop the build pipeline. ${workerText} ${activeWorkers === 1 ? 'is' : 'are'} currently active on ${taskText}. Active tasks will complete before stopping.`}
          confirmLabel="Stop Engine"
          confirmClass="h-btn-red"
          onConfirm={handleGracefulStop}
          onCancel={() => setModal(null)}
        />
      )}
      {modal === 'hard-stop' && (
        <ConfirmModal
          title="Hard Stop — Immediate Shutdown"
          body={`This will immediately stop the build engine. ${activeWorkers > 0 ? `${taskText} in progress will be interrupted. In-progress PRs may be left open.` : 'No active tasks.'}`}
          confirmLabel="Hard Stop"
          confirmClass="h-btn-red"
          onConfirm={handleHardStop}
          onCancel={() => setModal(null)}
        />
      )}
      {modal === 'restart' && (
        <ConfirmModal
          title="Restart Harmony Engine?"
          body={`This will gracefully stop and restart the build engine. ${activeWorkers > 0 ? `${taskText} will complete first, then the engine restarts.` : 'The engine will restart immediately.'}`}
          confirmLabel="Restart"
          confirmClass="h-btn-teal"
          onConfirm={handleRestart}
          onCancel={() => setModal(null)}
        />
      )}
    </>
  );
}
