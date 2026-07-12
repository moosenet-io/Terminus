// PROV-05: Providers page — Provider Intelligence analytics.
import { ProviderAnalytics } from '../components/ProviderAnalytics';

export function Providers() {
  return (
    <div style={{ padding: 16, display: 'flex', flexDirection: 'column', gap: 16 }}>
      <div>
        <h1 style={{ margin: 0, fontSize: 20 }}>Providers</h1>
        <div style={{ color: 'var(--text-tertiary)', fontSize: 13, marginTop: 4 }}>
          Data-driven provider performance from collected task outcomes.
        </div>
      </div>
      <ProviderAnalytics />
    </div>
  );
}
