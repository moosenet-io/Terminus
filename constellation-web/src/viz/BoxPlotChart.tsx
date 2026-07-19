// CONST-24: the viz kit's @nivo/boxplot wrapper — no panel may import @nivo/boxplot directly
// (§4.1/§9). §7.2 C3 encoding: horizontal boxes per model, single hue `--series-1` (models are
// nominal here, row label carries identity — not a categorical series comparison), thin boxes,
// 2px ink median, 1px whiskers, outlier dots >=8px with surface rings; log-scale x toggle
// default ON; n<5 groups render as a beeswarm strip instead of a box (rendered by this same
// component, not ScatterChart/SwarmPlotChart, since it needs to share the box chart's x-scale).
//
// EXACT-QUANTILE TRICK: our data contract (§8 `/mint/box`) gives SERVER-COMPUTED quartiles
// {min,q1,median,q3,max}, not raw per-run values — but @nivo/boxplot only knows how to derive
// quartiles itself from a raw `data: RawDatum[]` array (it has no "I already know the
// quantiles" mode). Feeding it a synthetic 5-element array [min,q1,median,q3,max] per group
// reproduces our exact server quartiles: for n=5 raw values, d3's quantile interpolation at
// fractions [0, .25, .5, .75, 1] lands on EXACT array indices (0, 1, 2, 3, 4) with zero
// interpolation error. This is deliberate, not a hack-of-convenience — it's the only way to
// honor server-side quartiles through nivo's box-plot statistics engine without recomputing
// quantiles ourselves (which would silently diverge from whatever quantile method the backend
// uses). Real outliers (and the true `n`) are carried in our OWN data (`MintBoxGroup`) and
// rendered by a custom SVG layer positioned via the same `xScale`/`yScale` nivo exposes to
// custom layers — nivo's synthetic n=5/box is never shown to the user directly (all tooltips
// and the table view read from the real `MintBoxGroup`).
import { useMemo } from 'react';
import { ResponsiveBoxPlot } from '@nivo/boxplot';
import type { BoxPlotCustomLayerProps, BoxPlotDatum } from '@nivo/boxplot';
import { getVizTheme } from './theme';
import { CATEGORICAL_HEX, CHART_CHROME } from './palette';
import { ChartTooltip } from './ChartTooltip';
import type { ChartTooltipRow } from './ChartTooltip';
import type { MintBoxGroup } from '../lib/aggregationClient';

export interface BoxPlotChartProps {
  groups: MintBoxGroup[];
  height: number;
  /** §7.2: log-scale x toggle, default ON. */
  logScale: boolean;
  onOutlierClick?: (outlier: MintBoxGroup['outliers'][number], model: string) => void;
}

const SINGLE_HUE = CATEGORICAL_HEX[0]; // --series-1, per §7.2 ("single hue")
const LOW_N_THRESHOLD = 5;

/** Synthetic 5-point dataset that makes nivo's own quantile computation reproduce our exact
 *  server quartiles (see file header). `group` doubles as the row label (§7.2: model identity). */
function toBoxPlotData(groups: MintBoxGroup[]): BoxPlotDatum[] {
  const out: BoxPlotDatum[] = [];
  for (const g of groups) {
    if (g.n < LOW_N_THRESHOLD) continue; // rendered as a beeswarm strip instead, see below
    for (const v of [g.min, g.q1, g.median, g.q3, g.max]) {
      out.push({ group: g.model, value: Math.max(v, 0.001) }); // log-scale needs value > 0
    }
  }
  return out;
}

/** Custom layer: draws >=8px outlier dots with a surface ring at each outlier's real value,
 *  aligned to nivo's own xScale/yScale for the box rows (so outliers land exactly on their
 *  model's row and true value, log-scale-aware). */
function OutlierLayer(groups: MintBoxGroup[], onOutlierClick?: BoxPlotChartProps['onOutlierClick']) {
  return function Layer({ xScale, yScale, boxPlots }: BoxPlotCustomLayerProps<BoxPlotDatum>) {
    return (
      <g>
        {groups.filter(g => g.n >= LOW_N_THRESHOLD).flatMap(g => {
          const box = boxPlots.find(b => b.group === g.model);
          if (!box) return [];
          // Row center for this group, in pixels — horizontal layout puts groups on the y-axis.
          const rowY = box.y;
          return g.outliers.map(o => {
            const x = xScale(Math.max(o.value, 0.001));
            return (
              <g key={o.run_id} transform={`translate(${x},${rowY})`} style={{ cursor: onOutlierClick ? 'pointer' : 'default' }} onClick={() => onOutlierClick?.(o, g.model)}>
                <circle r={5} fill="var(--bg-panel)" stroke={SINGLE_HUE} strokeWidth={2} />
              </g>
            );
          });
        })}
      </g>
    );
  };
}

/** n<5 fallback (§7.2): a simple jittered-dot strip per low-n model, sharing the parent's log/
 *  linear x-scale so it visually lines up with the box-plot rows above it. Deliberately NOT the
 *  full SwarmPlotChart (C5) — this is a single-row-per-model strip of raw sample values, not a
 *  per-run judge-score distribution; a lighter, purpose-built renderer keeps it honest. */
function LowNStrip({ groups, logScale, width }: { groups: MintBoxGroup[]; logScale: boolean; width: number }) {
  const theme = getVizTheme();
  const allValues = groups.flatMap(g => g.raw_values ?? []);
  if (allValues.length === 0 || groups.length === 0) return null;
  const min = Math.max(0.001, Math.min(...allValues));
  const max = Math.max(...allValues, min * 1.001);

  const scaleX = (v: number): number => {
    const value = Math.max(v, 0.001);
    if (logScale) {
      const t = (Math.log(value) - Math.log(min)) / (Math.log(max) - Math.log(min) || 1);
      return t * (width - 40) + 20;
    }
    const t = (value - min) / (max - min || 1);
    return t * (width - 40) + 20;
  };

  return (
    <div style={{ display: 'flex', flexDirection: 'column', gap: 4, marginTop: 4 }}>
      {groups.map(g => (
        <div key={g.model} style={{ position: 'relative', height: 22, display: 'flex', alignItems: 'center' }} title={`${g.model}: n=${g.n} (< 5 — beeswarm strip, not a box)`}>
          <span style={{ position: 'absolute', left: 0, fontSize: 10, fontFamily: theme.fontMono, color: theme.textMuted }}>
            {g.model} <span style={{ color: 'var(--status-warning)' }}>⚠ n={g.n}</span>
          </span>
          <svg width={width} height={22} style={{ overflow: 'visible' }}>
            {(g.raw_values ?? []).map((v, i) => (
              <circle key={i} cx={scaleX(v)} cy={11} r={5} fill={SINGLE_HUE} fillOpacity={0.85} stroke="var(--bg-panel)" strokeWidth={1.5}>
                <title>{`${g.model}: ${v}`}</title>
              </circle>
            ))}
          </svg>
        </div>
      ))}
    </div>
  );
}

export function BoxPlotChart({ groups, height, logScale, onOutlierClick }: BoxPlotChartProps) {
  const theme = getVizTheme();
  // @nivo/boxplot's theme type adds a `translation` bag (i18n strings) on top of the shared
  // PartialTheme every other nivo chart in this kit accepts as-is — supply an empty one rather
  // than loosen the shared `VizTheme.nivo` type just for this one consumer.
  const boxplotTheme = { ...theme.nivo, translation: {} };
  const highNGroups = useMemo(() => groups.filter(g => g.n >= LOW_N_THRESHOLD), [groups]);
  const lowNGroups = useMemo(() => groups.filter(g => g.n < LOW_N_THRESHOLD), [groups]);
  const data = useMemo(() => toBoxPlotData(groups), [groups]);
  const groupOrder = useMemo(() => highNGroups.map(g => g.model), [highNGroups]);
  const outlierLayer = useMemo(() => OutlierLayer(groups, onOutlierClick), [groups, onOutlierClick]);

  const boxHeight = lowNGroups.length > 0 ? Math.max(120, height - lowNGroups.length * 26 - 16) : height;

  return (
    <div style={{ height }}>
      {highNGroups.length > 0 && (
        <div style={{ height: boxHeight }}>
          <ResponsiveBoxPlot
            data={data}
            groupBy="group"
            groups={groupOrder}
            value="value"
            layout="horizontal"
            margin={{ top: 10, right: 30, bottom: 40, left: 140 }}
            padding={0.55} /* thin ~12px boxes relative to the row band */
            valueScale={logScale ? { type: 'log', base: 10 } : { type: 'linear' }}
            colors={SINGLE_HUE}
            borderWidth={1}
            borderColor={SINGLE_HUE}
            medianWidth={2}
            medianColor="var(--text-100)"
            whiskerWidth={1}
            whiskerColor={SINGLE_HUE}
            whiskerEndSize={0.4}
            axisBottom={{ legend: logScale ? 'total_time_ms (log)' : 'total_time_ms', legendPosition: 'middle', legendOffset: 34 }}
            theme={boxplotTheme}
            animate={false}
            layers={['grid', 'axes', 'boxPlots', outlierLayer, 'markers']}
            tooltip={({ formatted, label }) => {
              const g = groups.find(x => x.model === label);
              const rows: ChartTooltipRow[] = g
                ? [
                    { key: 'min', label: 'min', value: String(g.min) },
                    { key: 'q1', label: 'q1', value: String(g.q1) },
                    { key: 'median', label: 'median', value: String(g.median) },
                    { key: 'q3', label: 'q3', value: String(g.q3) },
                    { key: 'max', label: 'max', value: String(g.max) },
                    { key: 'n', label: 'n', value: String(g.n) },
                  ]
                : [{ key: 'v', label: 'value', value: String(formatted.quantiles) }];
              return <ChartTooltip title={String(label)} rows={rows} />;
            }}
          />
        </div>
      )}
      {lowNGroups.length > 0 && (
        <LowNStrip groups={lowNGroups} logScale={logScale} width={520} />
      )}
      {groups.length === 0 && (
        <div style={{ height, display: 'flex', alignItems: 'center', justifyContent: 'center', color: CHART_CHROME.deemphasis }}>
          no groups
        </div>
      )}
    </div>
  );
}
