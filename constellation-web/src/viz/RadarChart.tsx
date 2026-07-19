// CONST-22: the viz kit's radar wrapper — no chart-form wrapper existed yet for
// `@nivo/radar` (CONST-17 shipped only the pinned package + shared theme bridge, §10
// CONST-17 / README "The viz kit"); this is the first consumer, added here rather than
// inline in a panel so "panels never import nivo directly" (§4.1/§9) stays mechanically
// true. Used by `models.detail`'s MINT-profile thumbnail (this model vs. fleet median) and
// `models.compare`'s radar overlay (≤4 series, §6.1).
import { ResponsiveRadar } from '@nivo/radar';
import { getVizTheme } from './theme';
import { CATEGORICAL_HEX } from './palette';

export interface RadarSeries {
  id: string;
  color: string;
}

interface RadarChartProps {
  /** One row per dimension: `{ [indexBy]: dimensionLabel, [seriesId]: normValue, ... }`. */
  data: Array<Record<string, string | number>>;
  indexBy: string;
  series: RadarSeries[];
  height: number;
}

/** Fixed-order categorical fallback — used only if a caller passes a series with no
 *  `color` resolved yet (shouldn't happen once SlotAssigner is wired, kept as a safety net
 *  so the chart never renders an undefined/transparent line). */
const FALLBACK_COLOR = CATEGORICAL_HEX[0];

export function RadarChart({ data, indexBy, series, height }: RadarChartProps) {
  const theme = getVizTheme();
  const keys = series.map(s => s.id);
  const colors = series.map(s => s.color || FALLBACK_COLOR);

  return (
    <div style={{ height }}>
      <ResponsiveRadar
        data={data}
        keys={keys}
        indexBy={indexBy}
        maxValue={1}
        margin={{ top: 24, right: 60, bottom: 24, left: 60 }}
        curve="linearClosed"
        borderWidth={2}
        borderColor={{ from: 'color' }}
        gridLevels={4}
        gridShape="circular"
        gridLabelOffset={16}
        enableDots={true}
        dotSize={8}
        dotBorderWidth={2}
        dotColor={{ theme: 'background' }}
        dotBorderColor={{ from: 'color' }}
        colors={colors}
        fillOpacity={0.12}
        blendMode="normal"
        motionConfig="none"
        theme={theme.nivo}
      />
    </div>
  );
}
