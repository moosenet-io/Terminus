// CONST-17 / CGUI-01: NodeBadge — the signature flow-node primitive, reconciled EXACTLY
// to the DS NodeBadge (_ds_bundle NodeBadge.jsx §8): kind-tinted gradient chip
// (soft → --space-800), kind-colored hairline, a 9px glowing kind-colored dot (the
// semantic-color law §2.4), bold mono name + muted sans role line. `pulse` breathes the
// core dot (lumina-corepulse). `kind` encodes directionality: source (inbound) /
// core (processing) / endpoint (outbound) / cloud (gated).
export type NodeKind = 'source' | 'core' | 'endpoint' | 'cloud';

interface KindStyle { color: string; soft: string; bd: string; glow: string; }

const KINDS: Record<NodeKind, KindStyle> = {
  source:   { color: 'var(--flux-blue)',  soft: 'rgba(59, 130, 246, 0.16)', bd: 'rgba(59, 130, 246, 0.45)', glow: 'var(--glow-blue)' },
  core:     { color: 'var(--violet-400)', soft: 'rgba(168, 85, 247, 0.14)', bd: 'var(--line-strong)',        glow: 'var(--glow-violet)' },
  endpoint: { color: 'var(--flux-green)', soft: 'rgba(16, 185, 129, 0.12)', bd: 'rgba(16, 185, 129, 0.40)', glow: 'var(--glow-green)' },
  cloud:    { color: 'var(--flux-amber)', soft: 'rgba(245, 158, 11, 0.12)', bd: 'rgba(245, 158, 11, 0.42)', glow: 'var(--glow-amber)' },
};

interface NodeBadgeProps {
  name: string;
  role?: string;
  kind?: NodeKind;
  /** DS contract prop (§8 NodeBadge): render the name in JetBrains Mono (default true). */
  mono?: boolean;
  /** Active-core emphasis — breathes the dot via lumina-corepulse (§2.3). */
  pulse?: boolean;
  style?: React.CSSProperties;
}

export function NodeBadge({ name, role, kind = 'core', mono = true, pulse = false, style }: NodeBadgeProps) {
  const k = KINDS[kind];
  return (
    <span
      style={{
        display: 'inline-flex',
        alignItems: 'center',
        gap: 10,
        background: `linear-gradient(180deg, ${k.soft}, var(--space-800))`,
        border: `1px solid ${k.bd}`,
        borderRadius: 'var(--radius-md)',
        // DS-exact component padding (§8 NodeBadge) + 9px dot below — intentional raw px,
        // NOT tokenized (tokenizing would break DS pixel-parity). adherence-lint warns expected.
        padding: '9px 14px',
        boxShadow: 'var(--shadow-sm), var(--inset-hi)',
        ...style,
      }}
    >
      <span
        aria-hidden
        style={{
          width: 9,
          height: 9,
          borderRadius: '50%',
          flex: 'none',
          background: k.color,
          boxShadow: k.glow,
          animation: pulse ? 'lumina-corepulse 3.2s var(--ease-in-out) infinite' : 'none',
        }}
      />
      <span style={{ display: 'flex', flexDirection: 'column', gap: 2, minWidth: 0 }}>
        <span style={{
          fontFamily: mono ? 'var(--font-mono)' : 'var(--font-sans)',
          fontSize: 'var(--fs-sm)',
          fontWeight: 'var(--fw-bold)',
          letterSpacing: 'var(--ls-mono)',
          color: 'var(--text-100)',
        }}>
          {name}
        </span>
        {role && (
          <span style={{ fontSize: 'var(--fs-xs)', color: 'var(--text-300)', lineHeight: 1.3 }}>
            {role}
          </span>
        )}
      </span>
    </span>
  );
}
