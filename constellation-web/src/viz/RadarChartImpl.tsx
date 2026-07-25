// CGUI-09 (TERM #532): the nivo ResponsiveRadar implementation for the per-model dimension
// radar. Loaded ONLY through the React.lazy wrapper in RadarChart.tsx, so `@nivo/radar` +
// `@nivo/core` land in the lazy `viz` rollup chunk (vite.config.ts manualChunks) that CONST-17
// reserved for the MINT/Models charts — never in the shell's initial bundle.
//
// This is the first real consumer of the reserved nivo `viz` chunk (Muse's scatter stayed on
// Recharts per viz/recharts.ts). The chart is theme-bridged via getVizTheme().nivo (§4.2) so
// gridlines/labels/tooltip match every other chart; series color comes from the DS categorical
// palette (raw-hex literals live only in viz/palette.ts, imported here — never inlined).
import { ResponsiveRadar } from '@nivo/radar';
import { CATEGORICAL_HEX } from './palette';
import { getVizTheme } from './theme';
import type { RadarAxis } from '../panels/models/modelsData';

export interface RadarChartImplProps {
  axes: RadarAxis[];
}

export default function RadarChartImpl({ axes }: RadarChartImplProps) {
  const theme = getVizTheme();
  // nivo wants plain objects keyed by the index field + one key per series. Single-series
  // (the selected model's per-category pass rate); maxValue 1 because pass rates are 0–1.
  const data = axes.map(a => ({ category: a.category, score: a.score }));
  return (
    <ResponsiveRadar
      data={data}
      keys={['score']}
      indexBy="category"
      maxValue={1}
      // numeric layout props (nivo API units, not CSS px) — deterministic chart geometry.
      margin={{ top: 40, right: 60, bottom: 40, left: 60 }}
      gridShape="circular"
      gridLabelOffset={16}
      dotSize={8}
      dotBorderWidth={2}
      colors={[CATEGORICAL_HEX[0]]}
      fillOpacity={0.2}
      borderWidth={2}
      theme={theme.nivo}
      isInteractive
      animate={false}
    />
  );
}
