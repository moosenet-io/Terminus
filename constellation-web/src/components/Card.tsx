// CONST-04: Card system, ported from harmony-web (design-token driven).
// CONST-17: restyled to the Terminus brand (§2.3) — gradient fill, violet hairline/glow,
// new `glow`/`accent` emphasis props. API kept: same 4 variants, same StatusColor union.
import { useState } from 'react';

type CardVariant = 'metric' | 'content' | 'interactive' | 'expandable';

/** Constrained color options — map to CSS tokens internally. No raw hex may bypass this. */
export type StatusColor = 'primary' | 'success' | 'warning' | 'error' | 'accent' | 'secondary' | 'tertiary';

export const COLOR_MAP: Record<StatusColor, string> = {
  primary:   'var(--text-primary)',
  secondary: 'var(--text-secondary)',
  tertiary:  'var(--text-tertiary)',
  success:   'var(--status-success)',
  warning:   'var(--status-warning)',
  error:     'var(--status-error)',
  accent:    'var(--accent-bright)',
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
  /** §2.3: persistent brand-emphasis glow. Reserve for live/primary elements (§2.4) —
   *  never ambient decoration. */
  glow?: boolean;
  /** §2.3: violet-gradient border-mask + strong hairline — emphasis without full glow. */
  accent?: boolean;
}

const baseCard: React.CSSProperties = {
  background: 'var(--grad-card)',
  border: '1px solid var(--border)',
  borderRadius: 'var(--radius-lg)',
  boxShadow: 'var(--shadow-md), var(--inset-hi)',
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
  glow = false,
  accent = false,
}: CardProps) {
  const [expanded, setExpanded] = useState(defaultExpanded);
  const emphasisStyle: React.CSSProperties = {
    ...(accent ? { borderColor: 'var(--border-strong)', boxShadow: 'var(--shadow-md), var(--glow-violet-soft), var(--inset-hi)' } : {}),
    ...(glow ? { boxShadow: 'var(--shadow-md), var(--glow-violet), var(--inset-hi)' } : {}),
  };

  if (variant === 'expandable') {
    return (
      <div className={className} style={{ ...baseCard, ...emphasisStyle, ...style }}>
        <div
          className="h-card-header"
          onClick={() => { setExpanded(e => !e); onClick?.(); }}
          style={{ transition: `background var(--dur-fast) var(--ease-out)` }}
        >
          <div style={{ flex: 1 }}>{header ?? children}</div>
          {/* Chevron only when there is a distinct header (i.e. a separate body to reveal).
              Without a header the children ARE the summary, so there is nothing to expand. */}
          {header != null && (
            <span style={{
              color: 'var(--text-tertiary)',
              fontSize: 'var(--fs-xs)',
              display: 'inline-block',
              transform: expanded ? 'rotate(180deg)' : 'none',
              transition: `transform var(--dur-fast) var(--ease-out)`,
              marginLeft: 'var(--space-2)',
            }}>▼</span>
          )}
        </div>
        {/* Body renders whenever there is a header; the .h-expandable-body class drives the
            collapse/expand transition via data-expanded so children never fail to render. */}
        {header != null && (
          <div className="h-expandable-body" data-expanded={expanded}
            style={{ borderTop: '1px solid var(--border)', padding: 'var(--space-3) var(--space-4)' }}>
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
        style={{ padding: paddingMap.interactive, ...emphasisStyle, ...style }}
      >
        {children}
      </div>
    );
  }

  return (
    <div
      className={`h-card${className ? ` ${className}` : ''}`}
      onClick={onClick}
      style={{ padding: paddingMap[variant], ...emphasisStyle, ...style }}
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
