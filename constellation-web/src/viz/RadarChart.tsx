// CGUI-10 (TERM #533): nivo radar wrapper — the one place @nivo/radar is imported (the §4.1
// "panels import from src/viz, never nivo directly" rule). Loaded via the reserved lazy `viz`
// chunk (vite.config.ts manualChunks) — panels pull it through React.lazy so nivo stays out of
// the initial bundle until a MINT report actually mounts.
//
// Renders a capability radar: one ring per axis (metric/dimension), one colored web per model.
// All-pairs cap of 4 (§4.2): only the first 4 models get their own web; callers fold/label the
// rest. Values are unit scores in [0,1] (maxValue fixed at 1) so every axis is comparable.
import { ResponsiveRadar } from '@nivo/radar';
import { getVizTheme } from './theme';
import { CATEGORICAL_HEX, ALL_PAIRS_CEILING } from './palette';
import { metricLabel } from '../panels/mint/categoryMeta';
import type { RadarVM } from '../panels/mint/transforms';

interface RadarChartProps {
  vm: RadarVM;
}

export default function RadarChart({ vm }: RadarChartProps) {
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
