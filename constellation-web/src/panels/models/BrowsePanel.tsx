// CONST-22: `models.browse` (`/models`) — spec §6.1. Header stat row, ONE filter row above
// the whole grid (dataviz rule: filters never live inside a chart/table card), DataTable
// (default) with a card-grid toggle, row-click -> detail, checkbox-select (max 4) -> Compare.
import { useMemo, useState } from 'react';
import { useNavigate } from 'react-router-dom';
import { Card } from '../../components/Card';
import { MetricCard } from '../../components/MetricCard';
import { Badge } from '../../components/Badge';
import type { BadgeTone } from '../../components/Badge';
import { DataTable } from '../../components/DataTable';
import type { DataTableColumn } from '../../components/DataTable';
import { Button } from '../../components/Button';
import { SkeletonList } from '../../components/Skeleton';
import { useModelsList, useModelsSummary, toggleSelection, compareUrl } from '../../hooks/useModels';
import type {
  ModelListItem,
  ModelsScope,
  ModelCategory,
  BrochureStatus,
  SizeBucket,
  CoverageCells,
  CoverageState,
} from '../../types/models';

const CATEGORIES: ModelCategory[] = ['coder', 'assistant', 'agent', 'reasoning', 'vision', 'embedding', 'creative', 'tool-use'];
const BROCHURE_STATUSES: BrochureStatus[] = ['discovered', 'evaluating', 'evaluated', 'shortlisted', 'adopted', 'deprecated', 'rejected', 'archived'];
const SIZE_BUCKETS: SizeBucket[] = ['<4B', '4-10B', '10-35B', '>35B'];
const COVERAGE_DIMS: (keyof CoverageCells)[] = ['coder', 'assistant', 'serving', 'agent'];
const PAGE_SIZE = 50;

const selectStyle: React.CSSProperties = {
  padding: 'var(--space-2) var(--space-3)',
  borderRadius: 'var(--radius-md)',
  border: '1px solid var(--border)',
  background: 'var(--bg-elevated)',
  color: 'var(--text-100)',
  fontSize: 'var(--fs-sm)',
  fontFamily: 'var(--font-sans)',
};

const BROCHURE_TONE: Record<BrochureStatus, BadgeTone> = {
  discovered: 'neutral',
  evaluating: 'blue',
  evaluated: 'blue',
  shortlisted: 'violet',
  adopted: 'green',
  deprecated: 'amber',
  rejected: 'rose',
  archived: 'neutral',
};

const COVERAGE_COLOR: Record<CoverageState, string> = {
  covered: 'var(--flux-green)',
  partial: 'var(--flux-amber)',
  none: 'var(--space-500)',
};

function CoverageStrip({ coverage }: { coverage: CoverageCells }) {
  return (
    <div style={{ display: 'flex', gap: 3 }} title={COVERAGE_DIMS.map(d => `${d}: ${coverage[d]}`).join(' · ')}>
      {COVERAGE_DIMS.map(d => (
        <span
          key={d}
          aria-hidden
          style={{ width: 9, height: 9, borderRadius: 2, background: COVERAGE_COLOR[coverage[d]] }}
        />
      ))}
    </div>
  );
}

/** discovery_score sparkbar — a single filled bar, not a chart (no ChartCard needed for a
 *  one-value inline indicator; §4 chart standards govern actual charts, not table cells). */
function ScoreSparkbar({ score }: { score: number | undefined }) {
  if (score == null) return <span style={{ color: 'var(--text-faint)' }}>—</span>;
  const pct = Math.round(score * 100);
  return (
    <div style={{ display: 'flex', alignItems: 'center', gap: 6, minWidth: 72 }}>
      <div style={{ flex: 1, height: 5, borderRadius: 'var(--radius-sm)', background: 'var(--space-500)', overflow: 'hidden' }}>
        <div style={{ width: `${pct}%`, height: '100%', background: 'var(--grad-accent)' }} />
      </div>
      <span style={{ fontVariantNumeric: 'tabular-nums', fontSize: 'var(--fs-xs)', color: 'var(--text-muted)' }}>{pct}</span>
    </div>
  );
}

function scopeEmptyHint(scope: ModelsScope): string {
  if (scope === 'brochure') return 'brochure is empty — run model_discovery_refresh';
  if (scope === 'fleet') return 'no profiled fleet models match this filter';
  return 'no models match this filter';
}

export function BrowsePanel() {
  const navigate = useNavigate();
  const [scope, setScope] = useState<ModelsScope>('all');
  const [q, setQ] = useState('');
  const [category, setCategory] = useState<ModelCategory | ''>('');
  const [brochureStatus, setBrochureStatus] = useState<BrochureStatus | ''>('');
  const [sizeBucket, setSizeBucket] = useState<SizeBucket | ''>('');
  const [coverage, setCoverage] = useState<keyof CoverageCells | ''>('');
  const [servingOnly, setServingOnly] = useState(false);
  const [view, setView] = useState<'table' | 'card'>('table');
  const [offset, setOffset] = useState(0);
  const [selected, setSelected] = useState<string[]>([]);
  const [toast, setToast] = useState<string | null>(null);

  const params = useMemo(() => ({
    scope, q: q || undefined, category: category || undefined,
    brochure_status: brochureStatus || undefined, size_bucket: sizeBucket || undefined,
    coverage: coverage || undefined, serving: servingOnly || undefined,
    limit: PAGE_SIZE, offset,
  }), [scope, q, category, brochureStatus, sizeBucket, coverage, servingOnly, offset]);

  const { data, loading, isRefetching, error } = useModelsList(params);
  const { summary, loading: summaryLoading } = useModelsSummary();

  const models = data?.models ?? [];
  const total = data?.total ?? 0;

  function resetToFirstPage<T>(setter: (v: T) => void) {
    return (v: T) => { setter(v); setOffset(0); };
  }

  function showToast(msg: string) {
    setToast(msg);
    window.setTimeout(() => setToast(null), 3200);
  }

  function handleToggleSelect(name: string) {
    const { next, rejected } = toggleSelection(selected, name, 4);
    setSelected(next);
    if (rejected) showToast('Compare is limited to 4 models — deselect one first.');
  }

  const refreshedAmber = summary?.refreshedAt
    ? (Date.now() - new Date(summary.refreshedAt).getTime()) / 86_400_000 > 7
    : false;

  const columns: DataTableColumn<ModelListItem>[] = [
    {
      key: 'select', header: '', render: (r) => (
        <input
          type="checkbox"
          checked={selected.includes(r.model_name)}
          onClick={e => e.stopPropagation()}
          onChange={() => handleToggleSelect(r.model_name)}
          aria-label={`Select ${r.model_name} for compare`}
        />
      ),
    },
    {
      key: 'model', header: 'Model', render: (r) => (
        <span style={{ fontFamily: 'var(--font-mono)', color: 'var(--text-100)', fontWeight: 500 }}>{r.model_name}</span>
      ),
    },
    {
      key: 'family', header: 'Family/Params', render: (r) => (
        <span>{r.family ?? '—'}{r.params_b != null ? ` · ${r.params_b}B` : ''}</span>
      ),
    },
    { key: 'quant', header: 'Quant', render: (r) => r.quant ?? <span style={{ color: 'var(--text-faint)' }}>—</span> },
    { key: 'category', header: 'Category', render: (r) => r.category ?? '—' },
    {
      key: 'status', header: 'Status', render: (r) => (
        r.brochure_status
          ? <Badge tone={BROCHURE_TONE[r.brochure_status]}>{r.brochure_status}</Badge>
          : r.in_current_fleet
            ? <Badge tone="green">fleet</Badge>
            : <span style={{ color: 'var(--text-faint)' }}>—</span>
      ),
    },
    { key: 'coverage', header: 'Coverage', render: (r) => <CoverageStrip coverage={r.coverage} /> },
    {
      key: 'pass_rate', header: 'Best pass-rate', align: 'right', render: (r) => (
        r.best_pass_rate != null ? `${Math.round(r.best_pass_rate * 100)}%` : <span style={{ color: 'var(--text-faint)' }}>—</span>
      ),
    },
    { key: 'vram', header: 'VRAM', align: 'right', render: (r) => r.vram_gb != null ? `${r.vram_gb.toFixed(1)} GB` : '—' },
    {
      key: 'last_run', header: 'Last run', render: (r) => (
        r.last_run_at ? new Date(r.last_run_at).toLocaleDateString() : <span style={{ color: 'var(--text-faint)' }}>never</span>
      ),
    },
    { key: 'score', header: 'Discovery score', render: (r) => <ScoreSparkbar score={r.discovery_score} /> },
  ];

  return (
    <div style={{ display: 'flex', flexDirection: 'column', gap: 'var(--space-4)', padding: 'var(--space-5)', overflowY: 'auto', height: '100%' }}>
      <div>
        <h1 style={{ fontSize: 'var(--fs-h2)', color: 'var(--text-100)', margin: 0 }}>Model Library</h1>
        <p style={{ color: 'var(--text-muted)', fontSize: 'var(--fs-sm)', marginTop: 4 }}>
          Fleet catalog joined with the HF discovery brochure. Read-only — curation stays in the MCP tools.
        </p>
      </div>

      {/* Header stat row (§6.1) */}
      <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(160px, 1fr))', gap: 'var(--space-3)' }}>
        {summaryLoading || !summary ? (
          <SkeletonList rows={1} />
        ) : (
          <>
            <MetricCard label="Fleet models" value={String(summary.fleetCount)} />
            <MetricCard label="Brochure candidates" value={String(summary.brochureCount)} />
            <MetricCard label="Serving now" value={String(summary.servingNowCount)} valueColor="success" />
            <MetricCard
              label="Catalog refreshed"
              value={summary.refreshedAt ? new Date(summary.refreshedAt).toLocaleDateString() : '—'}
              valueColor={refreshedAmber ? 'warning' : 'primary'}
            />
          </>
        )}
      </div>

      {/* ONE filter row above everything it scopes (dataviz rule) */}
      <Card variant="content">
        <div style={{ display: 'flex', flexWrap: 'wrap', gap: 'var(--space-3)', alignItems: 'center' }}>
          <div style={{ display: 'inline-flex', gap: 2, background: 'var(--space-800)', borderRadius: 'var(--radius-sm)', padding: 2 }}>
            {(['fleet', 'brochure', 'all'] as ModelsScope[]).map(s => (
              <button
                key={s} type="button" aria-pressed={scope === s}
                onClick={() => resetToFirstPage(setScope)(s)}
                style={{
                  fontFamily: 'var(--font-mono)', fontSize: 'var(--fs-mono-sm)', textTransform: 'uppercase',
                  letterSpacing: 'var(--ls-label)', padding: '4px 12px', borderRadius: 'var(--radius-xs)',
                  border: 'none', cursor: 'pointer',
                  background: scope === s ? 'var(--grad-accent)' : 'transparent',
                  color: scope === s ? 'var(--accent-on)' : 'var(--text-muted)',
                }}
              >
                {s}
              </button>
            ))}
          </div>

          <input
            type="search"
            placeholder="Search model or family…"
            value={q}
            onChange={e => resetToFirstPage(setQ)(e.target.value)}
            style={{ ...selectStyle, minWidth: 200 }}
          />

          <select value={category} onChange={e => resetToFirstPage(setCategory)(e.target.value as ModelCategory | '')} style={selectStyle}>
            <option value="">All categories</option>
            {CATEGORIES.map(c => <option key={c} value={c}>{c}</option>)}
          </select>

          <select value={brochureStatus} onChange={e => resetToFirstPage(setBrochureStatus)(e.target.value as BrochureStatus | '')} style={selectStyle}>
            <option value="">Any brochure status</option>
            {BROCHURE_STATUSES.map(s => <option key={s} value={s}>{s}</option>)}
          </select>

          <select value={sizeBucket} onChange={e => resetToFirstPage(setSizeBucket)(e.target.value as SizeBucket | '')} style={selectStyle}>
            <option value="">Any size</option>
            {SIZE_BUCKETS.map(b => <option key={b} value={b}>{b}</option>)}
          </select>

          <select value={coverage} onChange={e => resetToFirstPage(setCoverage)(e.target.value as keyof CoverageCells | '')} style={selectStyle}>
            <option value="">Any coverage</option>
            {COVERAGE_DIMS.map(d => <option key={d} value={d}>{d} covered</option>)}
          </select>

          <label style={{ display: 'inline-flex', alignItems: 'center', gap: 6, fontSize: 'var(--fs-sm)', color: 'var(--text-body)' }}>
            <input type="checkbox" checked={servingOnly} onChange={e => resetToFirstPage(setServingOnly)(e.target.checked)} />
            Serving now
          </label>

          <div style={{ marginLeft: 'auto', display: 'flex', gap: 'var(--space-2)', alignItems: 'center' }}>
            <div style={{ display: 'inline-flex', gap: 2, background: 'var(--space-800)', borderRadius: 'var(--radius-sm)', padding: 2 }}>
              {(['table', 'card'] as const).map(v => (
                <button
                  key={v} type="button" aria-pressed={view === v} onClick={() => setView(v)}
                  style={{
                    fontFamily: 'var(--font-mono)', fontSize: 'var(--fs-mono-sm)', textTransform: 'uppercase',
                    letterSpacing: 'var(--ls-label)', padding: '4px 10px', borderRadius: 'var(--radius-xs)',
                    border: 'none', cursor: 'pointer',
                    background: view === v ? 'var(--grad-accent)' : 'transparent',
                    color: view === v ? 'var(--accent-on)' : 'var(--text-muted)',
                  }}
                >
                  {v}
                </button>
              ))}
            </div>
            <Button
              variant="primary" size="sm"
              disabled={selected.length < 2}
              onClick={() => navigate(compareUrl(selected))}
            >
              Compare ({selected.length})
            </Button>
          </div>
        </div>
      </Card>

      <Card variant="content" style={{ opacity: isRefetching ? 0.6 : 1, transition: 'opacity var(--dur-base) var(--ease-out)' }}>
        {error ? (
          <div style={{ padding: 'var(--space-5)', textAlign: 'center', color: 'var(--status-error)' }}>
            Failed to load models — {error}
          </div>
        ) : loading ? (
          <div style={{ padding: 'var(--space-4)' }}><SkeletonList rows={6} /></div>
        ) : models.length === 0 ? (
          <div style={{ padding: 'var(--space-6)', textAlign: 'center', color: 'var(--text-muted)' }}>
            <div style={{ fontSize: 'var(--fs-sm)' }}>No models found</div>
            <div style={{ fontSize: 'var(--fs-xs)', color: 'var(--text-faint)', marginTop: 4 }}>{scopeEmptyHint(scope)}</div>
          </div>
        ) : view === 'table' ? (
          <DataTable
            columns={columns}
            rows={models}
            rowKey={r => r.model_name}
            onRowClick={r => navigate(`/models/${encodeURIComponent(r.model_name)}`)}
          />
        ) : (
          <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fill, minmax(220px, 1fr))', gap: 'var(--space-3)' }}>
            {models.map(m => (
              <Card
                key={m.model_name}
                variant="interactive"
                onClick={() => navigate(`/models/${encodeURIComponent(m.model_name)}`)}
              >
                <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'flex-start' }}>
                  <span style={{ fontFamily: 'var(--font-mono)', color: 'var(--text-100)', fontWeight: 500 }}>{m.model_name}</span>
                  {m.brochure_status && <Badge tone={BROCHURE_TONE[m.brochure_status]}>{m.brochure_status}</Badge>}
                </div>
                <div style={{ fontSize: 'var(--fs-xs)', color: 'var(--text-muted)', marginTop: 4 }}>
                  {m.family ?? '—'}{m.params_b != null ? ` · ${m.params_b}B` : ''}{m.quant ? ` · ${m.quant}` : ''}
                </div>
                {m.category && (
                  <div style={{ marginTop: 4 }}>
                    <Badge tone="neutral">{m.category}</Badge>
                  </div>
                )}
                <div style={{ marginTop: 'var(--space-2)' }}>
                  <CoverageStrip coverage={m.coverage} />
                </div>
                {/* Card-grid keeps the same core field set as the table (§6.1: "the alternate
                    view keeps the core field set") — Best pass-rate/VRAM/Last run as compact
                    mono lines, not the full DataTable columns. */}
                <div
                  style={{
                    display: 'grid', gridTemplateColumns: 'repeat(3, 1fr)', gap: 'var(--space-2)',
                    marginTop: 'var(--space-2)', fontSize: 'var(--fs-xs)', fontFamily: 'var(--font-mono)',
                  }}
                >
                  <div>
                    <div style={{ color: 'var(--text-faint)' }}>pass-rate</div>
                    <div style={{ color: 'var(--text-body)', fontVariantNumeric: 'tabular-nums' }}>
                      {m.best_pass_rate != null ? `${Math.round(m.best_pass_rate * 100)}%` : '—'}
                    </div>
                  </div>
                  <div>
                    <div style={{ color: 'var(--text-faint)' }}>vram</div>
                    <div style={{ color: 'var(--text-body)', fontVariantNumeric: 'tabular-nums' }}>
                      {m.vram_gb != null ? `${m.vram_gb.toFixed(1)}G` : '—'}
                    </div>
                  </div>
                  <div>
                    <div style={{ color: 'var(--text-faint)' }}>last run</div>
                    <div style={{ color: 'var(--text-body)' }}>
                      {m.last_run_at ? new Date(m.last_run_at).toLocaleDateString() : 'never'}
                    </div>
                  </div>
                </div>
                <div style={{ marginTop: 'var(--space-2)' }}>
                  <ScoreSparkbar score={m.discovery_score} />
                </div>
              </Card>
            ))}
          </div>
        )}
      </Card>

      {/* Server-side pagination (§6.1) */}
      {total > PAGE_SIZE && (
        <div style={{ display: 'flex', justifyContent: 'center', gap: 'var(--space-3)', alignItems: 'center' }}>
          <Button variant="ghost" size="sm" disabled={offset === 0} onClick={() => setOffset(Math.max(0, offset - PAGE_SIZE))}>
            ← Prev
          </Button>
          <span style={{ fontSize: 'var(--fs-xs)', color: 'var(--text-muted)', fontVariantNumeric: 'tabular-nums' }}>
            {offset + 1}–{Math.min(offset + PAGE_SIZE, total)} of {total}
          </span>
          <Button variant="ghost" size="sm" disabled={offset + PAGE_SIZE >= total} onClick={() => setOffset(offset + PAGE_SIZE)}>
            Next →
          </Button>
        </div>
      )}

      {/* Minimal ephemeral toast for the "5th selection" rejection (§6/CONST-22 edge case) —
          no shared Toast primitive exists yet in this app (it's CONST-25/26 scope), so this is
          a self-contained, local-only banner rather than a new shared component. */}
      {toast && (
        <div
          role="status"
          style={{
            position: 'fixed', bottom: 'var(--space-5)', left: '50%', transform: 'translateX(-50%)',
            background: 'var(--bg-elevated)', border: '1px solid var(--border-strong)',
            borderRadius: 'var(--radius-md)', boxShadow: 'var(--shadow-lg)',
            padding: 'var(--space-3) var(--space-4)', color: 'var(--text-100)', fontSize: 'var(--fs-sm)',
            zIndex: 1000,
          }}
        >
          {toast}
        </div>
      )}
    </div>
  );
}
