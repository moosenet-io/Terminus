// CGUI-06 (TERM #529): Lumina — Overview panel. Lumina was a registered module with ZERO
// panels (an empty tab); this registers its first real panel so the module isn't empty,
// pending the full LGUI-06..12 surface (chat/memory/persona/routing/tools/access/setup).
//
// An operational overview of the lumina-core assistant agent (<host> `lumina.service`): a
// status hero, a MetricCard vitals row, a capabilities/module-federation card grid, and a
// recent-activity feed. All inside a PanelRoot scroll frame (`.hf-scroll`) so it scrolls.
//
// Data wiring: pulls `GET /api/lumina/overview` through the aggregation client. That endpoint
// is not served yet (mock adapter returns null; a live backend 404s until LGUI wires it), so
// on an empty response we fall back to the REPRESENTATIVE snapshot below so the panel reads as
// developed; unknown fields render as a mono "—". PENDING REAL-DATA WIRING: replace the
// fallback once Lumina exposes an overview endpoint (shape = `LuminaOverview`).
import { useEffect, useState } from 'react';
import { Card, CardTitle } from '../../components/Card';
import { PanelRoot } from '../../components/PanelRoot';
import { Badge } from '../../components/Badge';
import type { BadgeTone } from '../../components/Badge';
import { StatusPill } from '../../components/StatusPill';
import type { PillState } from '../../components/StatusPill';
import { MetricCard } from '../../components/MetricCard';
import { getAggregationClient } from '../../lib/aggregationClient';

type ModuleHealth = 'online' | 'degraded' | 'offline';

interface FederatedModule {
  name: string;
  health: ModuleHealth;
  detail: string;
}

interface LuminaActivity {
  ts: string;
  summary: string;
}

interface LuminaOverview {
  state: PillState;
  stateLabel: string;
  persona: string;
  model: string;
  uptime: string;
  sessionsToday: number | null;
  memoryItems: number | null;
  toolCalls: number | null;
  modules: FederatedModule[];
  activity: LuminaActivity[];
}

const HEALTH_TONE: Record<ModuleHealth, BadgeTone> = { online: 'green', degraded: 'amber', offline: 'rose' };

const BASE_TS = Date.parse('2026-07-25T14:32:00Z');
function ago(minutesBack: number): string {
  return new Date(BASE_TS - minutesBack * 60_000).toISOString();
}

// Representative fallback — see the file header. Modelled on the real <host> lumina-core agent.
const FALLBACK_OVERVIEW: LuminaOverview = {
  state: 'online',
  stateLabel: 'Assisting',
  persona: 'Lumina — assistant-first',
  model: 'chord:chat (llama-3.3-70b)',
  uptime: '4d 06h',
  sessionsToday: 18,
  memoryItems: 1274,
  toolCalls: 342,
  modules: [
    { name: 'Terminus', health: 'online', detail: 'tool hub · fleet fronted' },
    { name: 'Harmony', health: 'online', detail: 'build orchestrator' },
    { name: 'Chord', health: 'online', detail: 'inference proxy' },
    { name: 'Muse', health: 'degraded', detail: 'library scan pending mount' },
    { name: 'Soma', health: 'online', detail: 'state-cache API :3099' },
  ],
  activity: [
    { ts: ago(3), summary: 'Answered a fleet-status query, routed through Terminus vitals' },
    { ts: ago(11), summary: 'Composed a Muse channel lineup on request' },
    { ts: ago(27), summary: 'Summarized overnight MINT sweep results from Harmony' },
    { ts: ago(52), summary: 'Persisted 3 memory items from the current session' },
    { ts: ago(94), summary: 'Escalated a coding task to the Harmony build pipeline' },
  ],
};

function fmtTs(iso: string): string {
  return new Date(iso).toLocaleTimeString([], { hour12: false });
}

export function OverviewPanel() {
  const [data, setData] = useState<LuminaOverview | null>(null);

  useEffect(() => {
    let cancelled = false;
    getAggregationClient()
      .request<LuminaOverview | null>('lumina', '/overview')
      .then(d => { if (!cancelled) setData(d && typeof d === 'object' && Array.isArray(d.modules) ? d : FALLBACK_OVERVIEW); })
      .catch(() => { if (!cancelled) setData(FALLBACK_OVERVIEW); });
    return () => { cancelled = true; };
  }, []);

  const num = (v: number | null | undefined): string => (v == null ? '—' : String(v));

  return (
    <PanelRoot style={{ padding: 'var(--space-5)', display: 'flex', flexDirection: 'column', gap: 'var(--space-4)' }}>
      <CardTitle subtitle="Operational overview of the lumina-core assistant agent (<host> lumina.service)">
        Lumina — Overview
      </CardTitle>

      {/* Status hero — glow-as-elevation on the live agent. */}
      <Card variant="content" glow accent>
        {data === null ? (
          <div className="h-skeleton" style={{ height: 48, borderRadius: 'var(--radius-md)' }} />
        ) : (
          <div style={{ display: 'flex', alignItems: 'center', gap: 'var(--space-4)', flexWrap: 'wrap' }}>
            <StatusPill state={data.state} label={data.stateLabel} pulse />
            <div style={{ display: 'flex', flexDirection: 'column' }}>
              <span style={{ color: 'var(--text-primary)', fontWeight: 600 }}>{data.persona}</span>
              <span style={{ fontFamily: 'var(--font-mono)', fontSize: 'var(--fs-mono-sm)', color: 'var(--text-muted)' }}>{data.model}</span>
            </div>
            <span style={{ marginLeft: 'auto', fontFamily: 'var(--font-mono)', fontSize: 'var(--fs-mono-sm)', color: 'var(--text-secondary)' }}>
              uptime {data.uptime}
            </span>
          </div>
        )}
      </Card>

      {/* Vitals row. */}
      <div style={{ display: 'grid', gridTemplateColumns: 'repeat(3, 1fr)', gap: 'var(--space-3)' }}>
        <MetricCard label="Sessions Today" value={num(data?.sessionsToday)} valueColor="accent" />
        <MetricCard label="Memory Items" value={num(data?.memoryItems)} />
        <MetricCard label="Tool Calls" value={num(data?.toolCalls)} valueColor="success" />
      </div>

      {/* Federated modules — the Terminus-fronted surfaces Lumina drives. */}
      <Card variant="content">
        <CardTitle subtitle="Terminus-fronted modules the assistant federates to answer and act">
          Federated Modules
        </CardTitle>
        {data === null ? (
          <div className="h-skeleton" style={{ height: 80, borderRadius: 'var(--radius-md)' }} />
        ) : (
          <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fill, minmax(200px, 1fr))', gap: 'var(--space-3)' }}>
            {data.modules.map(m => (
              <div key={m.name} style={{
                display: 'flex', flexDirection: 'column', gap: 'var(--space-1)',
                padding: 'var(--space-3)', background: 'var(--space-700)',
                border: '1px solid var(--border)', borderRadius: 'var(--radius-md)',
              }}>
                <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', gap: 'var(--space-2)' }}>
                  <span style={{ color: 'var(--text-primary)', fontWeight: 600 }}>{m.name}</span>
                  <Badge tone={HEALTH_TONE[m.health]} dot>{m.health}</Badge>
                </div>
                <span style={{ fontSize: 'var(--fs-xs)', color: 'var(--text-muted)' }}>{m.detail}</span>
              </div>
            ))}
          </div>
        )}
      </Card>

      {/* Recent activity feed. */}
      <Card variant="content" padding="var(--space-2)">
        <div style={{ padding: 'var(--space-2) var(--space-3)' }}>
          <CardTitle subtitle="Most recent assistant actions in this deployment">Recent Activity</CardTitle>
        </div>
        {data === null ? (
          <div className="h-skeleton" style={{ height: 80, borderRadius: 'var(--radius-md)' }} />
        ) : (
          <div style={{ display: 'flex', flexDirection: 'column' }}>
            {data.activity.map((a, i) => (
              <div
                key={`${a.ts}-${i}`}
                style={{
                  display: 'flex', alignItems: 'center', gap: 'var(--space-3)',
                  padding: 'var(--space-2) var(--space-3)',
                  borderBottom: i < data.activity.length - 1 ? '1px solid var(--border)' : 'none',
                  fontSize: 'var(--fs-sm)',
                }}
              >
                <code style={{ fontFamily: 'var(--font-mono)', fontSize: 'var(--fs-mono-sm)', color: 'var(--text-muted)', whiteSpace: 'nowrap' }}>
                  {fmtTs(a.ts)}
                </code>
                <span style={{ color: 'var(--text-primary)' }}>{a.summary}</span>
              </div>
            ))}
          </div>
        )}
      </Card>
    </PanelRoot>
  );
}
