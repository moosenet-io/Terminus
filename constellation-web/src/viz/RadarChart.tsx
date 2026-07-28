// LGUI-09: the viz kit's radar chart-form wrapper (§4.2's "all-pairs" family — radar/boxplot/
// heatmap/parallel-coordinates/swarmplot/scatterplot). CONST-17 shipped only the nivo
// foundation for these, deferring the actual wrapper components to "the routes that use them"
// (README, "The viz kit" section) — this item is the first Recharts-based radar consumer
// (§3.4/§8's trait radar), and no radar wrapper existed on `main` yet at build time, so this
// adds one per the kit's own conventions: panels never touch `recharts`/`@nivo/*` directly,
// only `src/viz/*`.
//
// Scope: ONE primary series + ONE de-emphasis reference series (§8: "trait radar = slot 1 vs
// de-emphasis overlay") — not a general N-series radar. A future item building a genuinely
// multi-series radar (e.g. MINT's 8-dimension capability radar, CONST-22/23) should extend
// this file's props rather than duplicating it, but should keep the all-pairs 4-axis cap
// (`ALL_PAIRS_CEILING`, `src/viz/palette.ts`) in mind if it grows past 4 axes.
//
// RECONCILIATION NOTE (LGUI-06/CONST-22/23/24 reconciliation): this file also independently
// grew a SECOND, differently-named radar (`RadarChart`, CGUI-09/TERM #532) for the Models
// module's per-model detail view — a lazy-loaded `@nivo/radar` wrapper, code-split so the
// shell/roster never pay for the chunk. `RadarChartKit` (this item, Recharts, single+
// de-emphasis series, used by PersonaPanel) and `RadarChart` (nivo, N-axis, lazy, used by
// ModelDetailView) are DIFFERENT components with DIFFERENT exported names and DIFFERENT
// consumers — there is no actual naming collision, so both coexist in this one viz-kit file
// rather than one replacing the other.
import {
  RechartsRadarChart,
  PolarGrid,
  PolarAngleAxis,
  PolarRadiusAxis,
  Radar,
  Tooltip,
  ResponsiveContainer,
} from './recharts';
import { CATEGORICAL_HEX, CHART_CHROME } from './palette';
import { rechartsTickStyle, rechartsTooltipStyle } from './theme';
import { Suspense, lazy } from 'react';
import { ChartSkeleton } from './ChartSkeleton';
import type { RadarAxis } from '../panels/models/modelsData';

export interface RadarAxisPoint {
  /** Axis label, e.g. a trait name ("flair"). Rendered verbatim as the angle-axis tick — the
   *  chart never trusts this for anything except display (textContent-only per the kit's
   *  tooltip-label rule, ChartTooltip.tsx's doc). */
  axis: string;
  /** Primary series value, 0..1 (already clamped by the caller — this component does not
   *  re-clamp, so an out-of-domain value is a caller bug, not a chart concern). */
  value: number;
  /** De-emphasis reference value for the same axis (e.g. a fleet default) — 0..1. Omit the
   *  whole reference series (pass `deemphasisLabel={undefined}`, see below) if there is no
   *  meaningful reference to overlay; a per-axis `undefined` is NOT supported (all-or-nothing,
   *  keeps the two series' vertex counts equal, which Recharts' radar requires anyway). */
  deemphasis?: number;
}

interface RadarChartKitProps {
  data: RadarAxisPoint[];
  /** Domain max for the radius axis — e.g. 1 for a 0..1 trait scale. Domain min is always 0. */
  max: number;
  height: number;
  primaryLabel: string;
  /** Renders the de-emphasis series + its own tooltip row when set; omit entirely to render
   *  only the primary series (e.g. no fleet-default reference available yet). */
  deemphasisLabel?: string;
}

/** One series (§4.2's violet-400 slot 1) + an optional de-emphasis reference series
 *  (`--chart-deemphasis`/`CHART_CHROME.deemphasis`) — the exact pairing §8 specs for the
 *  trait radar. Vertex tooltips come from Recharts' own `Tooltip` (shared kit chrome via
 *  `rechartsTooltipStyle()`), not a custom hover layer. Callers own the `ChartCard` wrapper
 *  (loading/empty/degraded/table-twin) — this component is chart body only, matching every
 *  other form in this barrel (Line/Area/Scatter, etc. also render bare inside `ChartCard`).
 */
export function RadarChartKit({ data, max, height, primaryLabel, deemphasisLabel }: RadarChartKitProps) {
  const tick = rechartsTickStyle();
  return (
    <ResponsiveContainer width="100%" height={height}>
      <RechartsRadarChart data={data} outerRadius="70%">
        <PolarGrid stroke={CHART_CHROME.grid} />
        <PolarAngleAxis dataKey="axis" tick={tick} />
        <PolarRadiusAxis angle={90} domain={[0, max]} tick={tick} axisLine={false} />
        {deemphasisLabel && (
          <Radar
            name={deemphasisLabel}
            dataKey="deemphasis"
            stroke={CHART_CHROME.deemphasis}
            fill={CHART_CHROME.deemphasis}
            fillOpacity={0.12}
            strokeDasharray="4 3"
            isAnimationActive={false}
          />
        )}
        <Radar
          name={primaryLabel}
          dataKey="value"
          stroke={CATEGORICAL_HEX[0]}
          fill={CATEGORICAL_HEX[0]}
          fillOpacity={0.28}
          isAnimationActive={false}
        />
        <Tooltip contentStyle={rechartsTooltipStyle()} />
      </RechartsRadarChart>
    </ResponsiveContainer>
  );
}

// CGUI-09 (TERM #532): lazy boundary for the nivo radar. `RadarChartImpl` (and through it
// `@nivo/radar` + `@nivo/core`) is code-split into the reserved `viz` chunk and only fetched
// when a per-model detail actually renders a radar — the shell/roster never pay for it.
// A ChartSkeleton fills the frame while the chunk loads, so the fixed-height ChartCard body
// never collapses.
const RadarChartImpl = lazy(() => import('./RadarChartImpl'));

export interface RadarChartProps {
  axes: RadarAxis[];
  /** matches the enclosing ChartCard body height so the skeleton is the same size. */
  height: number;
}

export function RadarChart({ axes, height }: RadarChartProps) {
  return (
    <Suspense fallback={<ChartSkeleton height={height} />}>
      <RadarChartImpl axes={axes} />
    </Suspense>
  );
}
