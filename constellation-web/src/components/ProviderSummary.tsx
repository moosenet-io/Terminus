import { useNavigate } from 'react-router-dom';
import { useProviders } from '../hooks/useProviders';

export function ProviderSummary() {
  const navigate = useNavigate();
  const { providers, loading } = useProviders();

  if (loading || !providers) {
    return (
      <div className="h-card" style={{ cursor: 'pointer' }}>
        <div className="h-card-header">
          <span className="h-card-title">Providers</span>
        </div>
        <div style={{ display: 'flex', flexDirection: 'column', gap: 8, padding: '8px 0' }}>
          {[1, 2, 3].map(i => (
            <div key={i} className="h-skeleton" style={{ height: 20, borderRadius: 4 }} />
          ))}
        </div>
      </div>
    );
  }

  const online = providers.filter(p => p.status === 'healthy').length;
  const offline = providers.filter(p => p.status === 'error' || p.status === 'degraded').length;
  const disabled = providers.filter(p => !p.enabled).length;
  const allOffline = online === 0 && providers.length > 0;

  const costToday = providers
    .filter(p => p.type === 'cloud')
    .reduce((sum, p) => sum + (p.cost?.used_usd ?? 0), 0);

  const statusDot = (status: string) => {
    if (status === 'healthy') return 'var(--h-green)';
    if (status === 'unknown') return 'var(--h-yellow)';
    return 'var(--h-red)';
  };

  const badgeParts: string[] = [];
  if (online > 0) badgeParts.push(`${online} online`);
  if (disabled > 0) badgeParts.push(`${disabled} disabled`);
  if (offline > 0) badgeParts.push(`${offline} offline`);
  const badge = badgeParts.join(' · ') || 'No providers';

  return (
    <div
      className={`h-card${allOffline ? ' h-card-warning' : ''}`}
      style={{ cursor: 'pointer' }}
      onClick={() => navigate('/inference')}
      role="button"
      tabIndex={0}
      onKeyDown={e => e.key === 'Enter' && navigate('/inference')}
    >
      <div className="h-card-header">
        <span className="h-card-title">Providers</span>
        <span className="h-badge h-badge-neutral" style={{ fontSize: 11 }}>{badge}</span>
      </div>

      <div style={{ display: 'flex', flexDirection: 'column', gap: 6, margin: '8px 0' }}>
        {providers.map(p => (
          <div key={p.name} style={{ display: 'flex', alignItems: 'center', gap: 8 }}>
            <span style={{
              width: 8,
              height: 8,
              borderRadius: '50%',
              backgroundColor: statusDot(p.status),
              flexShrink: 0,
            }} />
            <span style={{ flex: 1, fontSize: 13, color: 'var(--h-text)' }}>{p.display_name ?? p.name}</span>
            {(p.active_tasks ?? 0) > 0 && (
              <span style={{ fontSize: 11, color: 'var(--h-text-muted)' }}>
                {p.active_tasks} active
              </span>
            )}
          </div>
        ))}
      </div>

      <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', marginTop: 4 }}>
        <span style={{ fontSize: 12, color: 'var(--h-text-muted)' }}>
          Cost today: ${costToday.toFixed(2)}
        </span>
        <span style={{ fontSize: 12, color: 'var(--h-accent)' }}>→ Manage</span>
      </div>
    </div>
  );
}
