// CONST-04: Horizontal status strip, adapted from harmony-web. Instead of a single
// Harmony-specific StatusResponse, this renders one cell per system health entry returned by
// the aggregation client's health.list() — generic across Harmony/Chord/Lumina/Terminus.
import type { HealthStatus } from '../lib/aggregationClient';

interface Props {
  health: HealthStatus[];
  loading: boolean;
}

const LABELS: Record<string, string> = {
  harmony: 'Harmony',
  chord: 'Chord',
  lumina: 'Lumina',
  terminus: 'Terminus',
};

export function StatusStrip({ health, loading }: Props) {
  const cells = health.map(h => ({
    label: LABELS[h.system] ?? h.system,
    value: loading ? '…' : h.available ? 'ok' : 'down',
    color: h.available ? 'var(--status-success)' : 'var(--status-error)',
  }));

  return (
    <div style={{
      display: 'flex',
      borderBottom: '1px solid var(--border-subtle)',
      flexShrink: 0,
      background: 'rgba(0,0,0,0.2)',
    }}>
      {cells.length === 0 && (
        <div style={{ padding: 'var(--space-1) var(--space-3)', color: 'var(--text-tertiary)', fontSize: 'var(--text-xs)' }}>
          No systems reporting
        </div>
      )}
      {cells.map((cell, i) => (
        <div key={i} style={{
          flex: 1,
          padding: 'var(--space-1) var(--space-3)',
          borderRight: i < cells.length - 1 ? '1px solid var(--border-subtle)' : 'none',
        }}>
          <div style={{
            fontSize: 'var(--text-xs)',
            color: 'var(--text-tertiary)',
            textTransform: 'uppercase',
            letterSpacing: '0.05em',
            marginBottom: 2,
          }}>
            {cell.label}
          </div>
          <div style={{
            fontSize: 'var(--text-metric)',
            fontWeight: 500,
            color: cell.color,
            fontFamily: 'var(--font-mono)',
            lineHeight: 1.1,
          }}>
            {cell.value}
          </div>
        </div>
      ))}
    </div>
  );
}
