// CONST-17: loading state for ChartCard — skeleton at final height, no spinner pages (§2.6).
interface ChartSkeletonProps {
  height: number;
}

export function ChartSkeleton({ height }: ChartSkeletonProps) {
  return <div className="h-skeleton" style={{ height, borderRadius: 'var(--radius-md)' }} />;
}
