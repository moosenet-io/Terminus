// CONST-28: Terminus module self — fleet health board. Per-system cards built from a
// client-held ring buffer of the last 120 `/api/health` polls (fleetRingBuffer.ts, unit-tested
// there), each with an uptime sparkline (viz kit), plus the broker/mesh summary pulled from
// `/api/terminus/config` (workerCount + module/tool totals).
import { useEffect, useRef, useState } from 'react';
import { Card, CardTitle } from '../../components/Card';
import { MetricCard } from '../../components/MetricCard';
import { StatusPill } from '../../components/StatusPill';
import { Sparkline } from '../../viz/Sparkline';
import { getAggregationClient } from '../../lib/aggregationClient';
import type { HealthStatus, SystemId, TerminusConfigSummary } from '../../lib/aggregationClient';
import {
  emptyFleetRingBuffers,
  pushHealthPoll,
  transitions,
  uptimeRatio,
  type FleetRingBuffers,
} from './fleetRingBuffer';

const POLL_INTERVAL_MS = 5000;
const KNOWN_SYSTEMS: SystemId[] = ['harmony', 'chord', 'lumina', 'terminus'];

function systemCard(system: SystemId, buffers: FleetRingBuffers) {
  const arr = buffers[system] ?? [];
  const uptime = uptimeRatio(buffers, system);
  const flapCount = Math.max(0, transitions(buffers, system).length - 1); // exclude the initial "into window" entry
  const current = arr.length > 0 ? arr[arr.length - 1] : null;
  const sparkData = arr.map(p => ({ t: p.t, v: p.available ? 1 : 0 }));

  return (
    <Card key={system} variant="content" style={{ display: 'flex', flexDirection: 'column', gap: 'var(--space-2)' }}>
      <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between' }}>
        <div style={{ fontFamily: 'var(--font-mono)', fontSize: 'var(--fs-mono-sm)', textTransform: 'uppercase', letterSpacing: 'var(--ls-label)', color: 'var(--text-400)' }}>
          {system}
        </div>
        {current ? (
          <StatusPill state={current.available ? 'online' : 'error'} label={current.available ? 'up' : 'down'} />
        ) : (
          <StatusPill state="idle" label="no data" />
        )}
      </div>

      <Sparkline data={sparkData} height={28} />

      <div style={{ display: 'flex', justifyContent: 'space-between', fontSize: 'var(--fs-xs)', color: 'var(--text-muted)' }}>
        <span>{uptime === null ? 'collecting…' : `${(uptime * 100).toFixed(0)}% uptime`}</span>
        <span>{arr.length === 0 ? '' : `${flapCount} transition${flapCount === 1 ? '' : 's'} · ${arr.length}/120 polls`}</span>
      </div>
    </Card>
  );
}

export function FleetPanel() {
  const [buffers, setBuffers] = useState<FleetRingBuffers>(emptyFleetRingBuffers());
  const [config, setConfig] = useState<TerminusConfigSummary | null>(null);
  const [configError, setConfigError] = useState<string | null>(null);
  const cancelledRef = useRef(false);

  useEffect(() => {
    cancelledRef.current = false;
    const client = getAggregationClient();

    async function pollHealth() {
      try {
        const health: HealthStatus[] = await client.health.list();
        if (!cancelledRef.current) {
          setBuffers(prev => pushHealthPoll(prev, health));
        }
      } catch {
        // A wholesale poll failure: leave the existing ring buffers untouched (spec edge
        // case — "health poll failing (keep last-known ring buffer content)"). Nothing to
        // push, no system's buffer changes.
      }
    }

    pollHealth();
    const id = setInterval(pollHealth, POLL_INTERVAL_MS);
    return () => { cancelledRef.current = true; clearInterval(id); };
  }, []);

  useEffect(() => {
    let cancelled = false;
    getAggregationClient()
      .terminus.configSummary()
      .then(c => { if (!cancelled) setConfig(c); })
      .catch(e => { if (!cancelled) setConfigError(e instanceof Error ? e.message : 'Failed to load'); });
    return () => { cancelled = true; };
  }, []);

  const workerCount = config?.workerCount ?? 0;
  const moduleCount = config?.modules.length ?? 0;
  const enabledCount = config?.modules.filter(m => m.enabled).length ?? 0;

  return (
    <div style={{ padding: 'var(--space-5)', display: 'flex', flexDirection: 'column', gap: 'var(--space-4)' }}>
      <CardTitle subtitle="Live per-system availability, uptime history, and mesh/broker summary">
        Terminus — Fleet
      </CardTitle>

      <div style={{ display: 'flex', gap: 'var(--space-3)', flexWrap: 'wrap' }}>
        <MetricCard label="Modules" value={String(moduleCount)} />
        <MetricCard label="Enabled" value={String(enabledCount)} valueColor="success" />
        {/* Edge case: an empty broker-routes table (nothing extracted to a worker yet) reads
            as "0 workers", never as an error/empty state — RouteTable's own contract is that
            an empty table is behavior-preserving (see handle_terminus_config's doc). */}
        <MetricCard
          label="Workers"
          value={workerCount === 0 ? '0 (in-process)' : String(workerCount)}
          valueColor="accent"
        />
      </div>

      {configError && (
        <Card variant="content">
          <span style={{ color: 'var(--status-error)' }}>{configError}</span>
        </Card>
      )}

      {/* Each card degrades on its own (StatusPill "no data" + Sparkline "collecting…") until
          its first poll lands — no separate page-level loading skeleton needed here. */}
      <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fill, minmax(220px, 1fr))', gap: 'var(--space-3)' }}>
        {KNOWN_SYSTEMS.map(s => systemCard(s, buffers))}
      </div>
    </div>
  );
}
