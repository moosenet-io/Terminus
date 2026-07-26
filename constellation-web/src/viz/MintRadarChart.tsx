// CGUI-10 (TERM #533): nivo radar wrapper for the MINT per-category reports — one of the two
// radars in the viz kit (the other, RadarChart.tsx/RadarChartImpl.tsx, is CGUI-09's single-
// series per-model radar for the Models module). This one is MULTI-series: one colored web per
// model over a category's metrics/dimensions. Kept as a distinct file because the two consume
// different data shapes (RadarVM here vs. RadarAxis[] there) and can't share one component.
//
// The one place @nivo/radar is imported for the MINT charts (§4.1 "panels import from src/viz,
// never nivo directly"). Loaded via the reserved lazy `viz` chunk — the MINT panel pulls it
// through React.lazy so nivo stays out of the initial bundle until a report mounts.
//
// All-pairs cap of 4 (§4.2): only the first 4 models get their own web; callers fold/label the
// rest. Values are unit scores in [0,1] (maxValue fixed at 1) so every axis is comparable.
import { ResponsiveRadar } from '@nivo/radar';
import { getVizTheme } from './theme';
import { CATEGORICAL_HEX, ALL_PAIRS_CEILING } from './palette';
import { metricLabel } from '../panels/mint/categoryMeta';
import type { RadarVM } from '../panels/mint/transforms';

interface MintRadarChartProps {
  vm: RadarVM;
}

export default function MintRadarChart({ vm }: MintRadarChartProps) {
  const theme = getVizTheme();
  const models = vm.series.slice(0, ALL_PAIRS_CEILING);
  const keys = models.map(m => m.model);

  // nivo shape: one row per axis, each model a numeric key on that row.
  const data = vm.axes.map((axis, i) => {
    const row: Record<string, string | number> = { axis: metricLabel(axis) };
    for (const m of models) row[m.model] = Math.round((m.values[i] ?? 0) * 1000) / 1000;
    return row;
  });

  return (
    <ResponsiveRadar
      data={data}
      keys={keys}
      indexBy="axis"
      maxValue={1}
      margin={{ top: 40, right: 60, bottom: 24, left: 60 }}
      borderColor={{ from: 'color' }}
      gridLabelOffset={20}
      dotSize={6}
      dotBorderWidth={1}
      colors={[...CATEGORICAL_HEX.slice(0, ALL_PAIRS_CEILING)]}
      fillOpacity={0.18}
      blendMode="normal"
      theme={theme.nivo}
      valueFormat={v => (Math.round(v * 100) / 100).toString()}
      gridShape="circular"
      isInteractive
    />
  );
}
