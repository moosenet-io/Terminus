// CONST-17: NodeBadge — the signature flow node primitive (§2.3). Kind-colored glowing 9px
// dot (source/core/endpoint/cloud — the semantic-color law, §2.4) + bold mono name + muted
// role line, kind-tinted gradient chip. Optional `pulse` for the active core (lumina-corepulse).
export type NodeKind = 'source' | 'core' | 'endpoint' | 'cloud';

const KIND_COLOR: Record<NodeKind, string> = {
  source: 'var(--node-source)',
  core: 'var(--node-core)',
  endpoint: 'var(--node-endpoint)',
  cloud: 'var(--node-cloud)',
};

interface NodeBadgeProps {
  kind: NodeKind;
  name: string;
  role?: string;
  /** Active-core emphasis (lumina-corepulse, §2.3) — reserve for the one live/primary node. */
  pulse?: boolean;
  style?: React.CSSProperties;
}

export function NodeBadge({ kind, name, role, pulse = false, style }: NodeBadgeProps) {
  const color = KIND_COLOR[kind];
  return (
    <span
      className={pulse ? 'lumina-corepulse' : undefined}
      style={{
        display: 'inline-flex',
        alignItems: 'center',
        gap: 8,
        padding: '6px 10px',
        borderRadius: 'var(--radius-md)',
        background: `linear-gradient(135deg, color-mix(in srgb, ${color} 18%, var(--space-700)), var(--space-700))`,
        border: '1px solid var(--border)',
        ...style,
      }}
    >
      <span
        aria-hidden
        style={{
          width: 9,
          height: 9,
          borderRadius: '50%',
          background: color,
          boxShadow: `0 0 8px ${color}`,
          flexShrink: 0,
        }}
      />
      <span style={{ display: 'flex', flexDirection: 'column', lineHeight: 'var(--lh-tight)' }}>
        <span style={{ fontFamily: 'var(--font-mono)', fontWeight: 'var(--fw-bold)', fontSize: 'var(--fs-sm)', color: 'var(--text-100)' }}>
          {name}
        </span>
        {role && (
          <span style={{ fontFamily: 'var(--font-mono)', fontSize: 'var(--fs-mono-sm)', color: 'var(--text-muted)' }}>
            {role}
          </span>
        )}
      </span>
    </span>
  );
}
