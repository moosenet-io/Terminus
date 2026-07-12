// CONST-04: ported unchanged from harmony-web — loading skeleton, shapes match content.
interface SkeletonProps {
  variant?: 'bar' | 'circle' | 'card';
  width?: string | number;
  height?: string | number;
  style?: React.CSSProperties;
}

export function Skeleton({ variant = 'bar', width, height, style }: SkeletonProps) {
  const base: React.CSSProperties = {
    display: 'block',
    width: width ?? '100%',
    height: height ?? (variant === 'circle' ? 32 : variant === 'card' ? 80 : 16),
    borderRadius: variant === 'circle'
      ? '50%'
      : variant === 'card'
        ? 'var(--radius-lg)'
        : 'var(--radius-sm)',
  };

  return <span className="h-skeleton" style={{ ...base, ...style }} />;
}

/** Convenience: a few skeleton bars mimicking a list */
export function SkeletonList({ rows = 3 }: { rows?: number }) {
  return (
    <div style={{ display: 'flex', flexDirection: 'column', gap: 'var(--space-2)' }}>
      {Array.from({ length: rows }).map((_, i) => (
        <Skeleton key={i} variant="bar" height={14} width={`${85 - i * 8}%`} />
      ))}
    </div>
  );
}
