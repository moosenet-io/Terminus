// CONST-17: Badge primitive per §2.3 — tone pill (~14% bg-tint + ~32% border + soft ink),
// optional glow dot, `mono` flag for cost/tier badges. Backed by the existing `.h-badge-*`
// classes in globals.css (kept so any code still using those classes directly restyles for
// free); this component is the typed, no-raw-hex entry point for new code.
export type BadgeTone = 'violet' | 'blue' | 'green' | 'amber' | 'rose' | 'neutral';

const TONE_CLASS: Record<BadgeTone, string> = {
  violet: 'h-badge-violet',
  blue: 'h-badge-blue',
  green: 'h-badge-green',
  amber: 'h-badge-amber',
  rose: 'h-badge-rose',
  neutral: 'h-badge-neutral',
};

const TONE_DOT: Record<BadgeTone, string> = {
  violet: 'var(--violet-400)',
  blue: 'var(--flux-blue)',
  green: 'var(--flux-green)',
  amber: 'var(--flux-amber)',
  rose: 'var(--flux-rose)',
  neutral: 'var(--text-400)',
};

interface BadgeProps {
  tone?: BadgeTone;
  children: React.ReactNode;
  /** Small glowing dot before the label — use only when the tone IS the semantic (§2.4). */
  glowDot?: boolean;
  /** JetBrains Mono rendering for cost/tier badges. */
  mono?: boolean;
  className?: string;
  style?: React.CSSProperties;
}

export function Badge({ tone = 'neutral', children, glowDot = false, mono = false, className, style }: BadgeProps) {
  return (
    <span
      className={`h-badge ${TONE_CLASS[tone]}${mono ? ' h-badge-mono' : ''}${className ? ` ${className}` : ''}`}
      style={style}
    >
      {glowDot && (
        <span
          aria-hidden
          style={{
            width: 6,
            height: 6,
            borderRadius: '50%',
            background: TONE_DOT[tone],
            boxShadow: `0 0 6px ${TONE_DOT[tone]}`,
            flexShrink: 0,
          }}
        />
      )}
      {children}
    </span>
  );
}
