// S127 TGUI2 POL-10 (§3.6): the ONE branded empty/degraded state. Replaces the bare "No data"
// strings and lone spinners scattered across panels — a data panel that has nothing to show
// (unconfigured backend, http mode returning empty while the backend warms, a filtered-to-zero
// table) renders THIS instead of a blank area: a centered node-dot glyph + title + one-line
// reason + an optional action. Node-dot iconography (guide §9 — no emoji), tokens only.
import type { ReactNode } from 'react';
import { Button } from './Button';

export interface EmptyStateProps {
  /** Short title, e.g. "No tools mounted". */
  title: string;
  /** One-line reason / next step. */
  message?: ReactNode;
  /** Optional single action (label + handler) rendered as a small secondary button. */
  action?: { label: string; onClick: () => void };
  /** Node-dot tint (defaults to the muted core violet). Semantic callers may pass a status hue. */
  tone?: string;
  /** Compact variant for inline/in-card use (smaller glyph + tighter padding). */
  compact?: boolean;
}

export function EmptyState({ title, message, action, tone = 'var(--text-500)', compact }: EmptyStateProps) {
  const dot = compact ? 10 : 16;
  return (
    <div
      role="status"
      style={{
        display: 'flex',
        flexDirection: 'column',
        alignItems: 'center',
        justifyContent: 'center',
        textAlign: 'center',
        gap: 'var(--space-3)',
        padding: compact ? 'var(--space-5) var(--space-4)' : 'var(--space-8) var(--space-4)',
        minHeight: compact ? undefined : 180,
        color: 'var(--text-tertiary)',
      }}
    >
      {/* Node-dot glyph: a soft ring around a core dot — the DS's own icon language, not an emoji. */}
      <span
        aria-hidden
        style={{
          width: dot * 2.4,
          height: dot * 2.4,
          borderRadius: '50%',
          border: 'var(--border-width) solid var(--border-default)',
          display: 'flex',
          alignItems: 'center',
          justifyContent: 'center',
          flexShrink: 0,
        }}
      >
        <span style={{ width: dot, height: dot, borderRadius: '50%', background: tone, opacity: 0.85 }} />
      </span>
      <div style={{ display: 'flex', flexDirection: 'column', gap: 'var(--space-1)', maxWidth: 380 }}>
        <div style={{ color: 'var(--text-200)', fontWeight: 'var(--fw-semibold)', fontSize: 'var(--fs-body)' }}>
          {title}
        </div>
        {message && <div style={{ fontSize: 'var(--fs-sm)', lineHeight: 'var(--lh-body)' }}>{message}</div>}
      </div>
      {action && (
        <Button variant="secondary" size="sm" onClick={action.onClick}>
          {action.label}
        </Button>
      )}
    </div>
  );
}
