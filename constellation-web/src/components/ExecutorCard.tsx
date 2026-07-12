// SGUI-03: Multi-agent executor card
import { useState, useEffect, useRef } from 'react';
import type { ExecutorSummary } from '../hooks/useExecutorState';
import type { Worker } from '../types/api';

interface Props { summary: ExecutorSummary; }

export function ExecutorCard({ summary }: Props) {
  const [expanded, setExpanded] = useState(true);
  const { workers, activeCount, waitingCount, idleCount } = summary;

  const statusText = workers.length === 0
    ? '0 active, 0 waiting, 0 idle'
    : `${activeCount} active, ${waitingCount} waiting, ${idleCount} idle`;

  return (
    <div className="h-card">
      <div className="h-card-header" onClick={() => setExpanded(e => !e)}>
        <div className="h-flex h-gap-sm">
          <span className={`h-dot ${activeCount > 0 ? 'h-dot-green h-pulse' : 'h-dot-dim'}`} />
          <span style={{ fontWeight: 600, fontSize: 13 }}>Engine</span>
          <span style={{ fontSize: 12, color: 'var(--h-text-dim)' }}>— {statusText}</span>
        </div>
        <span style={{ color: 'var(--h-text-muted)', fontSize: 12, transform: expanded ? 'rotate(180deg)' : 'none', transition: 'transform 0.2s' }}>▼</span>
      </div>

      {expanded && (
        <div className="h-card-body" style={{ padding: workers.length === 0 ? '16px 14px' : 0 }}>
          {workers.length === 0 ? (
            <div style={{ color: 'var(--h-text-muted)', fontSize: 13, textAlign: 'center' }}>
              No agents active
            </div>
          ) : (
            <div>
              {workers.map((worker, i) => (
                <AgentRow key={worker.id || i} worker={worker} />
              ))}
            </div>
          )}
        </div>
      )}
    </div>
  );
}

function AgentRow({ worker }: { worker: Worker }) {
  const [elapsed, setElapsed] = useState(worker.elapsed_ms || 0);
  const startRef = useRef(Date.now() - (worker.elapsed_ms || 0));

  useEffect(() => {
    if (worker.status !== 'working') return;
    startRef.current = Date.now() - (worker.elapsed_ms || 0);
    const interval = setInterval(() => {
      setElapsed(Date.now() - startRef.current);
    }, 1000);
    return () => clearInterval(interval);
  }, [worker.status, worker.elapsed_ms]);

  const dotClass = worker.status === 'working' ? 'h-dot-green h-pulse'
    : worker.status === 'waiting' ? 'h-dot-amber h-pulse'
    : worker.status === 'stalled' ? 'h-dot-red'
    : worker.status === 'failed' ? 'h-dot-red'
    : 'h-dot-dim';

  const statusLabel = worker.status === 'waiting' ? '⌛ waiting for GPU'
    : worker.status === 'working' ? '● working'
    : worker.status === 'stalled' ? '⚠ stalled'
    : worker.status === 'failed' ? '✗ failed'
    : '○ idle';

  const statusColor = worker.status === 'working' ? 'var(--h-green)'
    : worker.status === 'waiting' ? 'var(--h-amber)'
    : worker.status === 'stalled' || worker.status === 'failed' ? 'var(--h-red)'
    : 'var(--h-text-muted)';

  return (
    <div style={{
      display: 'flex',
      alignItems: 'center',
      gap: 10,
      padding: '8px 14px',
      borderBottom: '1px solid var(--h-border)',
    }}>
      <span className={`h-dot ${dotClass}`} style={{ flexShrink: 0 }} />
      <span style={{ width: 70, fontSize: 12, fontWeight: 600, color: 'var(--h-teal)', overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>
        {worker.provider}
      </span>
      <span className="h-truncate h-flex-1" style={{ fontSize: 12, color: 'var(--h-text-dim)' }}>
        {worker.task_id ? `${worker.task_id}` : ''}{worker.task_title ? `: ${worker.task_title}` : '—'}
      </span>
      <span style={{ fontSize: 11, color: statusColor, flexShrink: 0, fontFamily: 'var(--h-font-mono)' }}>
        {statusLabel}
      </span>
      {worker.status !== 'idle' && (
        <span className="h-mono" style={{ fontSize: 11, color: 'var(--h-text-muted)', flexShrink: 0 }}>
          {formatElapsed(elapsed)}
        </span>
      )}
    </div>
  );
}

function formatElapsed(ms: number): string {
  const s = Math.floor(ms / 1000);
  const m = Math.floor(s / 60);
  if (m > 0) return `${m}m${String(s % 60).padStart(2,'0')}s`;
  return `${s}s`;
}
