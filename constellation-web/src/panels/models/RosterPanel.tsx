// CGUI-09 (TERM #532): Models — Roster panel. The `models` module's primary surface.
// A master-detail: a searchable/filterable grid of rich model cards (master) that swaps to a
// per-model detail view (ModelDetailView) when a card is opened, with a Roster back button.
//
// Data wiring: 100% through the CGUI-08 data client (`client.models.list()` /
// `client.models.model()`, GET /api/terminus/models[*], CONST-21 + CGUI-07). No bespoke fetch
// calls. In the deployed http adapter these are the LIVE fleet⋈brochure⋈serving⋈advisor rows;
// under the default mock adapter (VITE_AGG_MODE=mock) they are the deterministic CGUI-08
// fixtures, so the module builds and demos offline. scope/serving/status filters drive the
// query (server-side in both adapters); free-text search is additionally applied client-side
// for keystroke responsiveness.
//
// Fail-open: an empty roster (no profile data yet, or a degraded backend) renders a "no
// models" empty state; the panel never throws.
import { useEffect, useMemo, useState } from 'react';
import { Card, CardTitle } from '../../components/Card';
import { PanelRoot } from '../../components/PanelRoot';
import { Badge } from '../../components/Badge';
import { StatusPill } from '../../components/StatusPill';
import { MetricCard } from '../../components/MetricCard';
import { getAggregationClient } from '../../lib/aggregationClient';
import type { ModelListEntry, ModelsListQuery } from '../../types/mint';
import { deriveServingState, deriveCostTier, coverageBadges, matchesQuery, fmtPct, fmtGb } from './modelsData';
import { ModelDetailView } from './ModelDetailView';

type Scope = 'all' | 'fleet' | 'brochure';

const SCOPES: Array<{ id: Scope; label: string }> = [
  { id: 'all', label: 'All' },
  { id: 'fleet', label: 'In Fleet' },
  { id: 'brochure', label: 'Brochure' },
];

const selectStyle: React.CSSProperties = {
  background: 'var(--space-700)',
  border: '1px solid var(--border)',
  borderRadius: 'var(--radius-md)',
  color: 'var(--text-secondary)',
  fontFamily: 'var(--font-mono)',
  fontSize: 'var(--fs-mono-sm)',
  padding: 'var(--space-1) var(--space-2)',
};

export function RosterPanel() {
  const [models, setModels] = useState<ModelListEntry[] | null>(null);
  const [refreshedAt, setRefreshedAt] = useState<string | null>(null);
  const [failed, setFailed] = useState(false);
  const [selected, setSelected] = useState<ModelListEntry | null>(null);

  // Filters (scope/serving drive the query; search is client-side on top).
  const [scope, setScope] = useState<Scope>('all');
  const [servingOnly, setServingOnly] = useState(false);
  const [search, setSearch] = useState('');

  useEffect(() => {
    let cancelled = false;
    setModels(null);
    setFailed(false);
    const query: ModelsListQuery = { scope };
    if (servingOnly) query.serving = true;
    getAggregationClient()
      .models.list(query)
      .then(res => { if (!cancelled) { setModels(res.models); setRefreshedAt(res.refreshed_at); } })
      .catch(() => { if (!cancelled) { setModels([]); setFailed(true); } });
    return () => { cancelled = true; };
  }, [scope, servingOnly]);

  const visible = useMemo(
    () => (models ?? []).filter(m => matchesQuery(m, search)),
    [models, search],
  );

  const summary = useMemo(() => {
    const rows = models ?? [];
    return {
      total: rows.length,
      serving: rows.filter(m => m.serving_now).length,
      fleet: rows.filter(m => m.in_current_fleet).length,
    };
  }, [models]);

  // Detail view — master/detail swap inside the same panel/route.
  if (selected) {
    return (
      <PanelRoot style={{ padding: 'var(--space-5)' }}>
        <ModelDetailView entry={selected} onBack={() => setSelected(null)} />
      </PanelRoot>
    );
  }

  return (
    <PanelRoot style={{ padding: 'var(--space-5)', display: 'flex', flexDirection: 'column', gap: 'var(--space-4)' }}>
      <CardTitle subtitle="Fleet ⋈ brochure ⋈ serving roster from the Terminus models API (CONST-21). Open a card for its dimension profile.">
        Models — Roster
      </CardTitle>

      <div style={{ display: 'grid', gridTemplateColumns: 'repeat(3, 1fr)', gap: 'var(--space-3)' }}>
        <MetricCard label="Models" value={models ? String(summary.total) : '—'} valueColor="accent" />
        <MetricCard label="Serving Now" value={models ? String(summary.serving) : '—'} valueColor="success" />
        <MetricCard label="In Current Fleet" value={models ? String(summary.fleet) : '—'} />
      </div>

      {/* Filter bar — search + scope + serving toggle. */}
      <div style={{ display: 'flex', flexWrap: 'wrap', alignItems: 'center', gap: 'var(--space-3)' }}>
        <input
          type="text"
          value={search}
          onChange={e => setSearch(e.target.value)}
          placeholder="Search name / family / category…"
          aria-label="Search models"
          style={{ ...selectStyle, flex: '1 1 220px', minWidth: 0 }}
        />
        <div style={{ display: 'inline-flex', gap: 'var(--space-1)' }} role="group" aria-label="Scope filter">
          {SCOPES.map(s => {
            const active = scope === s.id;
            return (
              <button
                key={s.id}
                type="button"
                onClick={() => setScope(s.id)}
                style={{
                  ...selectStyle,
                  cursor: 'pointer',
                  color: active ? 'var(--text-primary)' : 'var(--text-muted)',
                  borderColor: active ? 'var(--accent)' : 'var(--border)',
                  background: active ? 'var(--space-600)' : 'var(--space-700)',
                }}
              >
                {s.label}
              </button>
            );
          })}
        </div>
        <label style={{ display: 'inline-flex', alignItems: 'center', gap: 'var(--space-1)', fontSize: 'var(--fs-sm)', color: 'var(--text-secondary)', cursor: 'pointer' }}>
          <input type="checkbox" checked={servingOnly} onChange={e => setServingOnly(e.target.checked)} />
          Serving only
        </label>
      </div>

      {refreshedAt && (
        <div style={{ fontSize: 'var(--fs-xs)', color: 'var(--text-muted)', fontFamily: 'var(--font-mono)' }}>
          refreshed {new Date(refreshedAt).toLocaleString()}
        </div>
      )}

      {/* Roster grid. */}
      {models === null ? (
        <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fill, minmax(260px, 1fr))', gap: 'var(--space-3)' }}>
          {[1, 2, 3].map(i => <div key={i} className="h-skeleton" style={{ height: 150, borderRadius: 'var(--radius-lg)' }} />)}
        </div>
      ) : visible.length === 0 ? (
        <Card variant="content">
          <div style={{ padding: 'var(--space-5)', textAlign: 'center', color: 'var(--text-muted)', fontSize: 'var(--fs-sm)' }}>
            {failed
              ? 'Models roster unavailable — the models API returned an error.'
              : 'No models match the current filters.'}
          </div>
        </Card>
      ) : (
        <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fill, minmax(260px, 1fr))', gap: 'var(--space-3)' }}>
          {visible.map(m => {
            const serving = deriveServingState(m);
            const tier = deriveCostTier(m);
            const caps = coverageBadges(m);
            return (
              <Card key={m.model_name} variant="interactive" glow={m.serving_now} onClick={() => setSelected(m)}>
                <div style={{ display: 'flex', alignItems: 'flex-start', justifyContent: 'space-between', gap: 'var(--space-2)', marginBottom: 'var(--space-2)' }}>
                  <span style={{ fontWeight: 600, color: 'var(--text-primary)', wordBreak: 'break-all' }}>{m.model_name}</span>
                  <Badge tone={tier.tone} mono>{tier.label}</Badge>
                </div>
                <div style={{ display: 'flex', flexWrap: 'wrap', alignItems: 'center', gap: 'var(--space-2)', marginBottom: 'var(--space-3)' }}>
                  <StatusPill state={serving.state} label={serving.label} pulse={serving.pulse} />
                  {m.category && <Badge tone="neutral">{m.category}</Badge>}
                </div>
                {caps.length > 0 && (
                  <div style={{ display: 'flex', flexWrap: 'wrap', gap: 'var(--space-1)', marginBottom: 'var(--space-3)' }}>
                    {caps.map(c => <Badge key={c.key} tone="violet" dot>{c.label}</Badge>)}
                  </div>
                )}
                <div style={{ display: 'grid', gridTemplateColumns: 'auto 1fr', gap: 'var(--space-1) var(--space-3)', fontSize: 'var(--fs-sm)' }}>
                  <span style={{ color: 'var(--text-muted)' }}>Backend</span>
                  <span style={{ fontFamily: 'var(--font-mono)', fontSize: 'var(--fs-mono-sm)', color: 'var(--text-secondary)', textAlign: 'right' }}>{m.family ?? '—'}</span>
                  <span style={{ color: 'var(--text-muted)' }}>VRAM</span>
                  <span style={{ fontFamily: 'var(--font-mono)', fontSize: 'var(--fs-mono-sm)', color: 'var(--text-primary)', textAlign: 'right' }}>{fmtGb(m.vram_gb)}</span>
                  <span style={{ color: 'var(--text-muted)' }}>Best pass</span>
                  <span style={{ fontFamily: 'var(--font-mono)', fontSize: 'var(--fs-mono-sm)', color: 'var(--text-primary)', textAlign: 'right' }}>{fmtPct(m.best_pass_rate)}</span>
                </div>
              </Card>
            );
          })}
        </div>
      )}
    </PanelRoot>
  );
}
