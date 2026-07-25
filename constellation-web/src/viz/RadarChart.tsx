// CGUI-09 (TERM #532): lazy boundary for the nivo radar. `RadarChartImpl` (and through it
// `@nivo/radar` + `@nivo/core`) is code-split into the reserved `viz` chunk and only fetched
// when a per-model detail actually renders a radar — the shell/roster never pay for it.
// A ChartSkeleton fills the frame while the chunk loads, so the fixed-height ChartCard body
// never collapses.
import { Suspense, lazy } from 'react';
import { ChartSkeleton } from './ChartSkeleton';
import type { RadarAxis } from '../panels/models/modelsData';

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
