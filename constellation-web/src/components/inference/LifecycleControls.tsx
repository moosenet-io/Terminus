// WIRE-05: Lifecycle controls with restore defaults confirmation
import { useState } from 'react';

interface Props { onRestore?: () => void; }

export function LifecycleControls({ onRestore }: Props) {
  const [showConfirm, setShowConfirm] = useState(false);
  return (
    <div className="h-card" style={{ padding: 12 }}>
      <div style={{ fontWeight: 600, marginBottom: 8 }}>Lifecycle Controls</div>
      {!showConfirm ? (
        <button
          onClick={() => setShowConfirm(true)}
          style={{ padding: '6px 12px', background: 'rgba(59,130,246,0.1)', border: '1px solid rgba(59,130,246,0.3)', borderRadius: 4, color: '#3b82f6', cursor: 'pointer', fontSize: 12 }}
        >
          Restore Defaults
        </button>
      ) : (
        <div>
          <div style={{ fontSize: 12, marginBottom: 8 }}>Restore default model loadout? This will unload non-default models.</div>
          <div style={{ display: 'flex', gap: 8 }}>
            <button
              onClick={() => { onRestore?.(); setShowConfirm(false); }}
              style={{ padding: '4px 12px', background: '#3b82f6', border: 'none', borderRadius: 4, color: 'white', cursor: 'pointer', fontSize: 12 }}
            >Confirm</button>
            <button
              onClick={() => setShowConfirm(false)}
              style={{ padding: '4px 12px', background: 'var(--bg-surface-raised)', border: '1px solid var(--border-subtle)', borderRadius: 4, color: 'var(--text-primary)', cursor: 'pointer', fontSize: 12 }}
            >Cancel</button>
          </div>
        </div>
      )}
    </div>
  );
}
