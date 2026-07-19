// CONST-24: the viz kit's @nivo/parallel-coordinates wrapper — no panel may import
// @nivo/parallel-coordinates directly (§4.1/§9). See `nivo-parallel-coordinates.d.ts` for why
// this file needs an ambient module shim (the installed package's shipped types are missing).
//
// §7.2 C9 encoding: 6 fixed-order dims, normalized 0..1 server-side (raw ranges travel in the
// payload so axis ticks can show REAL units via a custom `tickFormat`); selected models (<=4)
// render 2px in series colors, every other profiled model renders 1px `--chart-deemphasis`
// context lines; an axis brush (drag a vertical range on any axis) further dims lines outside
// the selected range on ALL active axes, with a "reset brush" chip when any is active; hovering
// a line lifts it to 3px + shows a raw-value tooltip; <2 models with all 6 dims -> empty with a
// counted caveat for any excluded partial models.
//
// REFERENCE-ROW TRICK: nivo's parallel-coordinates has no API to hand it axis pixel positions
// or the value<->pixel mapping directly (its custom-layer context is just
// {computedData, variables, lineGenerator} — no scale accessors, unlike boxplot/scatter/swarm).
// So this component injects two invisible reference rows into the dataset whose every
// dimension is pinned to exactly 0 and exactly 1 (the domain endpoints, since every axis here
// has explicit `min:0,max:1`). Nivo computes real pixel coordinates for those rows exactly like
// any other line; reading them back out of `computedData` gives this component an EXACT
// per-axis (value=0 -> pixel, value=1 -> pixel) mapping, which is everything needed for the
// axis brush and for drawing this wrapper's own line/lift layer. The reference rows are
// filtered out of every visible layer (lines, tooltip, brush hit-test) — they exist purely as
// a coordinate probe, the same spirit as BoxPlotChart's exact-quantile synthetic points.
import { useMemo, useState } from 'react';
import { ResponsiveParallelCoordinates } from '@nivo/parallel-coordinates';
import type { ParallelCoordinatesCustomLayerContext, ParallelCoordinatesVariable } from '@nivo/parallel-coordinates';
import { getVizTheme } from './theme';
import { ChartTooltip } from './ChartTooltip';
import type { ChartTooltipRow } from './ChartTooltip';
import { CHART_CHROME } from './palette';
import type { MintTradeoffDim, MintTradeoffPoint, MintTradeoffDimKey } from '../lib/aggregationClient';

const REF_MIN_ID = '__pc_ref_min__';
const REF_MAX_ID = '__pc_ref_max__';

interface PCRow {
  id: string;
  model: string;
  [dimKey: string]: string | number;
}

interface AxisGeometry {
  x: number;
  yAtZero: number;
  yAtOne: number;
}

/** value (0..1) -> pixel y for one axis, given its two reference points. */
function valueToY(geo: AxisGeometry, v: number): number {
  return geo.yAtZero + v * (geo.yAtOne - geo.yAtZero);
}

/** pixel y -> value (0..1) for one axis (inverse of valueToY) — used to interpret a brush drag. */
function yToValue(geo: AxisGeometry, y: number): number {
  const span = geo.yAtOne - geo.yAtZero;
  return span === 0 ? 0 : (y - geo.yAtZero) / span;
}

function realUnitLabel(dim: MintTradeoffDim, norm: number): string {
  const clamped = Math.max(0, Math.min(1, norm));
  const raw = dim.invert ? dim.max - clamped * (dim.max - dim.min) : dim.min + clamped * (dim.max - dim.min);
  const rounded = Math.abs(raw) >= 100 ? Math.round(raw) : Math.round(raw * 10) / 10;
  return `${rounded}${dim.unit ? dim.unit : ''}`;
}

export interface ParallelCoordinatesChartProps {
  dims: MintTradeoffDim[];
  points: MintTradeoffPoint[]; // already filtered to complete-6-dim models by the caller
  selectedModels: string[]; // <=4, series-colored
  colorFor: (model: string) => string;
  height: number;
}

export function ParallelCoordinatesChart({ dims, points, selectedModels, colorFor, height }: ParallelCoordinatesChartProps) {
  const theme = getVizTheme();
  const [hoveredModel, setHoveredModel] = useState<string | null>(null);
  const [brushes, setBrushes] = useState<Record<string, [number, number]>>({});
  const [dragAxis, setDragAxis] = useState<{ key: string; startY: number; curY: number } | null>(null);

  const variables: ParallelCoordinatesVariable[] = useMemo(() => dims.map(d => ({
    id: d.key,
    value: d.key,
    min: 0,
    max: 1,
    label: d.label,
    tickValues: [0, 0.25, 0.5, 0.75, 1],
    tickFormat: (v: number) => realUnitLabel(d, v),
  })), [dims]);

  const data: PCRow[] = useMemo(() => {
    const rows: PCRow[] = points.map(p => {
      const row: PCRow = { id: p.model, model: p.model };
      for (const d of dims) row[d.key] = p.norm[d.key] ?? 0;
      return row;
    });
    const refMin: PCRow = { id: REF_MIN_ID, model: REF_MIN_ID };
    const refMax: PCRow = { id: REF_MAX_ID, model: REF_MAX_ID };
    for (const d of dims) { refMin[d.key] = 0; refMax[d.key] = 1; }
    return [...rows, refMin, refMax];
  }, [points, dims]);

  const brushActive = Object.keys(brushes).length > 0;

  const isSelected = (model: string) => selectedModels.includes(model);

  const passesBrush = (row: PCRow, geoByAxis: Map<string, AxisGeometry>): boolean => {
    for (const [dimKey, [lo, hi]] of Object.entries(brushes)) {
      const v = Number(row[dimKey]);
      const [a, b] = lo <= hi ? [lo, hi] : [hi, lo];
      if (v < a || v > b) return false;
    }
    void geoByAxis;
    return true;
  };

  if (points.length < 2) {
    return (
      <div style={{ height, display: 'flex', alignItems: 'center', justifyContent: 'center', color: 'var(--text-muted)' }}>
        Fewer than 2 models have all 6 trade-off dimensions profiled
      </div>
    );
  }

  return (
    <div style={{ height, position: 'relative' }}>
      {brushActive && (
        <button
          type="button"
          onClick={() => setBrushes({})}
          style={{
            position: 'absolute', top: 0, right: 0, zIndex: 5,
            fontFamily: 'var(--font-mono)', fontSize: 'var(--fs-mono-sm)', textTransform: 'uppercase',
            letterSpacing: 'var(--ls-label)', color: 'var(--accent-bright)', background: 'var(--bg-elevated)',
            border: '1px solid var(--border)', borderRadius: 'var(--radius-sm)', padding: '2px 8px', cursor: 'pointer',
          }}
        >
          reset brush
        </button>
      )}
      <ResponsiveParallelCoordinates
        data={data as unknown as Record<string, unknown>[]}
        variables={variables}
        layout="horizontal"
        margin={{ top: 30, right: 40, bottom: 24, left: 40 }}
        isInteractive={false}
        animate={false}
        theme={theme.nivo}
        colors={() => 'transparent'} // color is fully owned by the custom 'lines' layer below
        layers={[
          'axes',
          (ctx: ParallelCoordinatesCustomLayerContext) => {
            const geoByAxis = new Map<string, AxisGeometry>();
            const refMin = ctx.computedData.find(d => d.id === REF_MIN_ID);
            const refMax = ctx.computedData.find(d => d.id === REF_MAX_ID);
            if (refMin && refMax) {
              ctx.variables.forEach((v, i) => {
                geoByAxis.set(v.id, { x: refMin.points[i][0], yAtZero: refMin.points[i][1], yAtOne: refMax.points[i][1] });
              });
            }

            const lines = ctx.computedData.filter(d => d.id !== REF_MIN_ID && d.id !== REF_MAX_ID);

            return (
              <g>
                {/* Lines: selected models 2px in series color, context models 1px de-emphasis;
                    a brush-failing line dims further; the hovered line lifts to 3px. */}
                {lines.map(line => {
                  const model = String(line.data.model);
                  const selected = isSelected(model);
                  const row = data.find(r => r.id === line.id);
                  const inBrush = row ? passesBrush(row, geoByAxis) : true;
                  const isHovered = hoveredModel === model;
                  const baseColor = selected ? colorFor(model) : CHART_CHROME.deemphasis;
                  const width = isHovered ? 3 : selected ? 2 : 1;
                  const opacity = brushActive && !inBrush ? 0.08 : selected ? 0.95 : 0.45;
                  const path = ctx.lineGenerator(line.points);
                  if (!path) return null;
                  return (
                    <path
                      key={line.id}
                      d={path}
                      fill="none"
                      stroke={baseColor}
                      strokeWidth={width}
                      opacity={opacity}
                      style={{ cursor: 'pointer' }}
                      onMouseEnter={() => setHoveredModel(model)}
                      onMouseLeave={() => setHoveredModel(prev => prev === model ? null : prev)}
                    />
                  );
                })}

                {/* Per-axis brush drag surface + active-range highlight. */}
                {ctx.variables.map(v => {
                  const geo = geoByAxis.get(v.id);
                  if (!geo) return null;
                  const active = brushes[v.id];
                  const dragging = dragAxis?.key === v.id ? dragAxis : null;
                  const top = Math.min(geo.yAtZero, geo.yAtOne);
                  const bottom = Math.max(geo.yAtZero, geo.yAtOne);
                  return (
                    <g key={`brush-${v.id}`}>
                      {active && (
                        <rect
                          x={geo.x - 6}
                          y={Math.min(valueToY(geo, active[0]), valueToY(geo, active[1]))}
                          width={12}
                          height={Math.abs(valueToY(geo, active[1]) - valueToY(geo, active[0]))}
                          fill="var(--accent-bright)"
                          opacity={0.18}
                        />
                      )}
                      {dragging && (
                        <rect
                          x={geo.x - 6}
                          y={Math.min(dragging.startY, dragging.curY)}
                          width={12}
                          height={Math.abs(dragging.curY - dragging.startY)}
                          fill="var(--accent-bright)"
                          opacity={0.28}
                        />
                      )}
                      <rect
                        x={geo.x - 8}
                        y={top}
                        width={16}
                        height={bottom - top}
                        fill="transparent"
                        style={{ cursor: 'ns-resize' }}
                        onMouseDown={e => {
                          const svg = (e.target as SVGElement).ownerSVGElement;
                          const rect = svg?.getBoundingClientRect();
                          const localY = rect ? e.clientY - rect.top : e.clientY;
                          setDragAxis({ key: v.id, startY: localY, curY: localY });
                        }}
                        onMouseMove={e => {
                          if (!dragging) return;
                          const svg = (e.target as SVGElement).ownerSVGElement;
                          const rect = svg?.getBoundingClientRect();
                          const localY = rect ? e.clientY - rect.top : e.clientY;
                          setDragAxis({ ...dragging, curY: localY });
                        }}
                        onMouseUp={() => {
                          if (!dragging) return;
                          const v0 = yToValue(geo, dragging.startY);
                          const v1 = yToValue(geo, dragging.curY);
                          if (Math.abs(v1 - v0) > 0.02) {
                            setBrushes(prev => ({ ...prev, [v.id]: [Math.max(0, Math.min(v0, v1)), Math.min(1, Math.max(v0, v1))] }));
                          }
                          setDragAxis(null);
                        }}
                      />
                    </g>
                  );
                })}
              </g>
            );
          },
        ]}
      />
      {hoveredModel && (() => {
        const p = points.find(pt => pt.model === hoveredModel);
        if (!p) return null;
        const rows: ChartTooltipRow[] = dims.map(d => ({
          key: d.key,
          label: d.label,
          value: p.raw[d.key] != null ? `${p.raw[d.key]}${d.unit}` : '—',
        }));
        return (
          <div style={{ position: 'absolute', top: 4, left: 4, pointerEvents: 'none' }}>
            <ChartTooltip title={hoveredModel} rows={rows} />
          </div>
        );
      })()}
    </div>
  );
}

/** Splits fetched tradeoff points into {complete, excluded} — a model missing any of the 6
 *  dims is excluded with a counted caveat rather than silently drawing a broken line (§7.2:
 *  "partial models excluded with a counted caveat"). */
export function partitionCompleteTradeoffs(dims: MintTradeoffDim[], points: MintTradeoffPoint[]): { complete: MintTradeoffPoint[]; excludedCount: number } {
  const keys: MintTradeoffDimKey[] = dims.map(d => d.key);
  const complete = points.filter(p => keys.every(k => p.norm[k] != null));
  return { complete, excludedCount: points.length - complete.length };
}
