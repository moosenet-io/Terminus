// WIRE-05: Soma Inference page — full Chord management console
import { useState, useEffect } from 'react';
import { getAggregationClient } from '../lib/aggregationClient';
import { useChordHealth } from '../hooks/useChordHealth';
import { ProviderHealthCard } from '../components/inference/ProviderHealthCard';
import { VRAMGauge } from '../components/inference/VRAMGauge';
import { ModelInventory } from '../components/inference/ModelInventory';
import { StorageManager } from '../components/inference/StorageManager';
import { LifecycleControls } from '../components/inference/LifecycleControls';
import { ModelDownload } from '../components/inference/ModelDownload';
import { EngineControls } from '../components/EngineControls';

function ModelAliases() {
  const [aliases, setAliases] = useState<Record<string, string> | null>(null);

  useEffect(() => {
    getAggregationClient()
      .request<Record<string, string>>('chord', '/models/aliases')
      .then((d) => setAliases(typeof d === 'object' && d !== null && !Array.isArray(d) ? d : {}))
      .catch(() => setAliases({}));
  }, []);

  const entries = aliases ? Object.entries(aliases) : null;

  return (
    <div className="h-card" style={{ padding: 12 }}>
      <div style={{ fontWeight: 600, marginBottom: 8 }}>Model Aliases</div>
      {entries === null && (
        <div style={{ color: 'var(--text-tertiary)', fontSize: 12 }}>Loading…</div>
      )}
      {entries !== null && entries.length === 0 && (
        <div style={{ color: 'var(--text-tertiary)', fontSize: 12 }}>No aliases configured.</div>
      )}
      {entries !== null && entries.length > 0 && (
        <table style={{ width: '100%', borderCollapse: 'collapse', fontSize: 12 }}>
          <tbody>
            {entries.map(([alias, target]) => (
              <tr key={alias} style={{ borderBottom: '1px solid var(--border)' }}>
                <td style={{ padding: '4px 8px 4px 0', color: 'var(--text-primary)', fontWeight: 500, whiteSpace: 'nowrap' }}>
                  {alias}
                </td>
                <td style={{ padding: '4px 0 4px 4px', color: 'var(--text-tertiary)' }}>→</td>
                <td style={{ padding: '4px 0 4px 8px', color: 'var(--accent)', wordBreak: 'break-all' }}>
                  {target}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </div>
  );
}

export function Inference() {
  const { health, models, storage, loading, offline } = useChordHealth();

  if (!loading && offline) {
    return (
      <div style={{ padding: 24 }}>
        <div className="h-card" style={{ padding: 16, border: '2px solid var(--status-error)', textAlign: 'center' }}>
          <div style={{ color: 'var(--status-error)', fontWeight: 600, marginBottom: 4 }}>Chord Offline</div>
          <div style={{ color: 'var(--text-tertiary)', fontSize: 12 }}>Inference management unavailable. Check the Chord service on the GPU host.</div>
        </div>
      </div>
    );
  }

  return (
    <div style={{ padding: 16, display: 'flex', flexDirection: 'column', gap: 12, overflowY: 'auto', height: '100%' }}>
      <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between' }}>
        <h2 style={{ fontSize: 16, fontWeight: 600, margin: 0 }}>Inference Management</h2>
        <span style={{ fontSize: 11, color: 'var(--text-tertiary)' }}>
          {health ? `Updated ${new Date(health.timestamp).toLocaleTimeString()}` : 'Loading...'}
        </span>
      </div>

      {/* Provider Health row */}
      <div style={{ display: 'grid', gridTemplateColumns: 'repeat(3, 1fr)', gap: 12 }}>
        {health?.engines.map(e => (
          <ProviderHealthCard key={e.name} endpoint={e} />
        )) ?? [1, 2, 3].map(i => (
          <div key={i} className="h-skeleton" style={{ height: 80, borderRadius: 8 }} />
        ))}
      </div>

      {/* VRAM */}
      {health?.vram && <VRAMGauge vram={health.vram} />}

      {/* Engine Controls */}
      <EngineControls />

      {/* Model Inventory */}
      <ModelInventory models={models} />

      {/* Storage */}
      <StorageManager locations={storage} />

      {/* Model Aliases */}
      <ModelAliases />

      {/* Download + Lifecycle */}
      <div style={{ display: 'grid', gridTemplateColumns: '1fr 1fr', gap: 12 }}>
        <ModelDownload />
        <LifecycleControls />
      </div>
    </div>
  );
}
