// TRIAGE-09: Enrichment quality analytics charts.
// Uses recharts for pass-rate, failure-mode, complexity, and problem-specs.
import { PieChart, Pie, Cell, BarChart, Bar, XAxis, YAxis, Tooltip, ResponsiveContainer, Legend } from 'recharts';
import type { EscalationAnalytics } from '../hooks/useEscalationData';

const TIER_COLORS: Record<string, string> = {
  standard:       '#22c55e', // green
  max_vram:       '#3b82f6', // blue
  cloud_standard: '#eab308', // yellow
  cloud_deep:     '#f97316', // orange
  unresolved:     '#ef4444', // red
};

const FAILURE_COLORS: Record<string, string> = {
  compile:         '#ef4444',
  test:            '#f97316',
  scope_violation: '#eab308',
  review_rejected: '#3b82f6',
  timeout:         '#6b7280',
  unknown:         '#9ca3af',
};

const ENRICHMENT_COLORS: Record<string, string> = {
  sufficient:   '#22c55e',
  insufficient: '#eab308',
  poor:         '#ef4444',
};

interface Props {
  analytics: EscalationAnalytics;
}

export function EnrichmentAnalytics({ analytics }: Props) {
  const tierData = Object.entries(analytics.pass_rate_by_tier).map(([name, value]) => ({
    name: name.replace(/_/g, ' '),
    value: Math.round(value * 10) / 10,
    fill: TIER_COLORS[name] ?? '#6b7280',
  }));

  const failureData = Object.entries(analytics.failure_mode_counts)
    .sort(([, a], [, b]) => Number(b) - Number(a))
    .map(([name, value]) => ({
      name: name.replace(/_/g, ' '),
      count: Number(value),
      fill: FAILURE_COLORS[name] ?? '#6b7280',
    }));

  const complexityData = Object.entries(analytics.complexity_distribution).map(([name, value]) => ({
    name,
    count: value,
  }));

  const enrichmentData = Object.entries(analytics.enrichment_quality).map(([name, value]) => ({
    name,
    count: value,
    fill: ENRICHMENT_COLORS[name] ?? '#6b7280',
  }));

  if (analytics.total_tasks === 0) {
    return (
      <div style={{ padding: 24, textAlign: 'center', color: 'var(--text-tertiary)' }}>
        <p style={{ fontSize: 14 }}>No escalation data yet.</p>
        <p style={{ fontSize: 12 }}>Run the build pipeline to start collecting enrichment quality metrics.</p>
      </div>
    );
  }

  return (
    <div style={{ display: 'flex', flexDirection: 'column', gap: 24 }}>
      {/* Summary */}
      <div style={{ fontSize: 12, color: 'var(--text-tertiary)' }}>
        {analytics.total_tasks} tasks analyzed
      </div>

      {/* Pass rate donut */}
      <div>
        <h4 style={{ margin: '0 0 8px', fontSize: 13, color: 'var(--text-secondary)' }}>Pass Rate by Tier</h4>
        <ResponsiveContainer width="100%" height={200}>
          <PieChart>
            <Pie data={tierData} dataKey="value" nameKey="name" cx="50%" cy="50%" outerRadius={80} label={({ name, value }) => `${name}: ${value}%`}>
              {tierData.map((entry, i) => <Cell key={i} fill={entry.fill} />)}
            </Pie>
            <Tooltip formatter={(v) => `${v}%`} />
          </PieChart>
        </ResponsiveContainer>
      </div>

      {/* Failure mode distribution */}
      {failureData.length > 0 && (
        <div>
          <h4 style={{ margin: '0 0 8px', fontSize: 13, color: 'var(--text-secondary)' }}>Failure Mode Distribution</h4>
          <ResponsiveContainer width="100%" height={150}>
            <BarChart data={failureData} layout="vertical" margin={{ left: 80 }}>
              <XAxis type="number" />
              <YAxis type="category" dataKey="name" width={80} tick={{ fontSize: 11 }} />
              <Tooltip />
              <Bar dataKey="count">
                {failureData.map((entry, i) => <Cell key={i} fill={entry.fill} />)}
              </Bar>
            </BarChart>
          </ResponsiveContainer>
        </div>
      )}

      {/* Complexity distribution */}
      {complexityData.length > 0 && (
        <div>
          <h4 style={{ margin: '0 0 8px', fontSize: 13, color: 'var(--text-secondary)' }}>Complexity Distribution</h4>
          <ResponsiveContainer width="100%" height={120}>
            <BarChart data={complexityData}>
              <XAxis dataKey="name" tick={{ fontSize: 11 }} />
              <YAxis />
              <Tooltip />
              <Bar dataKey="count" fill="#3b82f6" />
            </BarChart>
          </ResponsiveContainer>
        </div>
      )}

      {/* Enrichment quality */}
      {enrichmentData.length > 0 && (
        <div>
          <h4 style={{ margin: '0 0 8px', fontSize: 13, color: 'var(--text-secondary)' }}>Enrichment Quality</h4>
          <ResponsiveContainer width="100%" height={100}>
            <BarChart data={enrichmentData}>
              <XAxis dataKey="name" tick={{ fontSize: 11 }} />
              <YAxis />
              <Tooltip />
              <Bar dataKey="count">
                {enrichmentData.map((entry, i) => <Cell key={i} fill={entry.fill} />)}
              </Bar>
            </BarChart>
          </ResponsiveContainer>
        </div>
      )}

      {/* Problem specs table */}
      {analytics.problem_specs.length > 0 && (
        <div>
          <h4 style={{ margin: '0 0 8px', fontSize: 13, color: 'var(--text-secondary)' }}>Problem Specs (by failure count)</h4>
          <table style={{ width: '100%', borderCollapse: 'collapse', fontSize: 12 }}>
            <thead>
              <tr style={{ borderBottom: '1px solid var(--border-subtle)' }}>
                <th style={{ textAlign: 'left', padding: '4px 8px', color: 'var(--text-tertiary)' }}>Spec</th>
                <th style={{ textAlign: 'right', padding: '4px 8px', color: 'var(--text-tertiary)' }}>Failures</th>
              </tr>
            </thead>
            <tbody>
              {analytics.problem_specs.slice(0, 10).map(([spec, count]) => (
                <tr key={spec} style={{ borderBottom: '1px solid var(--border-subtle)' }}>
                  <td style={{ padding: '4px 8px', fontFamily: 'var(--font-mono)' }}>{spec}</td>
                  <td style={{ padding: '4px 8px', textAlign: 'right', color: count > 0 ? 'var(--status-error)' : 'var(--status-success)' }}>
                    {count}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </div>
  );
}
