// CONST-28: a minimal, chrome-free line chart for compact per-row/per-card trends (FleetPanel's
// per-system uptime history). Deliberately NOT wrapped in ChartCard — a sparkline lives inline
// inside a bigger card's own layout, it isn't a standalone chart section with its own title/
// filters (ChartCard is for those; see viz/ChartCard.tsx doc). Still goes through the viz kit's
// recharts barrel (src/viz/recharts.ts) — never imports 'recharts' directly (§9 rule).
import { Line, LineChart, ResponsiveContainer } from './recharts';
import { SEMANTIC_SERIES_HEX } from './palette';

export interface SparklinePoint {
  t: number;
  /** Plotted y-value, 0..1 for an uptime ratio series (1 = available). */
  v: number;
}

interface SparklineProps {
  data: SparklinePoint[];
  width?: number | `${number}%`;
  height?: number;
  /** Stroke color — a literal hex from `palette.ts` (recharts' `stroke` is an SVG attribute
   *  and can't resolve CSS custom properties reliably, same reasoning as CostChart.tsx's
   *  `SEMANTIC_SERIES_HEX` usage). Defaults to the 'health-success' semantic (uptime = green,
   *  §2.4 — this IS a health/availability series, not a nominal identity). */
  color?: string;
  /** Rendered when `data` has fewer than 2 points (nothing to draw a line through yet). */
  emptyLabel?: string;
}

export function Sparkline({
  data,
  width = '100%',
  height = 28,
  color = SEMANTIC_SERIES_HEX['health-success'],
  emptyLabel = 'collecting…',
}: SparklineProps) {
  if (data.length < 2) {
    return (
      <div
        style={{
          height,
          display: 'flex',
          alignItems: 'center',
          fontSize: 'var(--fs-xs)',
          color: 'var(--text-faint)',
        }}
      >
        {emptyLabel}
      </div>
    );
  }
  return (
    <ResponsiveContainer width={width} height={height}>
      <LineChart data={data} margin={{ top: 2, right: 2, bottom: 2, left: 2 }}>
        <Line
          type="monotone"
          dataKey="v"
          stroke={color}
          strokeWidth={1.5}
          dot={false}
          isAnimationActive={false}
        />
      </LineChart>
    </ResponsiveContainer>
  );
}
