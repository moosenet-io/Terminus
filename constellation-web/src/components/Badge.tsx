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

// DS dot color = the tone's foreground ink (Badge.jsx: `background: t.fg`).
const TONE_DOT: Record<BadgeTone, string> = {
  violet: 'var(--violet-300)',
  blue: 'var(--flux-blue-soft)',
  green: 'var(--flux-green-soft)',
  amber: 'var(--flux-amber)',
  rose: 'var(--flux-rose-soft)',
  neutral: 'var(--text-300)',
};

interface BadgeProps {
  tone?: BadgeTone;
  children: React.ReactNode;
  /** DS contract prop (§8 Badge): leading 6px glowing dot in the tone's ink. */
  dot?: boolean;
  /** @deprecated use `dot` — retained so existing callers don't break. */
  glowDot?: boolean;
  /** JetBrains Mono rendering for cost/tier badges. */
  mono?: boolean;
  className?: string;
  style?: React.CSSProperties;
}

export function Badge({ tone = 'neutral', children, dot, glowDot = false, mono = false, className, style }: BadgeProps) {
  const showDot = dot ?? glowDot;
  return (
    <span
      className={`h-badge ${TONE_CLASS[tone]}${mono ? ' h-badge-mono' : ''}${className ? ` ${className}` : ''}`}
      style={style}
    >
      {showDot && (
        <span
          aria-hidden
          style={{
            width: 6,
            height: 6,
            borderRadius: '50%',
            background: TONE_DOT[tone],
            boxShadow: `0 0 8px ${TONE_DOT[tone]}`,
            flexShrink: 0,
          }}
        />
      )}
      {children}
    </span>
  );
}
