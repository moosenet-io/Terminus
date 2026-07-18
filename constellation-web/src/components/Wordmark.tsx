// CONST-16: shell wordmark (§2.2 of the CONST-GUI spec) — "Terminus." + the tracked-mono
// eyebrow ("LUMINA CONSTELLATION · WEB GUI SYSTEM"). CONST-17 lands the full brand token sheet
// (self-hosted Inter, the violet `--accent-bright` period, the exact ramp); this renders the
// same structure against today's tokens (`--accent-primary`) so that swap is value-only, no
// structural diff. Replaces the old Sidebar header block as the shell's one wordmark.
export function Wordmark() {
  return (
    <div style={{ display: 'flex', flexDirection: 'column', lineHeight: 1.15, userSelect: 'none' }}>
      <span
        style={{
          fontFamily: 'var(--font-sans)',
          fontWeight: 700,
          fontSize: 18,
          letterSpacing: '-0.02em',
          color: 'var(--text-primary)',
        }}
      >
        Terminus<span style={{ color: 'var(--accent-primary)' }}>.</span>
      </span>
      <span
        style={{
          fontFamily: 'var(--font-mono)',
          fontSize: 10,
          letterSpacing: '0.18em',
          textTransform: 'uppercase',
          color: 'var(--text-tertiary)',
          display: 'flex',
          alignItems: 'center',
          gap: 4,
          whiteSpace: 'nowrap',
        }}
      >
        LUMINA CONSTELLATION
        <span
          aria-hidden
          style={{
            width: 4,
            height: 4,
            borderRadius: '50%',
            background: 'var(--accent-primary)',
            flexShrink: 0,
            display: 'inline-block',
          }}
        />
        <span style={{ color: 'var(--accent-primary)' }}>WEB GUI SYSTEM</span>
      </span>
    </div>
  );
}
