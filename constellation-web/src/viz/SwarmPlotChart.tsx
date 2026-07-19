// CONST-24: the viz kit's @nivo/swarmplot wrapper — no panel may import @nivo/swarmplot
// directly (§4.1/§9). §7.2 C5 encoding: vertical lanes per model (caller caps at <=4 selected,
// else top-4 by n), discrete 1-5 judge scores per run, 8px dots with surface rings, model
// series colors, a 2px ink lane-median tick, failure_class != 'none' -> HOLLOW dots (ring-only —
// shape/fill carries the flag, never color alone per §2.4/§4.4), >400 dots/lane decimates with
// a "showing n of N" caption, and a lane-header click hook (-> C6 filtered to that model).
//
// Nodes are rendered by a CUSTOM layer (plain SVG, own mouse handlers) rather than nivo's
// default `circleComponent`, which is spring-animated (`@react-spring/web` SpringValue style
// props) — this component needs per-datum fill/hollow branching and a stable, un-animated
// hit-test surface for the run-drill-down click, so it draws its own circles the same way
// BoxPlotChart's outlier layer and ScatterChart's Pareto-front layer already do in this kit.
import { useMemo, useState } from 'react';
import { ResponsiveSwarmPlot } from '@nivo/swarmplot';
import type { SwarmPlotCustomLayerProps } from '@nivo/swarmplot';
import type { ScaleLinear, ScaleTime } from '@nivo/scales';
import type { ScaleOrdinal } from 'd3-scale';
import { getVizTheme } from './theme';
import { ChartTooltip } from './ChartTooltip';
import type { ChartTooltipRow } from './ChartTooltip';
import type { MintRun } from '../lib/aggregationClient';

// nivo's own layer union covers all three scale shapes a swarm axis can end up with (value axis
// is always linear here, but the type isn't narrowed at the `layers` prop boundary) — matching
// it exactly (rather than `never`/`any`) keeps the custom layers real-typed.
type AnySwarmScale = ScaleLinear<number> | ScaleTime<string | Date> | ScaleOrdinal<string, number, never>;

export interface SwarmLane {
  id: string; // model
  color: string;
}

interface SwarmPlotChartProps {
  runs: MintRun[];
  lanes: SwarmLane[]; // ordered, <=4 (caller's job per §7.2)
  height: number;
  onDotClick?: (run: MintRun) => void;
  onLaneHeaderClick?: (model: string) => void;
}

const DOT_RADIUS = 4; // 8px diameter, per §7.2
const DECIMATE_THRESHOLD = 400;

/** Density-preserving decimation: samples each score bucket (1-5) proportionally rather than a
 *  naive random/prefix cut, so the swarm's visual score distribution still matches the real one
 *  after decimating — a naive cut could, e.g., drop every failing run because they cluster at
 *  the end of the fixture's insertion order. */
function decimateLane(runs: MintRun[], target: number): MintRun[] {
  if (runs.length <= target) return runs;
  const byScore = new Map<number, MintRun[]>();
  for (const r of runs) {
    const bucket = byScore.get(r.score) ?? [];
    bucket.push(r);
    byScore.set(r.score, bucket);
  }
  const out: MintRun[] = [];
  for (const bucket of byScore.values()) {
    const keep = Math.max(1, Math.round((bucket.length / runs.length) * target));
    const step = bucket.length / keep;
    for (let i = 0; i < keep; i++) {
      out.push(bucket[Math.min(bucket.length - 1, Math.floor(i * step))]);
    }
  }
  return out;
}

function median(values: number[]): number {
  const sorted = [...values].sort((a, b) => a - b);
  const mid = Math.floor(sorted.length / 2);
  return sorted.length % 2 === 0 ? (sorted[mid - 1] + sorted[mid]) / 2 : sorted[mid];
}

interface SwarmNodeDatum {
  run_id: string;
  run: MintRun;
  model: string;
  score: number;
}

function CircleLayer({ nodes, laneColor, onDotClick, onHover }: {
  nodes: { x: number; y: number; data: SwarmNodeDatum }[];
  laneColor: (model: string) => string;
  onDotClick?: (run: MintRun) => void;
  onHover: (n: { x: number; y: number; run: MintRun } | null) => void;
}) {
  return (
    <g>
      {nodes.map(n => {
        const isHollow = n.data.run.failure_class !== 'none';
        const color = laneColor(n.data.model);
        return (
          <circle
            key={n.data.run.run_id}
            cx={n.x}
            cy={n.y}
            r={DOT_RADIUS}
            fill={isHollow ? 'var(--bg-panel)' : color}
            stroke={color}
            strokeWidth={isHollow ? 2 : 1.5}
            style={{ cursor: onDotClick ? 'pointer' : 'default' }}
            onMouseEnter={() => onHover({ x: n.x, y: n.y, run: n.data.run })}
            onMouseLeave={() => onHover(null)}
            onClick={() => onDotClick?.(n.data.run)}
          />
        );
      })}
    </g>
  );
}

/** 2px ink median tick per lane (§7.2). Lane x-center and the value->pixel-y mapping both come
 *  straight off nivo's already-computed `nodes` (each carries its lane's shared x + a linear-in-
 *  value y) — no need to re-derive nivo's internal scales ourselves. */
function MedianTickLayer({ lanes, runsByLane, nodes }: {
  lanes: SwarmLane[];
  runsByLane: Map<string, MintRun[]>;
  nodes: { x: number; y: number; data: SwarmNodeDatum }[];
}) {
  return (
    <g>
      {lanes.map(lane => {
        const runs = runsByLane.get(lane.id) ?? [];
        const laneNodes = nodes.filter(n => n.data.model === lane.id);
        if (runs.length === 0 || laneNodes.length < 2) return null;
        const m = median(runs.map(r => r.score));
        const x = laneNodes[0].x;
        // Two known (value, y) samples from this lane give us the linear value->pixel mapping
        // without needing nivo's internal yScale directly.
        const a = laneNodes[0];
        const b = laneNodes.reduce((best, n) => Math.abs(n.data.score - a.data.score) > Math.abs(best.data.score - a.data.score) ? n : best, laneNodes[0]);
        if (a.data.score === b.data.score) return null;
        const t = (m - a.data.score) / (b.data.score - a.data.score);
        const y = a.y + t * (b.y - a.y);
        return (
          <line key={lane.id} x1={x - 14} x2={x + 14} y1={y} y2={y} stroke="var(--text-100)" strokeWidth={2} />
        );
      })}
    </g>
  );
}

export function SwarmPlotChart({ runs, lanes, height, onDotClick, onLaneHeaderClick }: SwarmPlotChartProps) {
  const theme = getVizTheme();
  const [hover, setHover] = useState<{ x: number; y: number; run: MintRun } | null>(null);

  const runsByLane = useMemo(() => {
    const m = new Map<string, MintRun[]>();
    for (const lane of lanes) m.set(lane.id, runs.filter(r => r.model === lane.id));
    return m;
  }, [runs, lanes]);

  const decimationInfo = useMemo(() => {
    const info: { total: number; shown: number }[] = [];
    for (const lane of lanes) {
      const laneRuns = runsByLane.get(lane.id) ?? [];
      info.push({ total: laneRuns.length, shown: Math.min(laneRuns.length, DECIMATE_THRESHOLD) });
    }
    return info;
  }, [lanes, runsByLane]);

  const anyDecimated = decimationInfo.some(d => d.total > DECIMATE_THRESHOLD);

  const data: SwarmNodeDatum[] = useMemo(() => {
    const out: SwarmNodeDatum[] = [];
    for (const lane of lanes) {
      const laneRuns = runsByLane.get(lane.id) ?? [];
      const shown = decimateLane(laneRuns, DECIMATE_THRESHOLD);
      for (const r of shown) out.push({ run_id: r.run_id, run: r, model: lane.id, score: r.score });
    }
    return out;
  }, [lanes, runsByLane]);

  const laneColorOf = (model: string) => lanes.find(l => l.id === model)?.color ?? 'var(--chart-deemphasis)';
  const groupIds = lanes.map(l => l.id);
  const chartHeight = anyDecimated ? height - 20 : height;

  if (lanes.length === 0 || data.length === 0) {
    return (
      <div style={{ height, display: 'flex', alignItems: 'center', justifyContent: 'center', color: 'var(--text-muted)' }}>
        No runs for this filter
      </div>
    );
  }

  return (
    <div style={{ height }}>
      <div style={{ height: chartHeight, position: 'relative' }}>
        <ResponsiveSwarmPlot
          data={data}
          groups={groupIds}
          groupBy="model"
          id="run_id"
          value="score"
          valueScale={{ type: 'linear', min: 1, max: 5 }}
          size={DOT_RADIUS * 2}
          spacing={2}
          layout="vertical"
          gap={16}
          margin={{ top: 30, right: 20, bottom: 30, left: 40 }}
          axisLeft={{ legend: 'judge score', legendPosition: 'middle', legendOffset: -32, tickValues: [1, 2, 3, 4, 5] }}
          axisTop={{
            renderTick: tick => (
              <g transform={`translate(${tick.x},${tick.y})`}>
                <text
                  textAnchor="middle"
                  dy={-4}
                  onClick={() => onLaneHeaderClick?.(String(tick.value))}
                  style={{ cursor: onLaneHeaderClick ? 'pointer' : 'default', fontFamily: theme.fontMono, fontSize: 11, fill: 'var(--text-100)' }}
                >
                  {String(tick.value)}
                </text>
              </g>
            ),
          }}
          enableGridX={false}
          enableGridY
          colors={(d: { group: string }) => laneColorOf(d.group)}
          colorBy="group"
          theme={theme.nivo}
          animate={false}
          isInteractive={false}
          layers={[
            'grid',
            'axes',
            (props: SwarmPlotCustomLayerProps<SwarmNodeDatum, AnySwarmScale>) => (
              <CircleLayer
                nodes={props.nodes as unknown as { x: number; y: number; data: SwarmNodeDatum }[]}
                laneColor={laneColorOf}
                onDotClick={onDotClick}
                onHover={setHover}
              />
            ),
            (props: SwarmPlotCustomLayerProps<SwarmNodeDatum, AnySwarmScale>) => (
              <MedianTickLayer
                lanes={lanes}
                runsByLane={runsByLane}
                nodes={props.nodes as unknown as { x: number; y: number; data: SwarmNodeDatum }[]}
              />
            ),
          ]}
        />
        {hover && (
          <div style={{ position: 'absolute', left: hover.x + 12, top: hover.y - 12, pointerEvents: 'none', zIndex: 10 }}>
            <ChartTooltip
              title={hover.run.model}
              rows={([
                { key: 'case', label: 'case_id', value: hover.run.case_id },
                { key: 'lang', label: 'language', value: hover.run.language },
                { key: 'task', label: 'task_category', value: hover.run.task_category },
                { key: 'score', label: 'score', value: String(hover.run.score) },
                { key: 'failure', label: 'failure_class', value: hover.run.failure_class },
                { key: 'time', label: 'total_time_ms', value: String(hover.run.total_time_ms) },
              ]) as ChartTooltipRow[]}
            />
          </div>
        )}
      </div>
      {anyDecimated && (
        <div style={{ fontSize: 'var(--fs-xs)', color: 'var(--text-faint)', textAlign: 'center', marginTop: 4 }}>
          {decimationInfo.map((d, i) => d.total > DECIMATE_THRESHOLD ? `${lanes[i].id}: showing ${d.shown} of ${d.total}` : null).filter(Boolean).join(' · ')}
        </div>
      )}
    </div>
  );
}
