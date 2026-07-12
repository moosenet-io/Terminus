// TRIAGE-06: Collapsible panel showing held tasks and their failure info.
import { useState } from 'react';
import type { HeldTask } from '../types/engine';

interface HeldTasksPanelProps {
  heldTasks: HeldTask[];
  blockingTasks: string[];
  defaultExpanded?: boolean;
}

function formatHeldDuration(heldAt: string): string {
  const ms = Date.now() - new Date(heldAt).getTime();
  const secs = Math.floor(ms / 1000);
  if (secs < 60) return `${secs}s`;
  const mins = Math.floor(secs / 60);
  if (mins < 60) return `${mins}m`;
  return `${Math.floor(mins / 60)}h ${mins % 60}m`;
}

const FAILURE_COLORS: Record<string, string> = {
  compile: 'var(--status-error)',
  test: '#f97316',
  scope: '#eab308',
  review_rejected: '#3b82f6',
  timeout: 'var(--text-tertiary)',
  unknown: 'var(--text-tertiary)',
};

export function HeldTasksPanel({ heldTasks, blockingTasks, defaultExpanded = false }: HeldTasksPanelProps) {
  const [expanded, setExpanded] = useState(defaultExpanded);

  if (heldTasks.length === 0) {
    return (
      <div style={{ padding: '6px 12px', fontSize: 12, color: 'var(--text-tertiary)' }}>
        No held tasks
      </div>
    );
  }

  return (
    <div className="h-card" style={{ fontSize: 12 }}>
      <div
        className="h-card-header"
        style={{ cursor: 'pointer', display: 'flex', justifyContent: 'space-between', alignItems: 'center' }}
        onClick={() => setExpanded(e => !e)}
      >
        <span style={{ fontWeight: 600, color: 'var(--status-warning)' }}>
          ⚠ Held Tasks ({heldTasks.length})
        </span>
        <span style={{ color: 'var(--text-tertiary)', fontSize: 10 }}>
          {expanded ? '▲ collapse' : '▼ expand'}
        </span>
      </div>

      {expanded && (
        <div>
          {heldTasks.map(task => {
            const isBlocking = blockingTasks.includes(task.task_id);
            const failureColor = FAILURE_COLORS[task.last_failure_mode ?? 'unknown'] ?? 'var(--text-tertiary)';

            return (
              <div
                key={task.task_id}
                style={{
                  padding: '6px 12px',
                  borderBottom: '1px solid var(--border-subtle)',
                  background: isBlocking ? 'rgba(245,158,11,0.05)' : 'transparent',
                }}
              >
                <div style={{ display: 'flex', alignItems: 'center', gap: 6, marginBottom: 2 }}>
                  {isBlocking && <span title="Blocking forward progress" style={{ fontSize: 10 }}>🚧</span>}
                  <span style={{ fontFamily: 'var(--font-mono)', color: 'var(--accent-primary)', fontWeight: 600 }}>
                    {task.task_id}
                  </span>
                  <span style={{ color: 'var(--text-secondary)', flex: 1, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>
                    {task.title}
                  </span>
                </div>
                <div style={{ display: 'flex', gap: 8, alignItems: 'center' }}>
                  {/* Fail count badges */}
                  {Array.from({ length: Math.min(task.fail_count, 5) }, (_, i) => (
                    <span key={i} style={{
                      width: 8, height: 8, borderRadius: '50%',
                      background: 'var(--status-error)', display: 'inline-block',
                    }} />
                  ))}
                  <span style={{ color: 'var(--text-tertiary)' }}>{task.fail_count} fail{task.fail_count !== 1 ? 's' : ''}</span>
                  {task.last_failure_mode && (
                    <span style={{
                      padding: '1px 5px', borderRadius: 3,
                      background: `${failureColor}20`, color: failureColor,
                      fontSize: 10, fontWeight: 600,
                    }}>
                      {task.last_failure_mode}
                    </span>
                  )}
                  <span style={{ color: 'var(--text-tertiary)', marginLeft: 'auto' }}>
                    held {formatHeldDuration(task.held_at)}
                  </span>
                </div>
              </div>
            );
          })}
        </div>
      )}
    </div>
  );
}
