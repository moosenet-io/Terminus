// SPOL-04: ProgressBar — shared progress bar component using design tokens.
// Track: 6px height, --radius-sm rounded ends, token fill colors.
// Fill: gradient with subtle lightening at leading edge.

interface ProgressBarProps {
  pct: number;       // 0-100
  height?: number;   // px, default 6
  style?: React.CSSProperties;
}

function fillColor(pct: number): string {
  if (pct >= 80) return 'var(--status-success)';
  if (pct >= 20) return 'var(--status-warning)';
  return 'var(--status-error)';
}

function fillGradient(pct: number): string {
  const base = fillColor(pct);
  // Subtle leading-edge lighter stop gives depth without raw hex
  return `linear-gradient(90deg, ${base} 0%, color-mix(in srgb, ${base} 80%, white) 100%)`;
}

export function ProgressBar({ pct, height = 6, style }: ProgressBarProps) {
  const clamped = Math.max(0, Math.min(100, pct));
  return (
    <div
      style={{
        position: 'relative',
        height,
        borderRadius: 'var(--radius-sm)',
        background: 'var(--border-subtle)',
        overflow: 'hidden',
        ...style,
      }}
    >
      {clamped > 0 && (
        <div
          style={{
            position: 'absolute',
            inset: 0,
            width: `${clamped}%`,
            borderRadius: 'var(--radius-sm)',
            background: fillGradient(clamped),
            transition: `width var(--transition-default)`,
          }}
        />
      )}
    </div>
  );
}

/** Colour-only helper for pct — useful in labels */
export function pctColor(pct: number): string {
  return fillColor(pct);
}
