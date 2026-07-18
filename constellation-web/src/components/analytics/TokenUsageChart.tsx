// WIRE-06: Stacked bar chart of local vs cloud token usage over time.
// CONST-17 re-skin: raw hex fills removed — local/cloud is the same cost-free/cost-paid
// semantic pair as CostChart (§2.4), solid hairline grid + brand chrome. Ships a
// TableViewToggle twin (§4.4).
import { BarChart, Bar, XAxis, YAxis, CartesianGrid, Tooltip, ResponsiveContainer, Legend } from '../../viz/recharts';
import type { CostData } from '../../hooks/useChordAnalytics';
import { rechartsGridProps, rechartsTickStyle, rechartsTooltipStyle } from '../../viz/theme';
import { SEMANTIC_SERIES_HEX } from '../../viz/palette';
import { TableViewToggle } from '../../viz/TableViewToggle';

interface Props { data: CostData[]; }

export function TokenUsageChart({ data }: Props) {
  if (data.length === 0) {
    return (
      <div style={{ padding: 16, textAlign: 'center', color: 'var(--text-tertiary)' }}>
        No token data yet
      </div>
    );
  }
  const tick = rechartsTickStyle();
  return (
    <TableViewToggle
      columns={[
        { key: 'date', header: 'Date', render: (r: CostData) => r.date },
        { key: 'local', header: 'Local tokens', align: 'right', render: (r: CostData) => String(r.tokens_local) },
        { key: 'cloud', header: 'Cloud tokens', align: 'right', render: (r: CostData) => String(r.tokens_cloud) },
      ]}
      rows={data}
      rowKey={(r, i) => `${r.date}-${i}`}
    >
      <ResponsiveContainer width="100%" height={200}>
        <BarChart data={data}>
          <CartesianGrid {...rechartsGridProps()} />
          <XAxis dataKey="date" tick={tick} />
          <YAxis tick={tick} />
          <Tooltip contentStyle={rechartsTooltipStyle()} />
          <Legend wrapperStyle={{ fontSize: 11 }} />
          <Bar dataKey="tokens_local" name="Local" fill={SEMANTIC_SERIES_HEX['cost-free']} stackId="a" />
          <Bar dataKey="tokens_cloud" name="Cloud" fill={SEMANTIC_SERIES_HEX['cost-paid']} stackId="a" />
        </BarChart>
      </ResponsiveContainer>
    </TableViewToggle>
  );
}
