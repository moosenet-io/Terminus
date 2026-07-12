// WIRE-06: Hero card showing local-inference savings for the period
import type { SavingsData } from '../../hooks/useChordAnalytics';

interface Props { data: SavingsData | null; }

export function SavingsHero({ data }: Props) {
  if (!data) {
    return (
      <div className="h-card" style={{ padding: 24, textAlign: 'center' }}>
        <div style={{ color: 'var(--text-tertiary)', fontSize: 13 }}>
          No savings data yet. Route inference through Chord's proxy to start tracking.
        </div>
      </div>
    );
  }
  return (
    <div className="h-card" style={{ padding: 24, textAlign: 'center' }}>
      <div style={{ fontSize: 11, color: 'var(--text-tertiary)', textTransform: 'uppercase', letterSpacing: '0.1em', marginBottom: 4 }}>
        Saved this month by running locally
      </div>
      <div style={{ fontSize: 48, fontWeight: 700, color: '#22c55e', marginBottom: 8 }}>
        ${data.savings_usd.toFixed(2)}
      </div>
      <div style={{ fontSize: 12, color: 'var(--text-secondary)' }}>
        {(data.total_tokens_local / 1e6).toFixed(1)}M tokens local at $0 &nbsp;·&nbsp; {(data.total_tokens_cloud / 1e6).toFixed(1)}M via cloud at ${data.actual_cost_usd.toFixed(2)}
      </div>
    </div>
  );
}
