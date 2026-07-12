// PROV-05: Provider Intelligence analytics table.
// Renders generated provider profiles fetched from /api/providers/profiles.
import { useState, useEffect, useCallback } from 'react';
import { getAggregationClient } from '../lib/aggregationClient';

interface ComplexityRates {
  simple: number;
  moderate: number;
  complex: number;
}

interface GeneratedProfile {
  provider: string;
  model: string;
  total_tasks: number;
  success_rate: number;
  rate_by_complexity: ComplexityRates;
  top_failure_reasons: [string, number][];
  max_effective_file_count: number;
  is_bootstrap: boolean;
}

interface ProfilesResponse {
  profiles: Record<string, GeneratedProfile>;
  total_outcomes: number;
  window_days: number;
}

// Color-code a success rate: green > 60%, amber 30-60%, red < 30%.
function rateColor(rate: number): string {
  if (rate >= 0.6) return 'var(--h-green, #3fb950)';
  if (rate >= 0.3) return 'var(--h-amber, #d29922)';
  return 'var(--h-red, #f85149)';
}

function pct(v: number): string {
  return `${Math.round(v * 100)}%`;
}

export function ProviderAnalytics() {
  const [data, setData] = useState<ProfilesResponse | null>(null);
  const [loading, setLoading] = useState(true);

  const load = useCallback(() => {
    setLoading(true);
    getAggregationClient()
      .request<ProfilesResponse | null>('chord', '/providers/profiles')
      .then((d) => {
        setData(d);
        setLoading(false);
      })
      .catch(() => setLoading(false));
  }, []);

  useEffect(() => {
    load();
  }, [load]);

  if (loading) return <div className="h-skeleton" style={{ height: 200 }} />;

  const profiles = data ? Object.values(data.profiles) : [];

  return (
    <div className="h-card" style={{ padding: 16 }}>
      <div
        style={{
          display: 'flex',
          alignItems: 'center',
          justifyContent: 'space-between',
          marginBottom: 12,
        }}
      >
        <div style={{ fontWeight: 600 }}>
          Provider Intelligence
          {data && (
            <span style={{ color: 'var(--text-tertiary)', fontWeight: 400, marginLeft: 8, fontSize: 12 }}>
              {data.total_outcomes} outcomes · {data.window_days}d window
            </span>
          )}
        </div>
        <button className="h-btn" onClick={load} style={{ fontSize: 12 }}>
          Refresh
        </button>
      </div>

      {profiles.length === 0 && (
        <div style={{ color: 'var(--text-tertiary)', fontSize: 13, padding: '12px 0' }}>
          No provider profiles yet — outcomes are still being collected.
        </div>
      )}

      {profiles.length > 0 && (
        <table style={{ width: '100%', borderCollapse: 'collapse', fontSize: 12 }}>
          <thead>
            <tr style={{ textAlign: 'left', color: 'var(--text-tertiary)' }}>
              <th style={{ padding: '6px 8px' }}>Provider</th>
              <th style={{ padding: '6px 8px' }}>Tasks</th>
              <th style={{ padding: '6px 8px' }}>Success</th>
              <th style={{ padding: '6px 8px' }}>Simple</th>
              <th style={{ padding: '6px 8px' }}>Moderate</th>
              <th style={{ padding: '6px 8px' }}>Complex</th>
              <th style={{ padding: '6px 8px' }}>Max files</th>
              <th style={{ padding: '6px 8px' }}>Top failure</th>
            </tr>
          </thead>
          <tbody>
            {profiles.map((p) => {
              if (p.is_bootstrap) {
                return (
                  <tr key={p.provider} style={{ borderTop: '1px solid var(--border)' }}>
                    <td style={{ padding: '6px 8px', fontWeight: 500 }}>{p.provider}</td>
                    <td style={{ padding: '6px 8px' }}>{p.total_tasks}</td>
                    <td colSpan={6} style={{ padding: '6px 8px', color: 'var(--text-tertiary)' }}>
                      insufficient data
                    </td>
                  </tr>
                );
              }
              const top =
                p.top_failure_reasons && p.top_failure_reasons.length > 0
                  ? `${p.top_failure_reasons[0][0]} (${pct(p.top_failure_reasons[0][1])})`
                  : '—';
              return (
                <tr key={p.provider} style={{ borderTop: '1px solid var(--border)' }}>
                  <td style={{ padding: '6px 8px', fontWeight: 500 }}>{p.provider}</td>
                  <td style={{ padding: '6px 8px' }}>{p.total_tasks}</td>
                  <td style={{ padding: '6px 8px', color: rateColor(p.success_rate), fontWeight: 600 }}>
                    {pct(p.success_rate)}
                  </td>
                  <td style={{ padding: '6px 8px', color: rateColor(p.rate_by_complexity.simple) }}>
                    {pct(p.rate_by_complexity.simple)}
                  </td>
                  <td style={{ padding: '6px 8px', color: rateColor(p.rate_by_complexity.moderate) }}>
                    {pct(p.rate_by_complexity.moderate)}
                  </td>
                  <td style={{ padding: '6px 8px', color: rateColor(p.rate_by_complexity.complex) }}>
                    {pct(p.rate_by_complexity.complex)}
                  </td>
                  <td style={{ padding: '6px 8px' }}>
                    {p.max_effective_file_count > 0 ? p.max_effective_file_count : '—'}
                  </td>
                  <td style={{ padding: '6px 8px', color: 'var(--text-tertiary)' }}>{top}</td>
                </tr>
              );
            })}
          </tbody>
        </table>
      )}
    </div>
  );
}
