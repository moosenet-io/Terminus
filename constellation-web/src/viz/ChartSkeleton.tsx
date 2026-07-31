// CONST-17: loading state for ChartCard — skeleton at final height, no spinner pages (§2.6).
interface ChartSkeletonProps {
  /** MGUI-18: any CSS length (see ChartEmpty) — the skeleton must occupy the SAME box the
   *  loaded content will, or the page reflows under the operator when data lands. */
  height: number | string;
}

export function ChartSkeleton({ height }: ChartSkeletonProps) {
  return <div className="h-skeleton" style={{ height, borderRadius: 'var(--radius-md)' }} />;
}
