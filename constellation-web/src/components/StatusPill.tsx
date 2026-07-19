// CONST-17: StatusPill primitive per §2.3 — mono 11px uppercase pill on --space-700, 7px
// state-color dot with 8px glow + the lumina-ping expanding ring for 'online'. Idle = muted
// dot, NO ping (§2.6). Ping/pulse never carry meaning alone — the text label is always
// present, per §2.6.
export type PillState = 'online' | 'idle' | 'error' | 'hot' | 'warm' | 'cold';

const STATE_COLOR: Record<PillState, string> = {
  online: 'var(--flux-green)',
  idle: 'var(--text-400)',
  error: 'var(--flux-rose)',
  hot: 'var(--tier-hot)',
  warm: 'var(--tier-warm)',
  cold: 'var(--tier-cold)',
};

interface StatusPillProps {
  state: PillState;
  label?: string;
  style?: React.CSSProperties;
}

export function StatusPill({ state, label, style }: StatusPillProps) {
  const color = STATE_COLOR[state];
  const ping = state === 'online';
  return (
    <span
      style={{
        display: 'inline-flex',
        alignItems: 'center',
        gap: 6,
        fontFamily: 'var(--font-mono)',
        fontSize: 'var(--fs-mono-sm)',
        textTransform: 'uppercase',
        letterSpacing: 'var(--ls-label)',
        color,
        background: 'var(--space-700)',
        border: '1px solid var(--border)',
        padding: '2px 8px',
        borderRadius: 'var(--radius-pill)',
        ...style,
      }}
    >
      <span style={{ position: 'relative', width: 7, height: 7, flexShrink: 0 }}>
        <span
          aria-hidden
          style={{
            position: 'absolute',
            inset: 0,
            borderRadius: '50%',
            background: color,
            boxShadow: `0 0 8px ${color}`,
          }}
        />
        {ping && (
          <span
            aria-hidden
            className="lumina-ping"
            style={{
              position: 'absolute',
              inset: 0,
              borderRadius: '50%',
              background: color,
            }}
          />
        )}
      </span>
      {label ?? state}
    </span>
  );
}
