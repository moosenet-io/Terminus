// WIRE-06: Dual line chart — actual (local) cost vs imputed cloud cost.
// CONST-17 re-skin: raw hex fills removed; local/cloud cost is a semantic pair (§2.4
// cost-free/cost-paid), not nominal identity, so it wears the semantic tokens rather than a
// categorical slot. Solid hairline grid + brand tick/tooltip chrome from the viz theme.
import { LineChart, Line, XAxis, YAxis, CartesianGrid, Tooltip, ResponsiveContainer, Legend } from '../../viz/recharts';
import type { CostData } from '../../hooks/useChordAnalytics';
import { rechartsGridProps, rechartsTickStyle, rechartsTooltipStyle } from '../../viz/theme';
import { SEMANTIC_SERIES_HEX } from '../../viz/palette';

interface Props { data: CostData[]; }

export function CostChart({ data }: Props) {
  if (data.length === 0) {
    return (
      <div style={{ padding: 16, textAlign: 'center', color: 'var(--text-tertiary)' }}>
        No cost data yet
      </div>
    );
  }
  const tick = rechartsTickStyle();
  return (
    <ResponsiveContainer width="100%" height={200}>
      <LineChart data={data}>
        <CartesianGrid {...rechartsGridProps()} />
        <XAxis dataKey="date" tick={tick} />
        <YAxis tick={tick} />
        <Tooltip contentStyle={rechartsTooltipStyle()} />
        <Legend wrapperStyle={{ fontSize: 11 }} />
        <Line type="monotone" dataKey="actual_cost" name="Actual Cost ($)" stroke={SEMANTIC_SERIES_HEX['cost-free']} strokeWidth={2} dot={false} />
        <Line type="monotone" dataKey="imputed_cost" name="Cloud Would Cost ($)" stroke={SEMANTIC_SERIES_HEX['cost-paid']} strokeWidth={2} strokeDasharray="5 5" dot={false} />
      </LineChart>
    </ResponsiveContainer>
  );
}
