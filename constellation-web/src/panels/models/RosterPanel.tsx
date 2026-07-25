// CGUI-09 (TERM #532): Models — Roster panel. The `models` module's primary surface.
// A master-detail: a searchable/filterable roster of models (master) that swaps to a per-model
// detail view (ModelDetailView) when one is opened.
//
// S127 TGUI2 POL-08/07/M6: adds a dense sortable TABLE view (name · backend · serving · category ·
// VRAM · best-pass · tier) alongside the existing rich card grid, toggled from a neutral segmented
// control; the density-first table is the default. The colored filter-chip row is replaced by the
// contextual Toolbar (search + neutral scope segments + serving toggle + view/count). Per-row tags
// (capabilities, cost tier, category) are now NEUTRAL outline chips — the only saturated token per
// row is the genuine serving status pill.
//
// Data wiring is unchanged: 100% through the CGUI-08 data client (`client.models.list()` /
// `client.models.model()`); scope/serving/search drive the server query, pagination is server-side
// (limit/offset) so `total` is the true roster scale. Sorting in the table view orders the loaded
// page (a note under the table makes that explicit). Fail-open: an empty roster renders a clean
// empty state; the panel never throws.
import { useEffect, useMemo, useState } from 'react';
import { Card, CardTitle } from '../../components/Card';
import { PanelRoot } from '../../components/PanelRoot';
import { Badge } from '../../components/Badge';
import { StatusPill } from '../../components/StatusPill';
import { MetricCard } from '../../components/MetricCard';
import { SortableTable } from '../../components/SortableTable';
import type { SortableColumn } from '../../components/SortableTable';
import { Toolbar, SearchInput, SegmentedControl, ResultCount } from '../../components/Toolbar';
import { getAggregationClient } from '../../lib/aggregationClient';
import type { ModelListEntry, ModelsListQuery } from '../../types/mint';
import { deriveServingState, deriveCostTier, coverageBadges, fmtPct, fmtGb } from './modelsData';
import { ModelDetailView } from './ModelDetailView';

type Scope = 'all' | 'fleet' | 'brochure';
type ViewMode = 'table' | 'cards';

const SCOPES = [
  { id: 'all' as const, label: 'All' },
  { id: 'fleet' as const, label: 'In Fleet' },
  { id: 'brochure' as const, label: 'Brochure' },
];

const VIEWS = [
  { id: 'table' as const, label: 'Table' },
  { id: 'cards' as const, label: 'Cards' },
];

/** Roster page size — the backend clamps `limit` to [1, 500]; 50 matches its default. */
const PAGE_SIZE = 50;

export function RosterPanel() {
  const [models, setModels] = useState<ModelListEntry[] | null>(null);
  const [total, setTotal] = useState(0);
  const [refreshedAt, setRefreshedAt] = useState<string | null>(null);
  const [failed, setFailed] = useState(false);
  const [selected, setSelected] = useState<ModelListEntry | null>(null);
  const [view, setView] = useState<ViewMode>('table');

  const [scope, setScope] = useState<Scope>('all');
  const [servingOnly, setServingOnly] = useState(false);
  const [search, setSearch] = useState('');
  const [qParam, setQParam] = useState('');
  const [offset, setOffset] = useState(0);

  const changeScope = (s: Scope) => { setScope(s); setOffset(0); };
  const changeServing = (on: boolean) => { setServingOnly(on); setOffset(0); };

  // Debounce the search box into `qParam` (server-side `q`) and reset paging on a new term.
  useEffect(() => {
    const id = setTimeout(() => {
      setQParam(prev => {
        const next = search.trim();
        if (next !== prev) setOffset(0);
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
    if (qParam) query.q = qParam;
    getAggregationClient()
      .models.list(query)
      .then(res => { if (!cancelled) { setModels(res.models); setTotal(res.total); setRefreshedAt(res.refreshed_at); } })
      .catch(() => { if (!cancelled) { setModels([]); setTotal(0); setFailed(true); } });
    return () => { cancelled = true; };
  }, [scope, servingOnly, offset, qParam]);

  const visible = models ?? [];

  const summary = useMemo(() => {
    const rows = models ?? [];
    return {
      total,
      serving: rows.filter(m => m.serving_now).length,
      fleet: rows.filter(m => m.in_current_fleet).length,
    };
  }, [models, total]);

  const pageStart = total === 0 ? 0 : offset + 1;
  const pageEnd = Math.min(offset + (models?.length ?? 0), total);
  const hasPrev = offset > 0;
  const hasNext = offset + PAGE_SIZE < total;

  const columns: SortableColumn<ModelListEntry>[] = useMemo(() => [
    {
      key: 'name', header: 'Model', sortable: true, sortValue: m => m.model_name, width: '28%',
      render: m => (
        <button
          type="button"
          onClick={() => setSelected(m)}
          style={{ all: 'unset', cursor: 'pointer', color: 'var(--text-100)', fontWeight: 600, wordBreak: 'break-all' }}
        >
          {m.model_name}
        </button>
      ),
    },
    {
      key: 'backend', header: 'Backend', sortable: true, sortValue: m => m.family ?? '',
      render: m => <span style={{ fontFamily: 'var(--font-mono)', fontSize: 'var(--fs-mono-sm)', color: 'var(--text-400)' }}>{m.family ?? '—'}</span>,
    },
    {
      key: 'serving', header: 'Serving', sortable: true, sortValue: m => (m.serving_now ? 2 : m.in_current_fleet ? 1 : 0),
      render: m => {
        const s = deriveServingState(m);
        return <StatusPill state={s.state} label={s.label} pulse={s.pulse} />;
      },
    },
    {
      key: 'category', header: 'Category', sortable: true, sortValue: m => m.category ?? '',
      render: m => (m.category ? <Badge tone="neutral" mono>{m.category}</Badge> : <span style={{ color: 'var(--text-500)' }}>—</span>),
    },
    {
      key: 'vram', header: 'VRAM', align: 'right', sortable: true, sortValue: m => m.vram_gb ?? -1,
      render: m => <span style={{ fontFamily: 'var(--font-mono)', fontSize: 'var(--fs-mono-sm)', color: 'var(--text-100)' }}>{fmtGb(m.vram_gb)}</span>,
    },
    {
      key: 'bestpass', header: 'Best pass', align: 'right', sortable: true, sortValue: m => m.best_pass_rate ?? -1,
      render: m => <span style={{ fontFamily: 'var(--font-mono)', fontSize: 'var(--fs-mono-sm)', color: 'var(--text-100)' }}>{fmtPct(m.best_pass_rate)}</span>,
    },
    {
      key: 'tier', header: 'Tier', align: 'right', sortable: true, sortValue: m => deriveCostTier(m).label,
      render: m => <Badge tone="neutral" mono>{deriveCostTier(m).label}</Badge>,
    },
  ], []);

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
      <CardTitle subtitle="Fleet ⋈ brochure ⋈ serving roster from the Terminus models API (CONST-21). Open a model for its dimension profile.">
        Models — Roster
      </CardTitle>

      <div style={{ display: 'grid', gridTemplateColumns: 'repeat(3, 1fr)', gap: 'var(--space-3)' }}>
        <MetricCard label="Models" value={models ? String(summary.total) : '—'} valueColor="accent" />
        <MetricCard label="Serving Now" value={models ? String(summary.serving) : '—'} valueColor="success" />
        <MetricCard label="In Current Fleet" value={models ? String(summary.fleet) : '—'} />
      </div>

      {/* Contextual toolbar — search + neutral scope segments + serving toggle; view + count right. */}
      <Toolbar
        right={
          <div style={{ display: 'inline-flex', alignItems: 'center', gap: 'var(--space-3)' }}>
            <ResultCount count={total} noun="model" />
            <SegmentedControl options={VIEWS} value={view} onChange={setView} ariaLabel="Roster view" />
          </div>
        }
      >
        <SearchInput value={search} onChange={setSearch} placeholder="Search name / family / category…" ariaLabel="Search models" />
        <SegmentedControl options={SCOPES} value={scope} onChange={changeScope} ariaLabel="Scope filter" />
        <label style={{ display: 'inline-flex', alignItems: 'center', gap: 'var(--space-1)', fontSize: 'var(--fs-sm)', color: 'var(--text-secondary)', cursor: 'pointer' }}>
          <input type="checkbox" checked={servingOnly} onChange={e => changeServing(e.target.checked)} />
          Serving only
        </label>
      </Toolbar>

      {/* Server-side pagination over the full roster. */}
      {total > PAGE_SIZE && (
        <div style={{ display: 'flex', alignItems: 'center', gap: 'var(--space-3)', flexWrap: 'wrap' }}>
          <span className="h-dtable-count">{pageStart}–{pageEnd} of {total}</span>
          <div style={{ display: 'inline-flex', gap: 'var(--space-1)' }}>
            <button type="button" className="h-pagebtn" onClick={() => setOffset(o => Math.max(0, o - PAGE_SIZE))} disabled={!hasPrev}>Prev</button>
            <button type="button" className="h-pagebtn" onClick={() => setOffset(o => o + PAGE_SIZE)} disabled={!hasNext}>Next</button>
          </div>
        </div>
      )}

      {refreshedAt && (
        <div style={{ fontSize: 'var(--fs-xs)', color: 'var(--text-muted)', fontFamily: 'var(--font-mono)' }}>
          refreshed {new Date(refreshedAt).toLocaleString()}
        </div>
      )}

      {/* Roster body — table (default, dense) or the rich card grid. */}
      {models === null ? (
        view === 'table' ? (
          <Card variant="content"><div className="h-skeleton" style={{ height: 220, borderRadius: 'var(--radius-lg)' }} /></Card>
        ) : (
          <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fill, minmax(16rem, 1fr))', gap: 'var(--space-3)' }}>
            {[1, 2, 3].map(i => <div key={i} className="h-skeleton" style={{ height: 150, borderRadius: 'var(--radius-lg)' }} />)}
          </div>
        )
      ) : visible.length === 0 ? (
        <Card variant="content">
          <div style={{ padding: 'var(--space-5)', textAlign: 'center', color: 'var(--text-muted)', fontSize: 'var(--fs-sm)' }}>
            {failed
              ? 'Models roster unavailable — the models API returned an error.'
              : 'No models match the current filters.'}
          </div>
        </Card>
      ) : view === 'table' ? (
        <>
          <Card variant="content" padding="var(--space-2)">
            <SortableTable
              columns={columns}
              rows={visible}
              rowKey={m => m.model_name}
              initialSort={{ key: 'serving', dir: 'desc' }}
              emptyMessage="No models match the current filters."
            />
          </Card>
          {total > PAGE_SIZE && (
            <div style={{ fontSize: 'var(--fs-xs)', color: 'var(--text-muted)' }}>
              Sorting orders this page ({pageStart}–{pageEnd}); use Prev/Next to page the full roster of {total}.
            </div>
          )}
        </>
      ) : (
        <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fill, minmax(16rem, 1fr))', gap: 'var(--space-3)' }}>
          {visible.map(m => {
            const serving = deriveServingState(m);
            const tier = deriveCostTier(m);
            const caps = coverageBadges(m);
            return (
              <Card key={m.model_name} variant="interactive" glow={m.serving_now} onClick={() => setSelected(m)}>
                <div style={{ display: 'flex', alignItems: 'flex-start', justifyContent: 'space-between', gap: 'var(--space-2)', marginBottom: 'var(--space-2)' }}>
                  <span style={{ fontWeight: 600, color: 'var(--text-primary)', wordBreak: 'break-all' }}>{m.model_name}</span>
                  <Badge tone="neutral" mono>{tier.label}</Badge>
                </div>
                <div style={{ display: 'flex', flexWrap: 'wrap', alignItems: 'center', gap: 'var(--space-2)', marginBottom: 'var(--space-3)' }}>
                  <StatusPill state={serving.state} label={serving.label} pulse={serving.pulse} />
                  {m.category && <Badge tone="neutral">{m.category}</Badge>}
                </div>
                {caps.length > 0 && (
                  <div style={{ display: 'flex', flexWrap: 'wrap', gap: 'var(--space-1)', marginBottom: 'var(--space-3)' }}>
                    {caps.map(c => <Badge key={c.key} tone="neutral">{c.label}</Badge>)}
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
