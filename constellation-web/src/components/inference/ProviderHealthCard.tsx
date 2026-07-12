// WIRE-05: Shows one Chord engine endpoint with status, loaded models, response time
import type { ChordEngineEndpoint } from '../../types/chord';

interface Props { endpoint: ChordEngineEndpoint; }

export function ProviderHealthCard({ endpoint }: Props) {
  const dot = endpoint.status === 'online' ? '🟢' : endpoint.status === 'degraded' ? '🟡' : '🔴';
  return (
    <div className="h-card" style={{ padding: 12 }}>
      <div style={{ display: 'flex', alignItems: 'center', gap: 8, marginBottom: 8 }}>
        <span>{dot}</span>
        <span style={{ fontWeight: 600 }}>{endpoint.name}</span>
        <span style={{ color: 'var(--text-tertiary)', fontSize: 11 }}>{endpoint.response_time_ms}ms</span>
      </div>
      {endpoint.models.map(m => (
        <div key={m.name} style={{ fontSize: 11, color: 'var(--text-secondary)', marginLeft: 20 }}>
          {m.name} ({m.size_vram_mb}MB VRAM) {m.active_requests > 0 ? `— ${m.active_requests} active` : ''}
        </div>
      ))}
      {endpoint.models.length === 0 && (
        <div style={{ fontSize: 11, color: 'var(--text-tertiary)', marginLeft: 20 }}>No models loaded</div>
      )}
    </div>
  );
}
