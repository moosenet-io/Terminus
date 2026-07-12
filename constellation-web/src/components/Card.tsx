// CONST-04: Card system, ported unchanged from harmony-web (design-token driven, no
// Harmony-specific branding baked in). Four variants: metric | content | interactive | expandable.
import { useState } from 'react';

type CardVariant = 'metric' | 'content' | 'interactive' | 'expandable';

/** Constrained color options — map to CSS tokens internally */
export type StatusColor = 'primary' | 'success' | 'warning' | 'error' | 'accent' | 'secondary' | 'tertiary';

export const COLOR_MAP: Record<StatusColor, string> = {
  primary:   'var(--text-primary)',
  secondary: 'var(--text-secondary)',
  tertiary:  'var(--text-tertiary)',
  success:   'var(--status-success)',
  warning:   'var(--status-warning)',
  error:     'var(--status-error)',
  accent:    'var(--accent-primary)',
};

interface CardProps {
  variant?: CardVariant;
  children: React.ReactNode;
  onClick?: () => void;
  className?: string;
  style?: React.CSSProperties;
  /** Expandable: content shown collapsed (summary). children shown when expanded. */
  header?: React.ReactNode;
  defaultExpanded?: boolean;
}

const baseCard: React.CSSProperties = {
  background: 'var(--bg-surface)',
  border: '1px solid var(--border-subtle)',
  borderRadius: 'var(--radius-lg)',
  boxShadow: 'var(--shadow-card)',
  overflow: 'hidden',
};

const paddingMap: Record<'metric' | 'content' | 'interactive', string> = {
  metric:      'var(--space-3)',
  content:     'var(--space-4)',
  interactive: 'var(--space-4)',
};

export function Card({
  variant = 'content',
  children,
  onClick,
  className,
  style,
  header,
  defaultExpanded = false,
}: CardProps) {
  const [expanded, setExpanded] = useState(defaultExpanded);

  if (variant === 'expandable') {
    return (
      <div className={className} style={{ ...baseCard, ...style }}>
        <div
          className="h-card-header"
          onClick={() => { setExpanded(e => !e); onClick?.(); }}
          style={{ transition: `background var(--transition-fast)` }}
        >
          <div style={{ flex: 1 }}>{header ?? children}</div>
          <span style={{
            color: 'var(--text-tertiary)',
            fontSize: 'var(--text-xs)',
            display: 'inline-block',
            transform: expanded ? 'rotate(180deg)' : 'none',
            transition: `transform var(--transition-fast)`,
            marginLeft: 'var(--space-2)',
          }}>▼</span>
        </div>
        {expanded && header && (
          <div style={{ borderTop: '1px solid var(--border-subtle)', padding: 'var(--space-3) var(--space-4)' }}>
            {children}
          </div>
        )}
      </div>
    );
  }

  if (variant === 'interactive') {
    return (
      <div
        className={`h-card-interactive${className ? ` ${className}` : ''}`}
        onClick={onClick}
        style={{ padding: paddingMap.interactive, ...style }}
      >
        {children}
      </div>
    );
  }

  return (
    <div
      className={`h-card${className ? ` ${className}` : ''}`}
      onClick={onClick}
      style={{ padding: paddingMap[variant], ...style }}
    >
      {children}
    </div>
  );
}

/** Consistent card title + optional subtitle */
export function CardTitle({ children, subtitle }: { children: React.ReactNode; subtitle?: string }) {
  return (
    <div style={{ marginBottom: 'var(--space-3)' }}>
      <div style={{ fontSize: 'var(--text-md)', fontWeight: 600, color: 'var(--text-primary)' }}>
        {children}
      </div>
      {subtitle && (
        <div style={{ fontSize: 'var(--text-sm)', color: 'var(--text-secondary)', marginTop: 'var(--space-1)' }}>
          {subtitle}
        </div>
      )}
    </div>
  );
}
