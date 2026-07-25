// S127 TGUI2 POL-11 (§4): the always-visible global health + spend chip, promoted from the
// rail-footer "all systems · $0.00" into the GlobalBar. A compact status pill: a semantic dot
// (green healthy / amber degraded / rose down) + a plain-language label + the fleet spend
// (always $0.00 — local-inference-first). Folds in the old standalone poll-degraded triangle:
// a failed health poll or any degraded/unavailable subsystem elevates the chip to amber and
// names the count, so degradation is legible at a glance rather than a broken glyph.
import type { HealthStatus } from '../lib/aggregationClient';

export interface HealthChipProps {
  health: HealthStatus[];
  /** healthSystem ids currently inside the stale-while-degrading grace window (App.tsx). */
  degradedSystems: Set<string>;
  /** True when the last health poll failed outright (network/backend unreachable). */
  pollDegraded: boolean;
}

type Level = 'ok' | 'warn' | 'down';

const HUE: Record<Level, string> = {
  ok: 'var(--status-success)',
  warn: 'var(--status-warning)',
  down: 'var(--status-error)',
};

export function HealthChip({ health, degradedSystems, pollDegraded }: HealthChipProps) {
  const known = health.length;
  const unavailable = health.filter(h => h.available === false).length;
  const degraded = degradedSystems.size;

  let level: Level;
  let label: string;
  if (pollDegraded) {
    level = 'warn';
    label = 'health poll degraded';
  } else if (unavailable > 0) {
    level = 'down';
    label = `${unavailable} system${unavailable === 1 ? '' : 's'} down`;
  } else if (degraded > 0) {
    level = 'warn';
    label = `${degraded} degraded`;
  } else if (known === 0) {
    level = 'warn';
    label = 'no systems reported';
  } else {
    level = 'ok';
    label = 'all systems';
  }

  const hue = HUE[level];
  const title = pollDegraded
    ? 'Health poll degraded — showing last known status'
    : `${known - unavailable}/${known} systems available · $0.00 spend today`;

  return (
    <div
      role="status"
      aria-label={`${label}, $0.00 spend today`}
      title={title}
      style={{
        display: 'flex',
        alignItems: 'center',
        gap: 'var(--space-2)',
        padding: 'var(--space-1) var(--space-3)',
        borderRadius: 'var(--radius-pill)',
        border: 'var(--border-width) solid var(--border-default)',
        background: 'var(--bg-surface)',
        fontFamily: 'var(--font-mono)',
        fontSize: 'var(--fs-mono-sm)',
        letterSpacing: 'var(--ls-mono)',
        color: 'var(--text-300)',
        whiteSpace: 'nowrap',
        flexShrink: 0,
      }}
    >
      <span
        aria-hidden
        style={{
          width: 7,
          height: 7,
          borderRadius: '50%',
          background: hue,
          // Glow only on the single hero status dot (M3) — a soft cue, not ambient neon.
          boxShadow: `0 0 6px ${hue}`,
          flexShrink: 0,
        }}
      />
      <span style={{ color: level === 'ok' ? 'var(--text-300)' : hue }}>{label}</span>
      <span aria-hidden style={{ color: 'var(--text-500)' }}>·</span>
      <span style={{ color: 'var(--flux-green-soft)' }}>$0.00</span>
    </div>
  );
}
