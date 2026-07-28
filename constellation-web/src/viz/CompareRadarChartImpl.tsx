// CONST-22 (reconciled): the nivo ResponsiveRadar implementation for `models.compare`'s MINT
// dimension overlay — up to 4 models, one web per model. This is a THIRD radar shape in the viz
// kit alongside `RadarChart`/`RadarChartImpl` (CGUI-09, single-series per-model category
// profile) and `MintRadarChart` (CGUI-10, multi-series but coupled to the MINT category-report
// `RadarVM`/`metricLabel` shape) — kept as its own small file rather than bent into either,
// because Compare needs a GENERIC `{ dimension, [modelName]: normScore }` row shape driven by
// the caller's own `SlotAssigner` colors (so the radar's per-model color matches the same
// model's color in the compare table and the Pareto scatter), not MINT-category-report-specific
// labeling. Loaded ONLY through the lazy wrapper in CompareRadarChart.tsx, same as the other two
// nivo radars, so `@nivo/radar`/`@nivo/core` stay out of the shell's initial bundle.
import { ResponsiveRadar } from '@nivo/radar';
import { getVizTheme } from './theme';

export interface CompareRadarSeries {
  /** the model name — also the nivo `key` for this series. */
  id: string;
  color: string;
}

export interface CompareRadarChartImplProps {
  /** one row per MINT dimension: `{ dimension: <label>, [seriesId]: normScore, ... }`. */
  data: Array<Record<string, string | number>>;
  series: CompareRadarSeries[];
}

export default function CompareRadarChartImpl({ data, series }: CompareRadarChartImplProps) {
  const theme = getVizTheme();
  const keys = series.map(s => s.id);
  const colors = series.map(s => s.color);
  return (
    <ResponsiveRadar
      data={data}
      keys={keys}
      indexBy="dimension"
      maxValue={1}
      margin={{ top: 40, right: 60, bottom: 24, left: 60 }}
      borderColor={{ from: 'color' }}
      gridShape="circular"
      gridLabelOffset={16}
      dotSize={6}
      dotBorderWidth={1}
      colors={colors}
      fillOpacity={0.18}
      blendMode="normal"
      theme={theme.nivo}
      valueFormat={v => (Math.round(v * 100) / 100).toString()}
      isInteractive
      animate={false}
    />
  );
}
