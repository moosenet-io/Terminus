// WIRE-05: Model download stub
import { useState } from 'react';

export function ModelDownload() {
  const [model, setModel] = useState('');
  return (
    <div className="h-card" style={{ padding: 12 }}>
      <div style={{ fontWeight: 600, marginBottom: 8 }}>Download Model</div>
      <div style={{ display: 'flex', gap: 8 }}>
        <input
          placeholder="Model name (e.g. qwen3:8b)"
          value={model}
          onChange={e => setModel(e.target.value)}
          style={{ flex: 1, padding: '6px 8px', background: 'var(--bg-surface-raised)', border: '1px solid var(--border-subtle)', borderRadius: 4, color: 'var(--text-primary)', fontSize: 12 }}
        />
        <button
          style={{ padding: '6px 12px', background: 'rgba(34,197,94,0.1)', border: '1px solid rgba(34,197,94,0.3)', borderRadius: 4, color: '#22c55e', cursor: 'pointer', fontSize: 12 }}
        >
          Download
        </button>
      </div>
    </div>
  );
}
