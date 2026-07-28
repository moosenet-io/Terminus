// LGUI-06 (§3.1): the Overview panel's Identity Card -- brand Card (glow when online),
// StatusPill (online/idle/error from status.state), uptime + version (mono), one Badge per
// channel (green=connected, neutral=configured-off, amber=misconfigured).
import { Card } from '../../components/Card';
import { StatusPill } from '../../components/StatusPill';
import type { PillState } from '../../components/StatusPill';
import { Badge } from '../../components/Badge';
import type { BadgeTone } from '../../components/Badge';
import { ChartSkeleton } from '../../viz/ChartSkeleton';
import type { LuminaStatus } from '../../types/lumina';

function formatUptime(secs: number): string {
  const d = Math.floor(secs / 86400);
  const h = Math.floor((secs % 86400) / 3600);
  const m = Math.floor((secs % 3600) / 60);
  const parts: string[] = [];
  if (d > 0) parts.push(`${d}d`);
  if (d > 0 || h > 0) parts.push(`${h}h`);
  parts.push(`${m}m`);
  return parts.join(' ');
}

/** Channel `state` free-form string -> Badge tone (§3.1: "green=connected, neutral=configured-
 *  off, amber=misconfigured"). Anything unrecognized falls back to neutral rather than guessing
 *  a more alarming tone -- an unknown state is not the same claim as a confirmed misconfig. */
function channelTone(state: string): BadgeTone {
  if (state === 'connected') return 'green';
  if (state === 'configured-off') return 'neutral';
  if (state === 'misconfigured') return 'amber';
  return 'neutral';
}

const STATUS_TO_PILL: Record<LuminaStatus['state'], PillState> = {
  online: 'online',
  idle: 'idle',
  error: 'error',
};

interface IdentityCardProps {
  status: LuminaStatus | null;
  loading: boolean;
  error: string | null;
}

export function IdentityCard({ status, loading, error }: IdentityCardProps) {
  if (loading) {
    return (
      <Card variant="content">
        <ChartSkeleton height={120} />
      </Card>
    );
  }

  if (error || !status) {
    return (
      <Card variant="content">
        <div style={{ color: 'var(--status-error)', fontSize: 'var(--fs-sm)' }}>
          Identity unavailable{error ? ` — ${error}` : ''}
        </div>
      </Card>
    );
  }

  const online = status.state === 'online';

  return (
    <Card variant="content" glow={online}>
      <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', gap: 'var(--space-3)', flexWrap: 'wrap' }}>
        <div style={{ display: 'flex', alignItems: 'baseline', gap: 'var(--space-2)' }}>
          <span style={{ fontFamily: 'Inter, sans-serif', fontWeight: 700, fontSize: 'var(--fs-h3)', color: 'var(--text-100)' }}>
            {status.display_name ?? 'Lumina'}
          </span>
          <span style={{ fontSize: 'var(--fs-sm)', color: 'var(--text-muted)' }}>assistant</span>
        </div>
        <StatusPill state={STATUS_TO_PILL[status.state]} />
      </div>

      <div
        style={{
          display: 'flex',
          gap: 'var(--space-4)',
          marginTop: 'var(--space-3)',
          fontFamily: 'var(--font-mono)',
          fontSize: 'var(--fs-mono-sm)',
          color: 'var(--text-muted)',
        }}
      >
        <span>uptime {formatUptime(status.uptime_secs)}</span>
        <span>v{status.version}</span>
        {!status.chord_configured && <span style={{ color: 'var(--status-warning)' }}>Chord not configured</span>}
      </div>

      <div style={{ display: 'flex', flexWrap: 'wrap', gap: 'var(--space-2)', marginTop: 'var(--space-3)' }}>
        {status.channels.map(ch => (
          <Badge key={ch.name} tone={channelTone(ch.state)} glowDot>
            {ch.name}
          </Badge>
        ))}
      </div>
    </Card>
  );
}
