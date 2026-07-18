// TRIAGE-09: Enrichment quality analytics charts.
// CONST-17 re-skin (§5.1/§10 CONST-17): removed raw-hex per-tier/failure/enrichment fill
// maps (was Tailwind-palette hexes outside the token system); pass-rate donut replaced with
// independent horizontal bars, one per tier, each on its OWN 0-100 scale (review fix: this is
// NOT a stacked part-of-whole bar — each tier's pass rate is an independent percentage, so
// normalizing by the sum of all tiers' rates would misrepresent every tier whenever rates
// don't happen to sum to 100). Tier/failure-mode identities are nominal -> categorical slots
// (SlotAssigner, first-seen order, stable across filtering). Enrichment quality is a
// tri-state health signal -> semantic tokens (§2.4). Solid hairline grid via viz theme (no
// strokeDasharray anywhere). Every chart lives in a ChartCard with a TableViewToggle twin.
import { useMemo } from 'react';
import { BarChart, Bar, XAxis, YAxis, CartesianGrid, Tooltip, ResponsiveContainer, Cell } from '../viz/recharts';
import type { EscalationAnalytics } from '../hooks/useEscalationData';
import { ChartCard } from '../viz/ChartCard';
import { ChartLegend } from '../viz/ChartLegend';
import { TableViewToggle } from '../viz/TableViewToggle';
import { rechartsGridProps, rechartsTickStyle, rechartsTooltipStyle } from '../viz/theme';
import { SlotAssigner, SEMANTIC_SERIES_HEX } from '../viz/palette';

const ENRICHMENT_SEMANTIC: Record<string, keyof typeof SEMANTIC_SERIES_HEX> = {
  sufficient: 'health-success',
  insufficient: 'health-warning',
  poor: 'health-error',
};

interface Props {
  analytics: EscalationAnalytics;
}

export function EnrichmentAnalytics({ analytics }: Props) {
  const tierSlots = useMemo(() => new SlotAssigner(), []);
  const failureSlots = useMemo(() => new SlotAssigner(), []);

  const tierData = Object.entries(analytics.pass_rate_by_tier).map(([name, value]) => ({
    name: name.replace(/_/g, ' '),
    key: name,
    value: Math.round(value * 10) / 10,
    fill: tierSlots.colorFor(name),
  }));

  const failureData = Object.entries(analytics.failure_mode_counts)
    .sort(([, a], [, b]) => Number(b) - Number(a))
    .map(([name, value]) => ({
      name: name.replace(/_/g, ' '),
      key: name,
      count: Number(value),
      fill: failureSlots.colorFor(name),
    }));

  const complexityData = Object.entries(analytics.complexity_distribution).map(([name, value]) => ({
    name,
    count: value,
  }));

  const enrichmentData = Object.entries(analytics.enrichment_quality).map(([name, value]) => ({
    name,
    count: value,
    fill: SEMANTIC_SERIES_HEX[ENRICHMENT_SEMANTIC[name] ?? 'health-info'],
  }));

  const grid = rechartsGridProps();
  const tick = rechartsTickStyle();
  const tooltipStyle = rechartsTooltipStyle();

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
      <div style={{ fontSize: 12, color: 'var(--text-tertiary)' }}>
        {analytics.total_tasks} tasks analyzed
      </div>

      {/* Pass rate — independent horizontal bars, one per tier, each 0-100 (donut
          anti-pattern replaced, audit §1.4; review fix: NOT stacked/part-of-whole) */}
      <ChartCard
        title="Pass Rate by Tier"
        height={tierData.length * 28 + 8}
        empty={tierData.length === 0}
        footer={<ChartLegend entries={tierData.map(t => ({ id: t.key, label: t.name, color: t.fill }))} />}
      >
        <TableViewToggle
          columns={[
            { key: 'name', header: 'Tier', render: (r: typeof tierData[number]) => r.name },
            { key: 'value', header: 'Pass rate', align: 'right', render: (r: typeof tierData[number]) => `${r.value}%` },
          ]}
          rows={tierData}
          rowKey={r => r.key}
        >
          <div role="img" aria-label="Pass rate by tier, one independent 0-100 bar per tier" style={{ display: 'flex', flexDirection: 'column', gap: 6 }}>
            {tierData.map(t => (
              <div key={t.key} style={{ display: 'flex', alignItems: 'center', gap: 'var(--space-2)' }}>
                <span style={{ width: 120, fontSize: 'var(--fs-xs)', color: 'var(--text-muted)', flexShrink: 0, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>
                  {t.name}
                </span>
                <div style={{ flex: 1, height: 16, borderRadius: 'var(--radius-sm)', background: 'var(--space-800)', overflow: 'hidden' }}>
                  <div style={{ width: `${Math.min(100, Math.max(0, t.value))}%`, height: '100%', background: t.fill, minWidth: t.value > 0 ? 2 : 0 }} />
                </div>
                <span style={{ width: 44, textAlign: 'right', fontFamily: 'var(--font-mono)', fontSize: 'var(--fs-xs)', color: 'var(--text-body)', flexShrink: 0 }}>
                  {t.value}%
                </span>
              </div>
            ))}
          </div>
        </TableViewToggle>
      </ChartCard>

      {/* Failure mode distribution */}
      {failureData.length > 0 && (
        <ChartCard title="Failure Mode Distribution" height={150}>
          <TableViewToggle
            columns={[
              { key: 'name', header: 'Failure mode', render: (r: typeof failureData[number]) => r.name },
              { key: 'count', header: 'Count', align: 'right', render: (r: typeof failureData[number]) => String(r.count) },
            ]}
            rows={failureData}
            rowKey={r => r.key}
          >
            <ResponsiveContainer width="100%" height={150}>
              <BarChart data={failureData} layout="vertical" margin={{ left: 80 }}>
                <CartesianGrid {...grid} />
                <XAxis type="number" tick={tick} />
                <YAxis type="category" dataKey="name" width={80} tick={tick} />
                <Tooltip contentStyle={tooltipStyle} />
                <Bar dataKey="count">
                  {failureData.map((entry) => <Cell key={entry.key} fill={entry.fill} />)}
                </Bar>
              </BarChart>
            </ResponsiveContainer>
          </TableViewToggle>
        </ChartCard>
      )}

      {/* Complexity distribution */}
      {complexityData.length > 0 && (
        <ChartCard title="Complexity Distribution" height={120}>
          <TableViewToggle
            columns={[
              { key: 'name', header: 'Complexity', render: (r: typeof complexityData[number]) => r.name },
              { key: 'count', header: 'Count', align: 'right', render: (r: typeof complexityData[number]) => String(r.count) },
            ]}
            rows={complexityData}
            rowKey={r => r.name}
          >
            <ResponsiveContainer width="100%" height={120}>
              <BarChart data={complexityData}>
                <CartesianGrid {...grid} />
                <XAxis dataKey="name" tick={tick} />
                <YAxis tick={tick} />
                <Tooltip contentStyle={tooltipStyle} />
                <Bar dataKey="count" fill="var(--series-1)" />
              </BarChart>
            </ResponsiveContainer>
          </TableViewToggle>
        </ChartCard>
      )}

      {/* Enrichment quality — semantic tri-state (§2.4), not a categorical slot */}
      {enrichmentData.length > 0 && (
        <ChartCard
          title="Enrichment Quality"
          height={100}
          footer={<ChartLegend entries={enrichmentData.map(e => ({ id: e.name, label: e.name, color: e.fill }))} />}
        >
          <TableViewToggle
            columns={[
              { key: 'name', header: 'Quality', render: (r: typeof enrichmentData[number]) => r.name },
              { key: 'count', header: 'Count', align: 'right', render: (r: typeof enrichmentData[number]) => String(r.count) },
            ]}
            rows={enrichmentData}
            rowKey={r => r.name}
          >
            <ResponsiveContainer width="100%" height={100}>
              <BarChart data={enrichmentData}>
                <CartesianGrid {...grid} />
                <XAxis dataKey="name" tick={tick} />
                <YAxis tick={tick} />
                <Tooltip contentStyle={tooltipStyle} />
                <Bar dataKey="count">
                  {enrichmentData.map((entry) => <Cell key={entry.name} fill={entry.fill} />)}
                </Bar>
              </BarChart>
            </ResponsiveContainer>
          </TableViewToggle>
        </ChartCard>
      )}

      {/* Problem specs table */}
      {analytics.problem_specs.length > 0 && (
        <div>
          <h4 style={{ margin: '0 0 8px', fontSize: 13, color: 'var(--text-secondary)' }}>Problem Specs (by failure count)</h4>
          <table className="h-table" style={{ fontSize: 12 }}>
            <thead>
              <tr>
                <th>Spec</th>
                <th style={{ textAlign: 'right' }}>Failures</th>
              </tr>
            </thead>
            <tbody>
              {analytics.problem_specs.slice(0, 10).map(([spec, count]) => (
                <tr key={spec}>
                  <td style={{ fontFamily: 'var(--font-mono)' }}>{spec}</td>
                  <td style={{ textAlign: 'right', color: count > 0 ? 'var(--status-error)' : 'var(--status-success)' }}>
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
