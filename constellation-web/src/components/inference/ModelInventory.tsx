// WIRE-05: Searchable model table with tier filter and load/unload actions
import { useState } from 'react';
import type { ChordModelRecord } from '../../types/chord';

interface Props {
  models: ChordModelRecord[];
  onLoad?: (name: string) => void;
  onUnload?: (name: string) => void;
}

export function ModelInventory({ models, onLoad, onUnload }: Props) {
  const [search, setSearch] = useState('');
  const [tierFilter, setTierFilter] = useState('all');

  const filtered = models.filter(m =>
    (tierFilter === 'all' || m.storage_tier === tierFilter) &&
    (search === '' || m.name.toLowerCase().includes(search.toLowerCase()))
  );

  return (
    <div className="h-card" style={{ padding: 12 }}>
      <div style={{ fontWeight: 600, marginBottom: 8 }}>Model Inventory ({models.length})</div>
      <div style={{ display: 'flex', gap: 8, marginBottom: 8 }}>
        <input
          placeholder="Search models..."
          value={search}
          onChange={e => setSearch(e.target.value)}
          style={{ flex: 1, padding: '4px 8px', background: 'var(--bg-surface-raised)', border: '1px solid var(--border-subtle)', borderRadius: 4, color: 'var(--text-primary)', fontSize: 12 }}
        />
        <select
          value={tierFilter}
          onChange={e => setTierFilter(e.target.value)}
          style={{ padding: '4px 8px', background: 'var(--bg-surface-raised)', border: '1px solid var(--border-subtle)', borderRadius: 4, color: 'var(--text-primary)', fontSize: 12 }}
        >
          <option value="all">All Tiers</option>
          <option value="hot">Hot</option>
          <option value="warm">Warm</option>
        </select>
      </div>
      <table style={{ width: '100%', fontSize: 11, borderCollapse: 'collapse' }}>
        <thead>
          <tr style={{ color: 'var(--text-tertiary)', borderBottom: '1px solid var(--border-subtle)' }}>
            <th style={{ textAlign: 'left', padding: '4px 0' }}>Model</th>
            <th style={{ textAlign: 'right', padding: '4px 0' }}>Size</th>
            <th style={{ textAlign: 'center', padding: '4px 0' }}>Tier</th>
            <th style={{ textAlign: 'center', padding: '4px 0' }}>Loaded</th>
            <th style={{ textAlign: 'right', padding: '4px 0' }}>Actions</th>
          </tr>
        </thead>
        <tbody>
          {filtered.map(m => (
            <tr key={m.name} style={{ borderBottom: '1px solid var(--border-subtle)' }}>
              <td style={{ padding: '4px 0', maxWidth: 200, overflow: 'hidden', textOverflow: 'ellipsis' }}>{m.name}</td>
              <td style={{ textAlign: 'right', padding: '4px 0', color: 'var(--text-secondary)' }}>{Math.round(m.size_bytes / 1024 / 1024 / 1024)}GB</td>
              <td style={{ textAlign: 'center', padding: '4px 0' }}>
                <span style={{
                  padding: '1px 6px', borderRadius: 3, fontSize: 10,
                  background: m.storage_tier === 'hot' ? 'rgba(34,197,94,0.15)' : 'rgba(59,130,246,0.15)',
                  color: m.storage_tier === 'hot' ? '#22c55e' : '#3b82f6',
                }}>
                  {m.storage_tier}
                </span>
              </td>
              <td style={{ textAlign: 'center', padding: '4px 0' }}>{m.loaded ? '●' : '○'}</td>
              <td style={{ textAlign: 'right', padding: '4px 0' }}>
                {m.loaded ? (
                  <button
                    onClick={() => onUnload?.(m.name)}
                    style={{ fontSize: 10, padding: '1px 6px', cursor: 'pointer', background: 'rgba(239,68,68,0.1)', border: '1px solid rgba(239,68,68,0.3)', borderRadius: 3, color: '#ef4444' }}
                  >Unload</button>
                ) : (
                  <button
                    onClick={() => onLoad?.(m.name)}
                    style={{ fontSize: 10, padding: '1px 6px', cursor: 'pointer', background: 'rgba(34,197,94,0.1)', border: '1px solid rgba(34,197,94,0.3)', borderRadius: 3, color: '#22c55e' }}
                  >Load</button>
                )}
              </td>
            </tr>
          ))}
        </tbody>
      </table>
      {filtered.length === 0 && (
        <div style={{ textAlign: 'center', color: 'var(--text-tertiary)', padding: 16 }}>No models found</div>
      )}
    </div>
  );
}
