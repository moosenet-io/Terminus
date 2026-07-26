// CONST-16: shell wordmark (§2.2 of the CONST-GUI spec) — "Terminus." + the tracked-mono
// eyebrow ("LUMINA CONSTELLATION · WEB GUI SYSTEM"). CONST-17 lands the full brand token sheet
// (self-hosted Inter, the violet `--accent-bright` period, the exact ramp); this renders the
// same structure against today's tokens (`--accent-primary`) so that swap is value-only, no
// structural diff. Replaces the old Sidebar header block as the shell's one wordmark.
export function Wordmark() {
  return (
    <div style={{ display: 'flex', alignItems: 'center', gap: 'var(--space-2)', userSelect: 'none' }}>
      {/* CGUI-12 (§3.1): the wordmark leads with a violet node dot — the node-dot system is the
          brand's native icon language (§9), so the shell identity opens with a `core`-violet dot
          + "terminus". 9px + `0 0 8px` glow is intentional DS-parity dot geometry (matches the
          NodeBadge/StatusPill/card node-dots); the adherence-lint px warning on it is expected. */}
      <span
        aria-hidden
        title="terminus"
        style={{
          width: 9,
          height: 9,
          borderRadius: '50%',
          background: 'var(--node-core)',
          boxShadow: '0 0 8px var(--node-core)',
          flexShrink: 0,
        }}
      />
    <div style={{ display: 'flex', flexDirection: 'column', lineHeight: 1.15 }}>
      <span
        style={{
          fontFamily: 'var(--font-sans)',
          fontWeight: 700,
          fontSize: 18,
          letterSpacing: '-0.02em',
          color: 'var(--text-primary)',
        }}
      >
        terminus<span style={{ color: 'var(--accent-primary)' }}>.</span>
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
    </div>
  );
}
