import { useState } from 'react';
import type { Provider, ProviderTier } from '../types/provider';

interface ProviderCardProps {
  provider: Provider;
  onToggle: (name: string, enabled: boolean) => Promise<void>;
}

function tierColor(tier: ProviderTier): string {
  switch (tier) {
    case 'premium': return 'var(--h-purple, #a78bfa)';
    case 'standard': return 'var(--h-teal)';
    default: return 'var(--h-text-muted)';
  }
}

function statusDotClass(provider: Provider): string {
  if (!provider.enabled) return 'h-dot h-dot-gray';
  switch (provider.status) {
    case 'healthy': return 'h-dot h-dot-green';
    case 'degraded': return 'h-dot h-dot-yellow';
    case 'error': return 'h-dot h-dot-red';
    default: return 'h-dot h-dot-gray';
  }
}

function rateLimitColor(pct: number): string {
  if (pct >= 100) return 'var(--h-red, #f87171)';
  if (pct >= 80) return 'var(--h-yellow, #facc15)';
  return 'var(--h-green, #4ade80)';
}

function costColor(pct: number): string {
  if (pct >= 100) return 'var(--h-red, #f87171)';
  if (pct >= 80) return 'var(--h-yellow, #facc15)';
  return 'var(--h-green, #4ade80)';
}

function fmtTokens(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`;
  if (n >= 1_000) return `${(n / 1_000).toFixed(1)}k`;
  return String(n);
}

function Sparkline({ data }: { data: number[] }) {
  if (!data.length) return null;
  const max = Math.max(...data, 1);
  const w = 80;
  const h = 24;
  const pts = data.map((v, i) => {
    const x = (i / (data.length - 1)) * w;
    const y = h - (v / max) * h;
    return `${x},${y}`;
  }).join(' ');

  return (
    <svg width={w} height={h} style={{ display: 'block' }}>
      <polyline
        points={pts}
        fill="none"
        stroke="var(--h-teal)"
        strokeWidth={1.5}
        strokeLinejoin="round"
        strokeLinecap="round"
      />
    </svg>
  );
}

export function ProviderCard({ provider, onToggle }: ProviderCardProps) {
  const [toggling, setToggling] = useState(false);
  const [pendingDisable, setPendingDisable] = useState(false);

  const handleToggle = async () => {
    const next = !provider.enabled;

    if (!next && provider.active_tasks > 0) {
      setPendingDisable(true);
      return;
    }

    setToggling(true);
    try {
      await onToggle(provider.name, next);
    } finally {
      setToggling(false);
    }
  };

  const confirmDisable = async () => {
    setPendingDisable(false);
    setToggling(true);
    try {
      await onToggle(provider.name, false);
    } finally {
      setToggling(false);
    }
  };

  const cardStyle: React.CSSProperties = {
    opacity: provider.enabled ? 1 : 0.5,
    transition: 'opacity 0.15s',
    position: 'relative',
  };

  const { usage, cost } = provider;

  return (
    <div className="h-card" style={cardStyle}>
      <div className="h-card-body">
        {/* Header row */}
        <div style={{ display: 'flex', alignItems: 'center', gap: 8, marginBottom: 10 }}>
          <span className={statusDotClass(provider)} />
          <span style={{ fontWeight: 600, fontSize: 13, flex: 1 }}>{provider.display_name}</span>
          <span style={{
            fontSize: 10,
            padding: '1px 5px',
            borderRadius: 3,
            background: provider.type === 'local' ? 'rgba(20,184,166,0.15)' : 'rgba(167,139,250,0.15)',
            color: provider.type === 'local' ? 'var(--h-teal)' : 'var(--h-purple, #a78bfa)',
          }}>
            {provider.type}
          </span>
          {/* Toggle */}
          <button
            onClick={handleToggle}
            disabled={toggling}
            style={{
              width: 32,
              height: 18,
              borderRadius: 9,
              border: 'none',
              cursor: toggling ? 'wait' : 'pointer',
              background: provider.enabled ? 'var(--h-teal)' : 'var(--h-border, #334155)',
              position: 'relative',
              padding: 0,
              flexShrink: 0,
              transition: 'background 0.15s',
            }}
            title={provider.enabled ? 'Disable provider' : 'Enable provider'}
          >
            <span style={{
              position: 'absolute',
              top: 2,
              left: provider.enabled ? 16 : 2,
              width: 14,
              height: 14,
              borderRadius: '50%',
              background: '#fff',
              transition: 'left 0.15s',
            }} />
          </button>
        </div>

        {/* Inline disable confirmation */}
        {pendingDisable && (
          <div style={{
            marginBottom: 10,
            padding: '6px 8px',
            borderRadius: 4,
            background: 'rgba(248,113,113,0.12)',
            border: '1px solid rgba(248,113,113,0.3)',
            fontSize: 11,
          }}>
            <div style={{ color: 'var(--h-red, #f87171)', marginBottom: 6 }}>
              {provider.active_tasks} active task{provider.active_tasks !== 1 ? 's' : ''} — disable anyway?
            </div>
            <div style={{ display: 'flex', gap: 6 }}>
              <button
                className="h-btn"
                onClick={confirmDisable}
                style={{ fontSize: 10, padding: '2px 8px', background: 'var(--h-red, #f87171)', color: '#fff', border: 'none' }}
              >
                Disable
              </button>
              <button
                className="h-btn"
                onClick={() => setPendingDisable(false)}
                style={{ fontSize: 10, padding: '2px 8px' }}
              >
                Cancel
              </button>
            </div>
          </div>
        )}

        {/* Latency */}
        {provider.latency_ms !== null && provider.enabled && (
          <div style={{ fontSize: 11, color: 'var(--h-text-muted)', marginBottom: 8 }}>
            {provider.latency_ms}ms latency
            {provider.status === 'degraded' && (
              <span style={{ marginLeft: 6, color: 'var(--h-yellow, #facc15)' }}>⚠ degraded</span>
            )}
          </div>
        )}

        {/* Cloud cost */}
        {provider.type === 'cloud' && cost && (
          <div style={{ marginBottom: 10 }}>
            <div style={{
              fontSize: 20,
              fontWeight: 700,
              color: costColor(cost.pct_used),
              fontVariantNumeric: 'tabular-nums',
            }}>
              ${cost.used_usd.toFixed(2)}
              <span style={{ fontSize: 11, fontWeight: 400, color: 'var(--h-text-muted)', marginLeft: 4 }}>
                / ${cost.budget_usd.toFixed(2)}
              </span>
            </div>
            <div style={{ marginTop: 4, height: 4, borderRadius: 2, background: 'var(--h-border, #334155)', overflow: 'hidden' }}>
              <div style={{
                height: '100%',
                width: `${Math.min(cost.pct_used, 100)}%`,
                background: costColor(cost.pct_used),
                transition: 'width 0.3s',
              }} />
            </div>
            <div style={{ fontSize: 10, color: 'var(--h-text-muted)', marginTop: 2 }}>
              {cost.pct_used.toFixed(0)}% budget used
            </div>
          </div>
        )}

        {/* Usage stats */}
        <div style={{ fontSize: 11, color: 'var(--h-text-dim)', marginBottom: 8 }}>
          {fmtTokens(usage.tokens_24h)} tokens · {usage.requests_24h} requests (24h)
        </div>

        {/* Rate limit bar */}
        <div style={{ marginBottom: 8 }}>
          <div style={{ display: 'flex', justifyContent: 'space-between', fontSize: 10, color: 'var(--h-text-muted)', marginBottom: 2 }}>
            <span>Rate limit</span>
            <span style={{ color: rateLimitColor(usage.rate_limit_pct) }}>{usage.rate_limit_pct.toFixed(0)}%</span>
          </div>
          <div style={{ height: 3, borderRadius: 2, background: 'var(--h-border, #334155)', overflow: 'hidden' }}>
            <div style={{
              height: '100%',
              width: `${Math.min(usage.rate_limit_pct, 100)}%`,
              background: rateLimitColor(usage.rate_limit_pct),
              transition: 'width 0.3s',
            }} />
          </div>
        </div>

        {/* Sparkline */}
        {usage.sparkline_24h.length > 1 && (
          <div style={{ marginBottom: 8 }}>
            <Sparkline data={usage.sparkline_24h} />
          </div>
        )}

        {/* Models */}
        {provider.models.length > 0 && (
          <div style={{ display: 'flex', flexWrap: 'wrap', gap: 4, marginTop: 4 }}>
            {provider.models.map(m => (
              <span key={m.id} style={{
                fontSize: 10,
                padding: '1px 5px',
                borderRadius: 3,
                background: 'rgba(255,255,255,0.05)',
                border: '1px solid var(--h-border, #334155)',
                color: tierColor(m.tier),
              }} title={`Context: ${m.context_window.toLocaleString()} tokens`}>
                {m.name}
              </span>
            ))}
          </div>
        )}

        {/* Disabled overlay label */}
        {!provider.enabled && (
          <div style={{
            position: 'absolute',
            top: 8,
            right: 8,
            fontSize: 9,
            padding: '1px 5px',
            borderRadius: 3,
            background: 'rgba(255,255,255,0.08)',
            color: 'var(--h-text-muted)',
            letterSpacing: '0.05em',
            textTransform: 'uppercase',
          }}>
            Disabled
          </div>
        )}
      </div>
    </div>
  );
}
