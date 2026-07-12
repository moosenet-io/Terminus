// WIRE-07: Worker node card for engine diagram
import type { Worker } from '../../types/api';

interface Props {
  worker: Worker;
  isTriage?: boolean;
}

export function WorkerNode({ worker, isTriage }: Props) {
  const isActive = worker.status === 'working';
  const borderColor = isTriage ? '#f59e0b' : isActive ? '#22d3ee' : 'var(--border-subtle)';
  return (
    <div style={{
      border: `2px solid ${borderColor}`,
      borderRadius: 8,
      padding: '8px 10px',
      minWidth: 120,
      background: 'var(--bg-card)',
      boxShadow: isActive ? `0 0 8px ${borderColor}40` : 'none',
    }}>
      <div style={{ fontSize: 10, color: 'var(--text-tertiary)' }}>slot-{worker.id}</div>
      <div style={{ fontWeight: 600, fontSize: 12 }}>{worker.provider}</div>
      {worker.task_id && (
        <div style={{ fontSize: 10, color: 'var(--text-secondary)', maxWidth: 110, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>
          {worker.task_id}
        </div>
      )}
      {!worker.task_id && (
        <div style={{ fontSize: 10, color: 'var(--text-tertiary)' }}>idle</div>
      )}
    </div>
  );
}
