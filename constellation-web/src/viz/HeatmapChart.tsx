// CONST-23: the viz kit's @nivo/heatmap wrapper — no panel may import @nivo/heatmap directly
// (§4.1/§9). §7.2 C2 encoding: fill = pass_rate on the sequential violet ramp (--seq-1..6, high
// = light); not_run renders as a plain surface cell "—"; stale = 55% opacity + a clock glyph;
// non_viable = --chart-deemphasis + a ✕ glyph. Status is ALWAYS carried by glyph + tooltip +
// table (never color alone, §2.4/§4.4) — the custom cell renderer below is what makes that
// mechanically true (a pure color-scale cell would violate it for not_run/stale/non_viable).
import { ResponsiveHeatMap } from '@nivo/heatmap';
import type { ComputedCell, CellComponentProps } from '@nivo/heatmap';
import { getVizTheme } from './theme';
import { sequentialColor, CHART_CHROME } from './palette';
import { ChartTooltip } from './ChartTooltip';
import type { ChartTooltipRow } from './ChartTooltip';

export interface HeatmapDatum {
  x: string; // column key
  y: number | null; // pass_rate 0..1, or null for not_run/non_viable
  status: 'ok' | 'not_run' | 'stale' | 'non_viable';
  n_samples: number;
  score_stddev: number | null;
  low_confidence: boolean;
  last_run_at: string | null;
  harness_version: string | null;
}

export interface HeatmapRow {
  id: string; // model name
  data: HeatmapDatum[];
}

interface HeatmapChartProps {
  data: HeatmapRow[];
  height: number;
  onCellClick?: (row: string, col: string) => void;
}

const STATUS_GLYPH: Record<HeatmapDatum['status'], string> = {
  ok: '',
  not_run: '—',
  stale: '🕐',
  non_viable: '✕',
};

export function HeatmapChart({ data, height, onCellClick }: HeatmapChartProps) {
  const theme = getVizTheme();

  return (
    <ResponsiveHeatMap
      data={data}
      margin={{ top: 60, right: 20, bottom: 20, left: 140 }}
      valueFormat=">-.0%"
      forceSquare={false}
      xInnerPadding={0.08}
      yInnerPadding={0.08}
      colors={(cell: { data: HeatmapDatum }) => {
        const d = cell.data;
        if (d.status === 'not_run') return 'var(--bg-panel)';
        if (d.status === 'non_viable') return CHART_CHROME.deemphasis;
        if (d.status === 'stale') return sequentialColor(d.y ?? 0);
        return sequentialColor(d.y ?? 0);
      }}
      opacity={1}
      // stale = 55% opacity per §7.2 — nivo's `opacity` prop is scalar-global, so we bake the
      // per-cell alpha into the cell renderer below instead (label/glyph must stay legible).
      label={(cell: { data: HeatmapDatum }) => STATUS_GLYPH[cell.data.status]}
      labelTextColor={{ from: 'color', modifiers: [['darker', 3]] }}
      enableGridX={false}
      enableGridY={false}
      borderWidth={2}
      borderColor="var(--space-900)"
      theme={theme.nivo}
      animate={false}
      cellComponent={(props: CellComponentProps<HeatmapDatum>) => {
        const { cell } = props;
        const status = cell.data.status;
        const opacity = status === 'stale' ? 0.55 : status === 'non_viable' ? 0.7 : 1;
        const isNotRun = status === 'not_run';
        return (
          <g
            transform={`translate(${cell.x - cell.width / 2}, ${cell.y - cell.height / 2})`}
            onClick={() => onCellClick?.(cell.serieId, cell.data.x)}
            style={{ cursor: onCellClick ? 'pointer' : 'default' }}
          >
            <rect
              width={cell.width}
              height={cell.height}
              fill={isNotRun ? 'var(--bg-panel)' : cell.color}
              opacity={opacity}
              stroke="var(--space-900)"
              strokeWidth={2}
            />
            <text
              x={cell.width / 2}
              y={cell.height / 2}
              textAnchor="middle"
              dominantBaseline="central"
              style={{
                fontFamily: 'var(--font-mono)',
                fontSize: 11,
                fill: isNotRun ? 'var(--text-faint)' : status === 'non_viable' ? 'var(--text-muted)' : 'var(--space-900)',
                pointerEvents: 'none',
              }}
            >
              {STATUS_GLYPH[status]}
            </text>
          </g>
        );
      }}
      tooltip={({ cell }: { cell: ComputedCell<HeatmapDatum> }) => {
        const d = cell.data;
        const rows: ChartTooltipRow[] = [
          { key: 'status', label: 'status', value: d.status },
          { key: 'pass_rate', label: 'pass_rate', value: d.y != null ? `${Math.round(d.y * 100)}%` : '—' },
          { key: 'n', label: 'n_samples', value: String(d.n_samples) },
          { key: 'stddev', label: 'stddev', value: d.score_stddev != null ? `±${d.score_stddev.toFixed(2)}` : '—' },
          { key: 'last_run', label: 'last_run_at', value: d.last_run_at ?? 'never' },
          { key: 'harness', label: 'harness', value: d.harness_version ?? '—' },
        ];
        if (d.low_confidence) rows.push({ key: 'lc', label: '⚠', value: 'low confidence' });
        return <ChartTooltip title={`${cell.serieId} · ${cell.data.x}`} rows={rows} />;
      }}
    />
  );
}
