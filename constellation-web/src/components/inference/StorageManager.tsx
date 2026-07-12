// WIRE-05: Storage location list with disk usage bars
import type { ChordStorageLocation } from '../../types/chord';

interface Props { locations: ChordStorageLocation[]; }

function fmtBytes(bytes: number): string {
  if (bytes >= 1_000_000_000_000) return `${(bytes / 1_000_000_000_000).toFixed(1)}TB`;
  if (bytes >= 1_000_000_000)     return `${Math.round(bytes / 1_000_000_000)}GB`;
  if (bytes >= 1_000_000)         return `${Math.round(bytes / 1_000_000)}MB`;
  return `${Math.round(bytes / 1_000)}KB`;
}

function DiskBar({ used, total, modelBytes, modelCount }: {
  used: number; total: number; modelBytes: number; modelCount: number;
}) {
  const pct = total > 0 ? Math.min(100, Math.round((used / total) * 100)) : 0;
  const barColor = pct >= 90 ? 'var(--status-error)' : pct >= 75 ? 'var(--status-warning)' : 'var(--accent)';

  const modelNote = modelCount > 0
    ? `, ${modelCount} model${modelCount !== 1 ? 's' : ''}, ${fmtBytes(modelBytes)} model data`
    : `, ${modelCount} models`;

  return (
    <div style={{ marginTop: 4 }}>
      <div style={{
        height: 6, borderRadius: 3,
        background: 'var(--bg-tertiary)',
        overflow: 'hidden',
        position: 'relative',
      }}>
        <div style={{
          position: 'absolute', left: 0, top: 0, bottom: 0,
          width: `${pct}%`,
          background: barColor,
          borderRadius: 3,
          transition: 'width 0.3s ease',
        }} />
      </div>
      <div style={{ fontSize: 11, color: 'var(--text-tertiary)', marginTop: 3 }}>
        {pct}% ({fmtBytes(used)} used / {fmtBytes(total)} total{modelNote})
      </div>
    </div>
  );
}

export function StorageManager({ locations }: Props) {
  return (
    <div className="h-card" style={{ padding: 12 }}>
      <div style={{ fontWeight: 600, marginBottom: 10 }}>Storage Locations</div>
      {locations.map(loc => (
        <div key={loc.name} style={{ marginBottom: 14 }}>
          <div style={{ display: 'flex', alignItems: 'center', gap: 8 }}>
            <span style={{ fontSize: 13, fontWeight: 600 }}>{loc.name}</span>
            <span style={{
              fontSize: 10, fontWeight: 700, textTransform: 'uppercase', letterSpacing: '0.05em',
              padding: '1px 6px', borderRadius: 3,
              background: loc.tier === 'hot' ? 'rgba(255,100,50,0.18)' : 'rgba(100,160,255,0.18)',
              color: loc.tier === 'hot' ? 'var(--status-warning)' : 'var(--accent)',
            }}>{loc.tier}</span>
          </div>
          <div style={{ fontSize: 11, color: 'var(--text-tertiary)', marginTop: 1 }}>{loc.path}</div>
          {loc.disk && (
            <DiskBar
              used={loc.disk.used_bytes}
              total={loc.disk.total_bytes}
              modelBytes={loc.model_bytes}
              modelCount={loc.model_count}
            />
          )}
        </div>
      ))}
      {locations.length === 0 && (
        <div style={{ color: 'var(--text-tertiary)', fontSize: 12 }}>No storage locations configured</div>
      )}
    </div>
  );
}
