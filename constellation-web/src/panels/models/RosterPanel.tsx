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
import { deriveServingState, deriveCostTier, coverageBadges, fmtPct, fmtGb } from './modelsData';
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

/** Roster page size — the backend clamps `limit` to [1, 500]; 50 matches its default. */
const PAGE_SIZE = 50;

export function RosterPanel() {
  const [models, setModels] = useState<ModelListEntry[] | null>(null);
  // S127 (DATA-04): the SERVER's full-roster total (entries before pagination), reported as the
  // "Models" count instead of the loaded page size — so the metric shows the true scale, not 50.
  const [total, setTotal] = useState(0);
  const [refreshedAt, setRefreshedAt] = useState<string | null>(null);
  const [failed, setFailed] = useState(false);
  const [selected, setSelected] = useState<ModelListEntry | null>(null);

  // Filters. scope/serving/search are ALL server-side query params (models.list `q`), so they
  // filter the FULL roster, not just the loaded page — and `total`/pagination stay correct with a
  // search applied. `search` is the live input; `qParam` is its debounced value that drives the
  // fetch (so we don't hit the API on every keystroke).
  const [scope, setScope] = useState<Scope>('all');
  const [servingOnly, setServingOnly] = useState(false);
  const [search, setSearch] = useState('');
  const [qParam, setQParam] = useState('');
  // S127 (DATA-04): server-side pagination — the offset of the current page into the full roster.
  const [offset, setOffset] = useState(0);

  // Reset to the first page whenever a query-changing filter changes (a stale offset could point
  // past the new, smaller result set).
  const changeScope = (s: Scope) => { setScope(s); setOffset(0); };
  const changeServing = (on: boolean) => { setServingOnly(on); setOffset(0); };

  // FIX 3 (S127 review): debounce the search box into `qParam` and RESET the page — otherwise a
  // search on page N (offset 50) would filter only those 50 already-loaded rows and miss the rest
  // of the roster. Now the term goes to the backend `q` and paging restarts from the full result.
  useEffect(() => {
    const id = setTimeout(() => {
      setQParam(prev => {
        const next = search.trim();
        if (next !== prev) setOffset(0); // new search term → back to the first page
        return next;
      });
    }, 250);
    return () => clearTimeout(id);
  }, [search]);

  useEffect(() => {
    let cancelled = false;
    setModels(null);
    setFailed(false);
    const query: ModelsListQuery = { scope, limit: PAGE_SIZE, offset };
    if (servingOnly) query.serving = true;
    if (qParam) query.q = qParam; // server-side full-roster search
    getAggregationClient()
      .models.list(query)
      .then(res => { if (!cancelled) { setModels(res.models); setTotal(res.total); setRefreshedAt(res.refreshed_at); } })
      .catch(() => { if (!cancelled) { setModels([]); setTotal(0); setFailed(true); } });
    return () => { cancelled = true; };
  }, [scope, servingOnly, offset, qParam]);

  // The loaded page is already server-filtered by `qParam`; render it as-is (no client re-filter,
  // which previously scoped search to just the current page).
  const visible = models ?? [];

  const summary = useMemo(() => {
    const rows = models ?? [];
    return {
      // Full-roster count from the server (not the page size); serving/fleet are page-scoped.
      total,
      serving: rows.filter(m => m.serving_now).length,
      fleet: rows.filter(m => m.in_current_fleet).length,
    };
  }, [models, total]);

  const pageStart = total === 0 ? 0 : offset + 1;
  const pageEnd = Math.min(offset + (models?.length ?? 0), total);
  const hasPrev = offset > 0;
  const hasNext = offset + PAGE_SIZE < total;

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
                onClick={() => changeScope(s.id)}
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
          <input type="checkbox" checked={servingOnly} onChange={e => changeServing(e.target.checked)} />
          Serving only
        </label>
      </div>

      {/* S127 (DATA-04): pagination over the full server roster. `search` filters only the loaded
          page; the Prev/Next controls page the full set the "Models" metric counts. */}
      {total > PAGE_SIZE && (
        <div style={{ display: 'flex', alignItems: 'center', gap: 'var(--space-3)', flexWrap: 'wrap' }}>
          <span style={{ fontSize: 'var(--fs-sm)', color: 'var(--text-muted)', fontFamily: 'var(--font-mono)' }}>
            {pageStart}–{pageEnd} of {total}
          </span>
          <div style={{ display: 'inline-flex', gap: 'var(--space-1)' }}>
            <button
              type="button"
              onClick={() => setOffset(o => Math.max(0, o - PAGE_SIZE))}
              disabled={!hasPrev}
              style={{ ...selectStyle, cursor: hasPrev ? 'pointer' : 'not-allowed', opacity: hasPrev ? 1 : 0.5 }}
            >
              Prev
            </button>
            <button
              type="button"
              onClick={() => setOffset(o => o + PAGE_SIZE)}
              disabled={!hasNext}
              style={{ ...selectStyle, cursor: hasNext ? 'pointer' : 'not-allowed', opacity: hasNext ? 1 : 0.5 }}
            >
              Next
            </button>
          </div>
        </div>
      )}

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
