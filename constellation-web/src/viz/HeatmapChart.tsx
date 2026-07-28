// CGUI-10 (TERM #533): nivo heatmap wrapper — the one place @nivo/heatmap is imported (§4.1).
// Also lazy-loaded via the reserved `viz` chunk. Renders a model × metric coverage matrix: the
// CELL COLOR encodes the unit capability score [0,1] on the brand sequential ramp (high = light
// so strong cells pop on deep space, §4.2), while the CELL LABEL shows the raw metric value in
// its native units — different metrics live on different scales, so coloring by a normalized
// quality is the honest choice. Not-run cells render as the deep empty color, never a false 0.
//
// The nivo datum carries only {x, y} (y = quality, drives color); the raw value shown as the
// label is looked up out-of-band by `serieId|x` to keep the datum shape strictly HeatMapDatum
// (avoids the ExtraProps index-signature friction in @nivo/heatmap's generics).
import { ResponsiveHeatMap } from '@nivo/heatmap';
import { getVizTheme } from './theme';
import { sequentialColor } from './palette';
import { metricLabel, formatMetricValue } from '../panels/mint/categoryMeta';
import type { HeatmapVM } from '../panels/mint/transforms';

interface HeatmapChartProps {
  vm: HeatmapVM;
  /** Maps a display column label back to its raw metric id (for value formatting). */
  metricIdByLabel: Record<string, string>;
}

export default function HeatmapChart({ vm, metricIdByLabel }: HeatmapChartProps) {
  const theme = getVizTheme();

  // Raw-value lookup keyed by `serieId|columnLabel` — the label accessor reads it so the datum
  // itself stays a plain {x, y}.
  const rawByKey = new Map<string, number | null>();
  const data = vm.models.map(model => ({
    id: model,
    data: vm.metrics.map(metric => {
      const label = metricLabel(metric);
      const c = vm.cell[model]?.[metric];
      rawByKey.set(`${model}|${label}`, c?.value ?? null);
      return { x: label, y: c?.quality ?? null };
    }),
  }));

  return (
    <ResponsiveHeatMap
      data={data}
      margin={{ top: 64, right: 24, bottom: 24, left: 140 }}
      valueFormat={() => ''}
      theme={theme.nivo}
      emptyColor="var(--space-500)"
      colors={cell => (cell.value == null ? 'var(--space-500)' : sequentialColor(cell.value))}
      label={cell => {
        const label = String(cell.data.x);
        const metricId = metricIdByLabel[label] ?? label;
        return formatMetricValue(metricId, rawByKey.get(`${cell.serieId}|${label}`) ?? null);
      }}
      labelTextColor="var(--text-100)"
      borderColor="var(--space-900)"
      borderWidth={1}
      axisTop={{ tickSize: 0, tickPadding: 8, tickRotation: -40 }}
      axisLeft={{ tickSize: 0, tickPadding: 8 }}
      animate={false}
    />
  );
}
