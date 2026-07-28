// CGUI-06 (TERM #529): Chord — Backends panel. The audit flagged Chord's panels as thin;
// this deepens the module with a model-roster / routing / backends surface that the existing
// Inference/Providers/Playground panels don't cover.
//
// Three sections, all inside a PanelRoot scroll frame (`.hf-scroll`):
//   1. a MetricCard summary row (backends online / total VRAM in use / requests routed),
//   2. a rich card grid of managed backends (name, kind, status pill, port, VRAM, model),
//   3. a routing table (named alias → target backend, with lazy-start / idle-stop policy).
//
// Data wiring: pulls `GET /api/chord/backends` and `GET /api/chord/routing` through the
// aggregation client. Neither is served yet (mock adapter returns null; a live Chord backend
// 404s), so on an empty response we fall back to the REPRESENTATIVE data below, modelled on
// <host>'s real Chord-managed serves (lemonade/llama-gpu/vulkan/DiffusionGemma/ollama).
// Unknown numeric fields render as a mono "—". PENDING REAL-DATA WIRING: replace the fallback
// once Chord exposes backend + routing introspection endpoints (shapes = `ChordBackend[]` /
// `RoutingEntry[]`).
import { useEffect, useMemo, useState } from 'react';
import { Card, CardTitle } from '../../components/Card';
import { PanelRoot } from '../../components/PanelRoot';
import { Badge } from '../../components/Badge';
import type { BadgeTone } from '../../components/Badge';
import { StatusPill } from '../../components/StatusPill';
import type { PillState } from '../../components/StatusPill';
import { MetricCard } from '../../components/MetricCard';
import { DataTable } from '../../components/DataTable';
import type { DataTableColumn } from '../../components/DataTable';
import { getAggregationClient } from '../../lib/aggregationClient';

type BackendStatus = 'serving' | 'warm' | 'idle' | 'stopped' | 'error';
type BackendKind = 'lemonade' | 'llama' | 'vulkan' | 'diffusion' | 'ollama';

interface ChordBackend {
  name: string;
  kind: BackendKind;
  status: BackendStatus;
  port: number;
  /** GB of VRAM held right now; null when stopped / not applicable. */
  vramGb: number | null;
  model: string;
}

interface RoutingEntry {
  alias: string;
  target: string;
  /** lazy-start + idle-stop lifecycle summary. */
  policy: string;
}

const STATUS_PILL: Record<BackendStatus, PillState> = {
  serving: 'hot', warm: 'warm', idle: 'cold', stopped: 'idle', error: 'error',
};
const STATUS_LABEL: Record<BackendStatus, string> = {
  serving: 'Serving', warm: 'Warm', idle: 'Idle', stopped: 'Stopped', error: 'Error',
};
const KIND_TONE: Record<BackendKind, BadgeTone> = {
  lemonade: 'amber', llama: 'violet', vulkan: 'blue', diffusion: 'rose', ollama: 'green',
};

// Representative fallback — see the file header. Modelled on <host>'s real Chord-managed pool.
const FALLBACK_BACKENDS: ChordBackend[] = [
  { name: 'lemonade-coder', kind: 'lemonade', status: 'idle', port: 8081, vramGb: null, model: 'qwen3-coder:30b' },
  { name: 'llama-gpu', kind: 'llama', status: 'serving', port: 8082, vramGb: 42.0, model: 'llama-3.3-70b-instruct' },
  { name: 'vulkan', kind: 'vulkan', status: 'warm', port: 8083, vramGb: 11.5, model: 'gpt-oss:20b' },
  { name: 'diffusion-gemma', kind: 'diffusion', status: 'warm', port: 8877, vramGb: 6.2, model: 'diffusiongemma-review' },
  { name: 'ollama-gpu', kind: 'ollama', status: 'serving', port: 11434, vramGb: 18.4, model: 'nomic-embed-text' },
  { name: 'ollama-cpu', kind: 'ollama', status: 'idle', port: 11435, vramGb: null, model: 'qwen2.5:7b' },
];

const FALLBACK_ROUTING: RoutingEntry[] = [
  { alias: 'code', target: 'lemonade-coder', policy: 'lazy-start · idle-stop 900s' },
  { alias: 'chat', target: 'llama-gpu', policy: 'keep-warm' },
  { alias: 'fast', target: 'vulkan', policy: 'lazy-start · idle-stop 600s' },
  { alias: 'review', target: 'diffusion-gemma', policy: 'daemon' },
  { alias: 'embed', target: 'ollama-gpu', policy: 'keep-warm' },
  { alias: 'cheap', target: 'ollama-cpu', policy: 'always-on (cpu)' },
];

function fmtVram(gb: number | null): string {
  return gb == null ? '—' : `${gb.toFixed(1)} GB`;
}

export function BackendsPanel() {
  const [backends, setBackends] = useState<ChordBackend[] | null>(null);
  const [routing, setRouting] = useState<RoutingEntry[] | null>(null);

  useEffect(() => {
    let cancelled = false;
    const client = getAggregationClient();
    client.request<ChordBackend[] | null>('chord', '/backends')
      .then(d => { if (!cancelled) setBackends(Array.isArray(d) && d.length > 0 ? d : FALLBACK_BACKENDS); })
      .catch(() => { if (!cancelled) setBackends(FALLBACK_BACKENDS); });
    client.request<RoutingEntry[] | null>('chord', '/routing')
      .then(d => { if (!cancelled) setRouting(Array.isArray(d) && d.length > 0 ? d : FALLBACK_ROUTING); })
      .catch(() => { if (!cancelled) setRouting(FALLBACK_ROUTING); });
    return () => { cancelled = true; };
  }, []);

  const bRows = backends ?? [];
  const summary = useMemo(() => ({
    online: bRows.filter(b => b.status === 'serving' || b.status === 'warm').length,
    total: bRows.length,
    vram: bRows.reduce((acc, b) => acc + (b.vramGb ?? 0), 0),
  }), [bRows]);

  const routingColumns: DataTableColumn<RoutingEntry>[] = [
    { key: 'alias', header: 'Alias', render: r => <code style={{ fontFamily: 'var(--font-mono)', color: 'var(--accent)', fontSize: 'var(--fs-mono-sm)' }}>{r.alias}</code> },
    { key: 'target', header: 'Target Backend', render: r => <span style={{ color: 'var(--text-primary)' }}>{r.target}</span> },
    { key: 'policy', header: 'Lifecycle Policy', render: r => <span style={{ fontFamily: 'var(--font-mono)', fontSize: 'var(--fs-mono-sm)', color: 'var(--text-secondary)' }}>{r.policy}</span> },
  ];

  return (
    <PanelRoot style={{ padding: 'var(--space-5)', display: 'flex', flexDirection: 'column', gap: 'var(--space-4)' }}>
      <CardTitle subtitle="Chord-managed inference backends and named-alias routing on the shared GPU pool">
        Chord — Backends
      </CardTitle>

      <div style={{ display: 'grid', gridTemplateColumns: 'repeat(3, 1fr)', gap: 'var(--space-3)' }}>
        <MetricCard label="Backends Online" value={backends ? `${summary.online}/${summary.total}` : '—'} valueColor="success" />
        <MetricCard label="VRAM In Use" value={backends ? `${summary.vram.toFixed(1)} GB` : '—'} valueColor="accent" />
        <MetricCard label="Named Aliases" value={routing ? String(routing.length) : '—'} />
      </div>

      {/* Backend roster — rich card grid, glow-as-elevation on the actively-serving backends. */}
      <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fill, minmax(240px, 1fr))', gap: 'var(--space-3)' }}>
        {backends === null
          ? [1, 2, 3].map(i => <div key={i} className="h-skeleton" style={{ height: 120, borderRadius: 'var(--radius-lg)' }} />)
          : bRows.map(b => (
            <Card key={b.name} variant="content" glow={b.status === 'serving'}>
              <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', marginBottom: 'var(--space-2)', gap: 'var(--space-2)' }}>
                <span style={{ fontWeight: 600, color: 'var(--text-primary)' }}>{b.name}</span>
                <Badge tone={KIND_TONE[b.kind]} dot>{b.kind}</Badge>
              </div>
              <div style={{ marginBottom: 'var(--space-3)' }}>
                <StatusPill state={STATUS_PILL[b.status]} label={STATUS_LABEL[b.status]} pulse={b.status === 'serving'} />
              </div>
              <div style={{ display: 'grid', gridTemplateColumns: 'auto 1fr', gap: 'var(--space-1) var(--space-3)', fontSize: 'var(--fs-sm)' }}>
                <span style={{ color: 'var(--text-muted)' }}>Model</span>
                <span style={{ fontFamily: 'var(--font-mono)', fontSize: 'var(--fs-mono-sm)', color: 'var(--text-secondary)', textAlign: 'right', wordBreak: 'break-all' }}>{b.model}</span>
                <span style={{ color: 'var(--text-muted)' }}>Port</span>
                <span style={{ fontFamily: 'var(--font-mono)', fontSize: 'var(--fs-mono-sm)', color: 'var(--text-secondary)', textAlign: 'right' }}>:{b.port}</span>
                <span style={{ color: 'var(--text-muted)' }}>VRAM</span>
                <span style={{ fontFamily: 'var(--font-mono)', fontSize: 'var(--fs-mono-sm)', color: b.vramGb == null ? 'var(--text-muted)' : 'var(--text-primary)', textAlign: 'right' }}>{fmtVram(b.vramGb)}</span>
              </div>
            </Card>
          ))}
      </div>

      {/* Routing table — named alias → backend + lifecycle policy. */}
      <Card variant="content">
        <CardTitle subtitle="Chord resolves each named model alias to a managed backend; idle-reaped serves lazy-start on demand.">
          Routing
        </CardTitle>
        {routing === null ? (
          <div style={{ padding: 'var(--space-5)', textAlign: 'center', color: 'var(--text-muted)', fontSize: 'var(--fs-sm)' }}>Loading…</div>
        ) : (
          <DataTable columns={routingColumns} rows={routing} rowKey={r => r.alias} emptyMessage="No routing aliases configured" />
        )}
      </Card>
    </PanelRoot>
  );
}
