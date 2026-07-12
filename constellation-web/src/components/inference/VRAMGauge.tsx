// WIRE-05: Horizontal segmented VRAM bar with allocation breakdown
import type { ChordVRAMState } from '../../types/chord';

interface Props { vram: ChordVRAMState; }

export function VRAMGauge({ vram }: Props) {
  const usedPct = vram.total_mb > 0 ? (vram.used_mb / vram.total_mb) * 100 : 0;
  const color = usedPct < 80 ? '#22c55e' : usedPct < 95 ? '#eab308' : '#ef4444';

  return (
    <div className="h-card" style={{ padding: 12 }}>
      <div style={{ fontWeight: 600, marginBottom: 8 }}>VRAM</div>
      <div style={{ height: 20, background: 'var(--bg-surface-raised)', borderRadius: 4, overflow: 'hidden', marginBottom: 6 }}>
        <div style={{ height: '100%', width: `${usedPct}%`, background: color, transition: 'width 0.5s ease' }} />
      </div>
      <div style={{ fontSize: 12, color: 'var(--text-secondary)' }}>
        {Math.round(vram.used_mb / 1024)}GB / {Math.round(vram.total_mb / 1024)}GB used ({Math.round(usedPct)}%)
      </div>
      {vram.allocations.map(a => (
        <div key={a.model_name} style={{ fontSize: 11, color: 'var(--text-tertiary)', marginTop: 2 }}>
          {a.model_name} ({a.engine}) — {Math.round(a.size_mb / 1024 * 10) / 10}GB
        </div>
      ))}
    </div>
  );
}
