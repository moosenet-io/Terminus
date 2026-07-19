// CONST-04: ported from harmony-web — compact metric display, design tokens only.
// CONST-17: label restyled to the brand's tracked-mono eyebrow style (§2.3); value stays
// JetBrains Mono at --fs-h3.
import { Card, COLOR_MAP } from './Card';
import type { StatusColor } from './Card';

interface MetricCardProps {
  label: string;
  value: string;
  valueColor?: StatusColor;
  style?: React.CSSProperties;
}

export function MetricCard({ label, value, valueColor = 'primary', style }: MetricCardProps) {
  return (
    <Card variant="metric" style={style}>
      <div style={{
        fontFamily: 'var(--font-mono)',
        fontSize: 'var(--fs-mono-sm)',
        color: 'var(--text-400)',
        textTransform: 'uppercase',
        letterSpacing: 'var(--ls-label)',
        marginBottom: 'var(--space-1)',
        fontWeight: 'var(--fw-medium)',
      }}>
        {label}
      </div>
      <div style={{
        fontSize: 'var(--fs-h3)',
        fontWeight: 'var(--fw-semibold)',
        color: COLOR_MAP[valueColor],
        fontFamily: 'var(--font-mono)',
        lineHeight: 'var(--lh-tight)',
      }}>
        {value}
      </div>
    </Card>
  );
}
