// CONST-04: ported unchanged from harmony-web — compact metric display, design tokens only.
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
        fontSize: 'var(--text-xs)',
        color: 'var(--text-secondary)',
        textTransform: 'uppercase',
        letterSpacing: '0.06em',
        marginBottom: 'var(--space-1)',
        fontWeight: 500,
      }}>
        {label}
      </div>
      <div style={{
        fontSize: 'var(--text-metric)',
        fontWeight: 600,
        color: COLOR_MAP[valueColor],
        fontFamily: 'var(--font-mono)',
        lineHeight: 1.1,
      }}>
        {value}
      </div>
    </Card>
  );
}
