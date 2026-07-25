// CONST-17 / CGUI-01: StatusPill primitive reconciled EXACTLY to the DS StatusPill
// (_ds_bundle StatusPill.jsx §8): mono fs-mono-sm uppercase ls .06em on --space-700,
// NEUTRAL --text-200 text, a 7px STATE-colored dot with an 8px glow, and the optional
// lumina-ping expanding ring (inset -3px). Ping/pulse never carry meaning alone — the
// text label is always present (§2.6).
export type PillState = 'hot' | 'warm' | 'cold' | 'online' | 'idle' | 'error';

const STATE_COLOR: Record<PillState, string> = {
  hot: 'var(--tier-hot)',
  warm: 'var(--tier-warm)',
  cold: 'var(--tier-cold)',
  online: 'var(--flux-green)',
  idle: 'var(--text-400)',
  error: 'var(--flux-rose)',
};

// DS glow halos (separate soft-rgba values, per STATES[state].glow).
const STATE_GLOW: Record<PillState, string> = {
  hot: 'rgba(244, 63, 94, 0.5)',
  warm: 'rgba(245, 158, 11, 0.5)',
  cold: 'rgba(59, 130, 246, 0.5)',
  online: 'rgba(16, 185, 129, 0.5)',
  idle: 'rgba(107, 114, 128, 0.4)',
  error: 'rgba(244, 63, 94, 0.5)',
};

const STATE_LABEL: Record<PillState, string> = {
  hot: 'Hot', warm: 'Warm', cold: 'Cold', online: 'Online', idle: 'Idle', error: 'Error',
};

interface StatusPillProps {
  state: PillState;
  label?: string;
  /**
   * DS contract prop (§8 StatusPill): show the expanding ping ring.
   * DEVIATION from the DS literal default (`true`): defaults to `state === 'online'`
   * so existing call-sites keep their behavior and idle never pings (§2.6). Callers
   * that want a live pulse on warm/error/etc. pass `pulse` explicitly.
   */
  pulse?: boolean;
  style?: React.CSSProperties;
}

export function StatusPill({ state, label, pulse, style }: StatusPillProps) {
  const color = STATE_COLOR[state];
  const ping = pulse ?? state === 'online';
  return (
    <span
      style={{
        display: 'inline-flex',
        alignItems: 'center',
        gap: 7,
        fontFamily: 'var(--font-mono)',
        fontSize: 'var(--fs-mono-sm)',
        textTransform: 'uppercase',
        letterSpacing: '0.06em',
        color: 'var(--text-200)',
        background: 'var(--space-700)',
        border: '1px solid var(--border)',
        padding: '4px 11px 4px 9px',
        borderRadius: 'var(--radius-pill)',
        ...style,
      }}
    >
      <span style={{ position: 'relative', width: 7, height: 7, flexShrink: 0 }}>
        {ping && (
          <span
            aria-hidden
            className="lumina-ping"
            style={{
              position: 'absolute',
              inset: -3,
              borderRadius: '50%',
              background: color,
              opacity: 0.35,
            }}
          />
        )}
        <span
          aria-hidden
          style={{
            position: 'absolute',
            inset: 0,
            borderRadius: '50%',
            background: color,
            boxShadow: `0 0 8px ${STATE_GLOW[state]}`,
          }}
        />
      </span>
      {label ?? STATE_LABEL[state]}
    </span>
  );
}
