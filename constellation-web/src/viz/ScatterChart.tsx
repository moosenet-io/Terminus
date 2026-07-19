// CONST-23: the viz kit's @nivo/scatterplot wrapper — no panel may import @nivo/scatterplot
// directly (§4.1/§9). Built for §7.2 C4 (Pareto scatter) but generic enough for any nivo
// scatter use: x/y scale specs are caller-supplied (C4 needs x=log), size is a caller-computed
// per-point value (C4's √-scaled VRAM 8-24px), and a custom SVG layer renders the Pareto-front
// step line — nivo's own layers don't include "connect these specific points with a line".
import { ResponsiveScatterPlot } from '@nivo/scatterplot';
import type { ScatterPlotLayerProps, ScatterPlotNodeProps } from '@nivo/scatterplot';
import { getVizTheme } from './theme';
import { ChartTooltip } from './ChartTooltip';
import type { ChartTooltipRow } from './ChartTooltip';

export interface ScatterDatum {
  x: number;
  y: number;
  size: number;
  color: string;
  label: string;
  tooltipRows: ChartTooltipRow[];
  /** True once this point is on the computed Pareto front (drives the direct-label + front line). */
  onFront?: boolean;
}

export interface ScatterSeries {
  id: string;
  data: ScatterDatum[];
}

interface ScatterChartProps {
  data: ScatterSeries[];
  height: number;
  xScaleType: 'linear' | 'log';
  yScaleType: 'linear' | 'log';
  /** Pareto-front points in x order — rendered as a 2px step line + selective labels. */
  frontPoints?: ScatterDatum[];
  onPointClick?: (point: ScatterDatum) => void;
}

function ParetoFrontLayer(frontPoints: ScatterDatum[] | undefined) {
  return function Layer({ xScale, yScale }: ScatterPlotLayerProps<ScatterDatum>) {
    if (!frontPoints || frontPoints.length < 2) return null;
    const pts = frontPoints
      .slice()
      .sort((a, b) => a.x - b.x)
      .map(p => [xScale(p.x), yScale(p.y)] as const);
    // Step line (upper-left non-dominated front): horizontal-then-vertical hops between points.
    let path = `M ${pts[0][0]},${pts[0][1]}`;
    for (let i = 1; i < pts.length; i++) {
      const [, prevY] = pts[i - 1];
      const [x, y] = pts[i];
      path += ` L ${x},${prevY} L ${x},${y}`;
    }
    return (
      <g>
        <path d={path} fill="none" stroke="var(--accent-bright)" strokeWidth={2} />
        {frontPoints.map((p, i) => (
          // Selective direct labels: only every-other frontier point, to avoid overlap crowding.
          i % 2 === 0 ? (
            <text
              key={p.label}
              x={xScale(p.x)}
              y={yScale(p.y) - 14}
              textAnchor="middle"
              style={{ fontFamily: 'var(--font-mono)', fontSize: 10, fill: 'var(--accent-bright)' }}
            >
              {p.label}
            </text>
          ) : null
        ))}
      </g>
    );
  };
}

// Custom node renderer: nivo's `colors` prop is per-SERIES (one color per serieId), but C4
// needs per-POINT color (selection emphasis/deemphasis, §7.2) — so color/size are read straight
// off our own datum (`node.data`) instead of nivo's ordinal color scale. `animate={false}`
// below means these are already-resolved plain numbers, not react-spring values, so a plain
// <circle> is enough (no @react-spring/web dependency needed here).
function PointNode({ node, isInteractive, onMouseEnter, onMouseMove, onMouseLeave, onClick }: ScatterPlotNodeProps<ScatterDatum>) {
  const r = node.size / 2;
  return (
    <circle
      cx={node.x}
      cy={node.y}
      r={r}
      fill={node.data.color}
      fillOpacity={0.85}
      stroke={node.data.color}
      strokeWidth={1}
      style={{ cursor: isInteractive ? 'pointer' : 'default' }}
      onMouseEnter={isInteractive ? (e => onMouseEnter?.(node, e)) : undefined}
      onMouseMove={isInteractive ? (e => onMouseMove?.(node, e)) : undefined}
      onMouseLeave={isInteractive ? (e => onMouseLeave?.(node, e)) : undefined}
      onClick={isInteractive ? (e => onClick?.(node, e)) : undefined}
    />
  );
}

export function ScatterChart({ data, height, xScaleType, yScaleType, frontPoints, onPointClick }: ScatterChartProps) {
  const theme = getVizTheme();
  const frontLayer = ParetoFrontLayer(frontPoints);

  return (
    <ResponsiveScatterPlot
      data={data}
      margin={{ top: 20, right: 30, bottom: 50, left: 60 }}
      xScale={xScaleType === 'log' ? { type: 'log', base: 10 } : { type: 'linear' }}
      yScale={yScaleType === 'log' ? { type: 'log', base: 10 } : { type: 'linear', min: 0, max: 5 }}
      axisBottom={{ legend: 'mean latency (ms, log)', legendPosition: 'middle', legendOffset: 40 }}
      axisLeft={{ legend: 'mean score (0-5)', legendPosition: 'middle', legendOffset: -45 }}
      nodeSize={(d: { data: ScatterDatum }) => d.data.size}
      nodeComponent={PointNode}
      useMesh
      theme={theme.nivo}
      layers={['grid', 'axes', 'nodes', frontLayer, 'markers', 'mesh', 'legends']}
      onClick={(node: { data: ScatterDatum }) => onPointClick?.(node.data)}
      tooltip={({ node }: { node: { data: ScatterDatum } }) => (
        <ChartTooltip title={node.data.label} rows={node.data.tooltipRows} />
      )}
      animate={false}
    />
  );
}
