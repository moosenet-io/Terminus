// CONST-24: the viz kit's Recharts horizontal-stacked wrapper for §7.2 C6 (failure-class bars).
// Recharts stays for this form per §4.1 ("Recharts stays for what it renders well") — routed
// through viz/recharts.ts, never imported directly by panels. Per-model counts by
// failure_class; top-4 classes fleet-wide occupy categorical slots 2/3/4/5 (slot 1/--series-1
// is reserved elsewhere in the module for the single-hue box plot, so C6 starts at slot 2 to
// keep a consistent cross-chart color vocabulary within the Coder section); "Other" folds to
// `--chart-deemphasis`; 'none' is excluded entirely (only failures are plotted); bars <=24px
// with 2px gaps; legend always on (>=2 series is the norm here); segment click scopes C5 to
// that failure class.
import { BarChart, Bar, XAxis, YAxis, CartesianGrid, Tooltip, Legend, ResponsiveContainer } from './recharts';
import { rechartsGridProps, rechartsTickStyle, rechartsTooltipStyle } from './theme';
import { CATEGORICAL_HEX, CHART_CHROME } from './palette';
import type { MintFailureModelCounts } from '../lib/aggregationClient';

interface FailureBarsChartProps {
  classes: string[]; // top-4 + optional 'other', 'none' already excluded upstream
  models: MintFailureModelCounts[];
  height: number;
  onSegmentClick?: (failureClass: string) => void;
}

/** Slots 2..5 (CATEGORICAL_HEX[1..4]); 'other' always wears the deemphasis chrome, never a
 *  categorical slot (it isn't a nominal identity, it's an explicit fold-bucket). */
function colorForClass(cls: string, classes: string[]): string {
  if (cls === 'other') return CHART_CHROME.deemphasis;
  const idx = classes.indexOf(cls);
  return CATEGORICAL_HEX[(idx + 1) % CATEGORICAL_HEX.length] ?? CHART_CHROME.deemphasis;
}

export function FailureBarsChart({ classes, models, height, onSegmentClick }: FailureBarsChartProps) {
  const rows = models.map(m => ({
    model: m.model,
    total_runs: m.total_runs,
    ...Object.fromEntries(classes.map(c => [c, m.counts[c] ?? 0])),
  }));

  return (
    <ResponsiveContainer width="100%" height={height}>
      <BarChart data={rows} layout="vertical" barGap={2} barCategoryGap={8} margin={{ top: 4, right: 16, bottom: 4, left: 8 }}>
        <CartesianGrid {...rechartsGridProps()} horizontal={false} />
        <XAxis type="number" tick={rechartsTickStyle()} allowDecimals={false} />
        <YAxis type="category" dataKey="model" tick={rechartsTickStyle()} width={120} />
        <Tooltip
          contentStyle={rechartsTooltipStyle()}
          formatter={((value: number, name: string, item: { payload?: { total_runs?: number } }) => {
            const totalRuns = item?.payload?.total_runs ?? 0;
            const pct = totalRuns > 0 ? `${Math.round((Number(value) / totalRuns) * 100)}%` : '—';
            return [`${value} (${pct} of runs)`, name];
          }) as never}
        />
        <Legend wrapperStyle={{ fontFamily: 'var(--font-mono)', fontSize: 11 }} />
        {classes.map(cls => (
          <Bar
            key={cls}
            dataKey={cls}
            name={cls}
            stackId="failures"
            fill={colorForClass(cls, classes)}
            maxBarSize={24}
            radius={cls === classes[classes.length - 1] ? [0, 3, 3, 0] : undefined}
            onClick={() => onSegmentClick?.(cls)}
            style={{ cursor: onSegmentClick ? 'pointer' : 'default' }}
          />
        ))}
      </BarChart>
    </ResponsiveContainer>
  );
}
