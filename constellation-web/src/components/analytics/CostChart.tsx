// WIRE-06: Dual line chart — actual (local) cost vs imputed cloud cost.
// CONST-17 re-skin: raw hex fills removed; local/cloud cost is a semantic pair (§2.4
// cost-free/cost-paid), not nominal identity, so it wears the semantic tokens rather than a
// categorical slot. Solid hairline grid + brand tick/tooltip chrome from the viz theme.
// Both lines are solid (color + the legend/tooltip series names carry the distinction — no
// strokeDasharray anywhere, review fix: a dashed data line reads as a residual of the
// dashed-gridline anti-pattern this item retires).
//
// Table-view twin (§4.4): this component is already embedded inside its own `.h-card` in
// Analytics.tsx (title + h-card-body), so it does NOT wrap itself in a second ChartCard
// (that would double the card chrome). Instead the toggle row renders in its own row ABOVE
// the fixed-height chart box — r2 review fix: the toggle-row must never sit INSIDE the
// fixed-height box (it used to, in TableViewToggle v1, and its own height clipped/overflowed
// the chart's axis band). See viz/TableViewToggle.tsx.
import { LineChart, Line, XAxis, YAxis, CartesianGrid, Tooltip, ResponsiveContainer, Legend } from '../../viz/recharts';
import type { CostData } from '../../hooks/useChordAnalytics';
import { rechartsGridProps, rechartsTickStyle, rechartsTooltipStyle } from '../../viz/theme';
import { SEMANTIC_SERIES_HEX } from '../../viz/palette';
import { TableView, TableViewControls, useTableView } from '../../viz/TableViewToggle';

interface Props { data: CostData[]; }

const CHART_HEIGHT = 200;

export function CostChart({ data }: Props) {
  const { view, setView } = useTableView();

  if (data.length === 0) {
    return (
      <div style={{ padding: 16, textAlign: 'center', color: 'var(--text-tertiary)' }}>
        No cost data yet
      </div>
    );
  }
  const tick = rechartsTickStyle();
  return (
    <div>
      <div style={{ display: 'flex', justifyContent: 'flex-end', marginBottom: 'var(--space-1)' }}>
        <TableViewControls view={view} onChange={setView} />
      </div>
      <div style={{ height: CHART_HEIGHT, overflowY: 'auto' }}>
        <TableView
          view={view}
          columns={[
            { key: 'date', header: 'Date', render: (r: CostData) => r.date },
            { key: 'actual', header: 'Actual Cost ($)', align: 'right', render: (r: CostData) => r.actual_cost.toFixed(4) },
            { key: 'imputed', header: 'Cloud Would Cost ($)', align: 'right', render: (r: CostData) => r.imputed_cost.toFixed(4) },
          ]}
          rows={data}
          rowKey={(r, i) => `${r.date}-${i}`}
        >
          <ResponsiveContainer width="100%" height={CHART_HEIGHT}>
            <LineChart data={data}>
              <CartesianGrid {...rechartsGridProps()} />
              <XAxis dataKey="date" tick={tick} />
              <YAxis tick={tick} />
              <Tooltip contentStyle={rechartsTooltipStyle()} />
              <Legend wrapperStyle={{ fontSize: 11 }} />
              <Line type="monotone" dataKey="actual_cost" name="Actual Cost ($)" stroke={SEMANTIC_SERIES_HEX['cost-free']} strokeWidth={2} dot={false} />
              <Line type="monotone" dataKey="imputed_cost" name="Cloud Would Cost ($)" stroke={SEMANTIC_SERIES_HEX['cost-paid']} strokeWidth={2} dot={false} />
            </LineChart>
          </ResponsiveContainer>
        </TableView>
      </div>
    </div>
  );
}
