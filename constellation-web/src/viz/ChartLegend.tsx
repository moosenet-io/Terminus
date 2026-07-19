// CONST-17: ChartLegend — always rendered for >=2 series; hidden for 1 (§4.2). Swatches use
// the SlotAssigner-resolved colors (or an explicit semantic color) so legend order matches
// first-seen slot assignment.
export interface ChartLegendEntry {
  id: string;
  label: string;
  color: string;
  /** Toggle support: legend click can hide/show a series (§4.2 interactions). */
  active?: boolean;
  onToggle?: () => void;
}

interface ChartLegendProps {
  entries: ChartLegendEntry[];
}

export function ChartLegend({ entries }: ChartLegendProps) {
  if (entries.length < 2) return null;
  return (
    <div style={{ display: 'flex', flexWrap: 'wrap', gap: 'var(--space-3)', padding: 'var(--space-2) 0' }}>
      {entries.map(e => (
        <button
          key={e.id}
          type="button"
          onClick={e.onToggle}
          disabled={!e.onToggle}
          style={{
            display: 'inline-flex',
            alignItems: 'center',
            gap: 6,
            background: 'none',
            border: 'none',
            padding: 0,
            cursor: e.onToggle ? 'pointer' : 'default',
            opacity: e.active === false ? 0.4 : 1,
            fontFamily: 'var(--font-mono)',
            fontSize: 'var(--fs-mono-sm)',
            color: 'var(--text-body)',
          }}
        >
          <span aria-hidden style={{ width: 10, height: 10, borderRadius: 2, background: e.color, flexShrink: 0 }} />
          {e.label}
        </button>
      ))}
    </div>
  );
}
