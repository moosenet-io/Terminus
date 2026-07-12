// CONST-04: Minimal example panel proving the aggregation-client + module-registry pattern.
// Reads Terminus module config through the (mock-backed, by default) aggregation client.
// Do NOT extend this into the full Terminus config UI here — that's CONST-05..12.
import { useEffect, useState } from 'react';
import { Card, CardTitle } from '../../components/Card';
import { MetricCard } from '../../components/MetricCard';
import { Skeleton, SkeletonList } from '../../components/Skeleton';
import { getAggregationClient } from '../../lib/aggregationClient';
import type { TerminusConfigSummary } from '../../lib/aggregationClient';

export function TerminusPanel() {
  const [summary, setSummary] = useState<TerminusConfigSummary | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);

  useEffect(() => {
    let cancelled = false;
    setLoading(true);
    getAggregationClient()
      .terminus.configSummary()
      .then(d => { if (!cancelled) setSummary(d); })
      .catch(e => { if (!cancelled) setError(e instanceof Error ? e.message : 'Failed to load'); })
      .finally(() => { if (!cancelled) setLoading(false); });
    return () => { cancelled = true; };
  }, []);

  return (
    <div style={{ padding: 'var(--space-5)', display: 'flex', flexDirection: 'column', gap: 'var(--space-4)' }}>
      <CardTitle subtitle="Registered infra modules on the Terminus tool hub">Terminus — Config</CardTitle>

      {error && (
        <Card variant="content">
          <span style={{ color: 'var(--status-error)' }}>{error}</span>
        </Card>
      )}

      {loading && !error && (
        <Card variant="content">
          <SkeletonList rows={4} />
        </Card>
      )}

      {!loading && !error && summary && (
        <>
          <div style={{ display: 'flex', gap: 'var(--space-3)' }}>
            <MetricCard label="Modules" value={String(summary.modules.length)} />
            <MetricCard
              label="Enabled"
              value={String(summary.modules.filter(m => m.enabled).length)}
              valueColor="success"
            />
            <MetricCard label="Workers" value={String(summary.workerCount)} valueColor="accent" />
          </div>

          <Card variant="content">
            <table className="h-table">
              <thead>
                <tr>
                  <th>Module</th>
                  <th>Status</th>
                  <th>Version</th>
                </tr>
              </thead>
              <tbody>
                {summary.modules.map(m => (
                  <tr key={m.name}>
                    <td>{m.name}</td>
                    <td>
                      <span className={`h-badge ${m.enabled ? 'h-badge-green' : 'h-badge-dim'}`}>
                        {m.enabled ? 'enabled' : 'disabled'}
                      </span>
                    </td>
                    <td>{m.version ?? <Skeleton variant="bar" width={40} height={11} />}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </Card>
        </>
      )}
    </div>
  );
}
