// WIRE-07: Engine node card for diagram — shows one inference backend
interface Props {
  name: string;
  model?: string;
  status?: string;
  throughput?: number;
}

export function EngineNode({ name, model, status, throughput }: Props) {
  const dot = status === 'online' ? '#22c55e' : status === 'degraded' ? '#eab308' : '#ef4444';
  return (
    <div style={{
      border: '2px solid var(--border-subtle)',
      borderRadius: 8,
      padding: '8px 10px',
      minWidth: 140,
      background: 'var(--bg-card)',
    }}>
      <div style={{ display: 'flex', alignItems: 'center', gap: 6, marginBottom: 4 }}>
        <div style={{ width: 8, height: 8, borderRadius: '50%', background: dot, flexShrink: 0 }} />
        <span style={{ fontWeight: 600, fontSize: 12 }}>{name}</span>
      </div>
      {model && (
        <div style={{ fontSize: 10, color: 'var(--text-secondary)', maxWidth: 130, overflow: 'hidden', textOverflow: 'ellipsis' }}>
          {model}
        </div>
      )}
      {throughput !== undefined && (
        <div style={{ fontSize: 10, color: 'var(--text-tertiary)' }}>{throughput} t/s</div>
      )}
    </div>
  );
}
