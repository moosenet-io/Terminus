// CONST-22 (reconciled): lazy Suspense boundary for CompareRadarChartImpl, matching the same
// code-split pattern RadarChart.tsx and the MINT report panel use for their nivo radars — the
// `models.compare` panel (and the models module generally) never pays for the `@nivo/radar`
// chunk until a compare view actually renders one.
import { Suspense, lazy } from 'react';
import { ChartSkeleton } from './ChartSkeleton';
import type { CompareRadarSeries } from './CompareRadarChartImpl';

export type { CompareRadarSeries };

const CompareRadarChartImpl = lazy(() => import('./CompareRadarChartImpl'));

export interface CompareRadarChartProps {
  data: Array<Record<string, string | number>>;
  series: CompareRadarSeries[];
  /** matches the enclosing ChartCard body height so the skeleton is the same size. */
  height: number;
}

export function CompareRadarChart({ data, series, height }: CompareRadarChartProps) {
  return (
    <Suspense fallback={<ChartSkeleton height={height} />}>
      <CompareRadarChartImpl data={data} series={series} />
    </Suspense>
  );
}
