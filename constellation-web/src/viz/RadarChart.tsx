// CONST-23: the viz kit's @nivo/radar wrapper — no panel may import @nivo/radar directly
// (§4.1/§9). Brand theme via getVizTheme(); series colors are caller-supplied (SlotAssigner
// output) rather than nivo's own ordinal scale, so color stays stable across filtering (§4.2).
//
// §7.2 C1 encoding: 2px lines, ~10% fills, vertices >=8px with a surface ring, fleet median
// wears --chart-deemphasis (not a categorical slot — it's the one non-nominal series here).
// Missing dimensions (a model profiled on fewer than `dimensions.length` axes) render at 0 with
// a hollow vertex + a caveat (surfaced via `hollowKeys` below).
//
// Vertex tooltip (§7.2: "value, raw, ±std_dev, n, ⚠"): nivo's radar tooltip primitive is a
// per-AXIS slice tooltip (all series' values at one dimension) rather than a true per-point
// hover — that IS the vertex-tooltip granularity this chart needs (a "vertex" here is one
// series' point on one axis; hovering the axis surfaces every series' vertex at once, which is
// strictly more informative, not less).
import { ResponsiveRadar } from '@nivo/radar';
import type { PointData } from '@nivo/radar';
import { getVizTheme } from './theme';
import { ChartTooltip } from './ChartTooltip';
import type { ChartTooltipRow } from './ChartTooltip';

export interface RadarSeries {
  id: string;
  label: string;
  color: string;
  /** true for the fleet-median reference series (deemphasis chrome, not a categorical slot). */
  isReference?: boolean;
}

export interface RadarDatum {
  dimension: string;
  /** One key per series id -> normalized 0..1 value (0 when synthesized for a missing axis). */
  [seriesId: string]: number | string;
}

export interface RadarVertexMeta {
  raw: number;
  std_dev: number;
  n: number;
  low_confidence: boolean;
  /** True when this axis was never profiled for this model (value is a synthesized 0). */
  missing?: boolean;
}

interface RadarChartProps {
  data: RadarDatum[];
  series: RadarSeries[];
  height: number;
  /** Per-vertex metadata for the tooltip, keyed `${dimension}::${seriesId}`. */
  meta: Map<string, RadarVertexMeta>;
  onAxisClick?: (dimension: string) => void;
}

/** §7.2: a model with fewer than the full 8 dimensions renders the missing axes with a
 *  HOLLOW vertex (surface-fill ring, series-colored stroke only) instead of a filled dot. */
function HollowAwareDot(meta: Map<string, RadarVertexMeta>) {
  return function Dot({ datum, size, color }: { datum: PointData; size: number; color: string; borderWidth: number; borderColor: string }) {
    const isMissing = meta.get(`${datum.index}::${datum.key}`)?.missing === true;
    const r = Math.max(4, size / 2);
    // §7.2: vertices >=8px with a surface ring (filled) — missing axes render hollow
    // (surface fill, series-colored stroke only) instead.
    return (
      <circle
        r={r}
        fill={isMissing ? 'var(--bg-panel)' : color}
        stroke={isMissing ? color : 'var(--bg-panel)'}
        strokeWidth={2}
      />
    );
  };
}

export function RadarChart({ data, series, height, meta, onAxisClick }: RadarChartProps) {
  const theme = getVizTheme();
  // nivo's radar `colors` config is keyed by { key, index } (key === our series id).
  const colorFor = (d: { key: string }) => series.find(s => s.id === d.key)?.color ?? 'var(--chart-deemphasis)';
  const hasMissing = [...meta.values()].some(m => m.missing);
  const dotSymbol = HollowAwareDot(meta);

  return (
    <div style={{ height }}>
      <ResponsiveRadar
        data={data}
        keys={series.map(s => s.id)}
        indexBy="dimension"
        maxValue={1}
        margin={{ top: 46, right: 70, bottom: 34, left: 70 }}
        gridLevels={4}
        gridShape="circular"
        gridLabelOffset={16}
        dotSize={9}
        dotBorderWidth={2}
        dotBorderColor={{ from: 'color' }}
        dotSymbol={dotSymbol}
        enableDotLabel={false}
        colors={colorFor}
        fillOpacity={0.1}
        borderWidth={2}
        borderColor={{ from: 'color' }}
        blendMode="normal"
        theme={theme.nivo}
        motionConfig="gentle"
        onClick={(datum: unknown) => {
          const idx = (datum as { index?: string }).index;
          if (idx && onAxisClick) onAxisClick(idx);
        }}
        sliceTooltip={({ index, data: sliceData }) => {
          const rows: ChartTooltipRow[] = sliceData.map(d => {
            const s = series.find(x => x.id === d.id);
            const m = meta.get(`${index}::${d.id}`);
            const parts = [d.formattedValue];
            if (m) {
              parts.push(`raw ${m.raw.toFixed(2)}`, `±${m.std_dev.toFixed(2)}`, `n=${m.n}`);
              if (m.low_confidence) parts.push('⚠ low n');
              if (m.missing) parts.push('⚠ not profiled');
            }
            return {
              key: d.id,
              label: s?.label ?? d.id,
              value: parts.join(' · '),
              color: d.color,
            };
          });
          return <ChartTooltip title={String(index)} rows={rows} />;
        }}
      />
      {hasMissing && (
        <div style={{ fontSize: 'var(--fs-xs)', color: 'var(--text-faint)', marginTop: -8, textAlign: 'center' }}>
          ⚠ hollow vertex = dimension not yet profiled for that model (rendered at 0)
        </div>
      )}
    </div>
  );
}
