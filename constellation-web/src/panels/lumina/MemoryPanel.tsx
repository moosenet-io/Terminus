// LGUI-08 (§3.3 "lumina.memory — Engram browser", route `/lumina/memory`, min role operator
// per §2's panel table). v1 is READ-ONLY end to end — no delete/edit affordance anywhere in
// this file (an acceptance criterion; keep it true).
import { useMemo, useState } from 'react';
import { Card, CardTitle } from '../../components/Card';
import { MetricCard } from '../../components/MetricCard';
import { DataTable } from '../../components/DataTable';
import type { DataTableColumn } from '../../components/DataTable';
import { useAuthRole } from '../../hooks/AuthRoleContext';
import { useLuminaMemory, DEFAULT_MEMORY_FILTERS } from '../../hooks/useLuminaMemory';
import type { MemoryFilters } from '../../hooks/useLuminaMemory';
import { MemoryTypeBadge, MemoryTypeLegend } from './MemoryTypeBadge';
import { SensitivityBadge } from './SensitivityBadge';
import { MemoryDrawer } from './MemoryDrawer';
import { clampPreview, formatBytes } from './memorySearch';
import { SENSITIVITY_CATEGORIES } from '../../types/luminaMemory';
import type { Memory, MemoryType, MemoryVisibility, SensitivityCategory } from '../../types/luminaMemory';

const MEMORY_TYPES: MemoryType[] = ['Episodic', 'Semantic', 'Preference', 'Principle'];
const VISIBILITIES: MemoryVisibility[] = ['Private', 'Shared', 'System'];

function ViewerPlaceholder() {
  return (
    <Card variant="content">
      <CardTitle subtitle="Memory browsing requires operator access">Read-only engram access</CardTitle>
      <p style={{ color: 'var(--text-tertiary)', fontSize: 'var(--fs-sm)', margin: 0 }}>
        Your session can see Lumina's status elsewhere in this module, but browsing individual
        memory records needs operator role. Ask an operator if you need to inspect the engram
        store directly.
      </p>
    </Card>
  );
}

function selectStyle(): React.CSSProperties {
  return {
    background: 'var(--space-700)', border: '1px solid var(--border)', borderRadius: 'var(--radius-md)',
    color: 'var(--text-primary)', padding: '5px 8px', fontSize: 'var(--fs-sm)', fontFamily: 'inherit',
  };
}

function FilterRow({ filters, onChange }: { filters: MemoryFilters; onChange: (f: MemoryFilters) => void }) {
  return (
    <div style={{ display: 'flex', flexWrap: 'wrap', gap: 'var(--space-2)', alignItems: 'center' }}>
      <input
        type="search"
        value={filters.q}
        onChange={e => onChange({ ...filters, q: e.target.value })}
        placeholder="Search content (hybrid search)…"
        aria-label="Search memory content"
        style={{ ...selectStyle(), flex: '1 1 220px', minWidth: 180 }}
      />
      <select
        aria-label="Filter by memory type"
        value={filters.type}
        onChange={e => onChange({ ...filters, type: (e.target.value || '') as MemoryFilters['type'] })}
        style={selectStyle()}
      >
        <option value="">All types</option>
        {MEMORY_TYPES.map(t => <option key={t} value={t}>{t}</option>)}
      </select>
      <select
        aria-label="Filter by sensitivity"
        value={filters.sensitivity}
        onChange={e => onChange({ ...filters, sensitivity: (e.target.value || '') as MemoryFilters['sensitivity'] })}
        style={selectStyle()}
      >
        <option value="">All sensitivities</option>
        {SENSITIVITY_CATEGORIES.map((s: SensitivityCategory) => <option key={s} value={s}>{s}</option>)}
      </select>
      <select
        aria-label="Filter by visibility"
        value={filters.visibility}
        onChange={e => onChange({ ...filters, visibility: (e.target.value || '') as MemoryFilters['visibility'] })}
        style={selectStyle()}
      >
        <option value="">All visibility</option>
        {VISIBILITIES.map(v => <option key={v} value={v}>{v}</option>)}
      </select>
      <input
        type="text"
        value={filters.user}
        onChange={e => onChange({ ...filters, user: e.target.value })}
        placeholder="User scope (admin)"
        aria-label="Filter by user scope (admin only)"
        style={{ ...selectStyle(), width: 160 }}
      />
      <select
        aria-label="Result limit"
        value={filters.limit}
        onChange={e => onChange({ ...filters, limit: Number(e.target.value) })}
        style={selectStyle()}
      >
        {[10, 25, 50, 100].map(n => <option key={n} value={n}>{n} results</option>)}
      </select>
      {(filters.q || filters.type || filters.sensitivity || filters.visibility || filters.user || filters.limit !== DEFAULT_MEMORY_FILTERS.limit) && (
        <button
          type="button"
          onClick={() => onChange(DEFAULT_MEMORY_FILTERS)}
          style={{
            background: 'transparent', border: '1px solid var(--border)', borderRadius: 'var(--radius-md)',
            color: 'var(--text-tertiary)', fontFamily: 'var(--font-mono)', fontSize: 'var(--fs-mono-sm)',
            padding: '5px 10px', cursor: 'pointer',
          }}
        >
          Clear filters
        </button>
      )}
    </div>
  );
}

function TypeMiniBars({ byType }: { byType: Record<string, number> }) {
  const total = Object.values(byType).reduce((a, b) => a + b, 0) || 1;
  const order: MemoryType[] = ['Principle', 'Semantic', 'Preference', 'Episodic'];
  return (
    <div style={{ display: 'flex', flexDirection: 'column', gap: 4 }}>
      {order.map(t => {
        const count = byType[t] ?? 0;
        const pct = (count / total) * 100;
        return (
          <div key={t} style={{ display: 'flex', alignItems: 'center', gap: 8 }}>
            <div style={{ width: 76, flexShrink: 0 }}><MemoryTypeBadge type={t} /></div>
            <div style={{ flex: 1, height: 6, background: 'var(--space-700)', borderRadius: 'var(--radius-pill)', overflow: 'hidden' }}>
              <div style={{ width: `${pct}%`, height: '100%', background: 'var(--accent-bright)' }} />
            </div>
            <span style={{ fontFamily: 'var(--font-mono)', fontSize: 'var(--fs-mono-sm)', color: 'var(--text-tertiary)', width: 44, textAlign: 'right' }}>
              {count}
            </span>
          </div>
        );
      })}
    </div>
  );
}

export function MemoryPanel() {
  const role = useAuthRole();
  // Rules-of-hooks: useLuminaMemory must always be called, but `enabled: false` for a viewer
  // session means it never issues the stats/search fetch at all — a viewer only ever sees the
  // ViewerPlaceholder below, and now never even requests the underlying memory content.
  const { stats, results, filters, setFilters, refetchAll } = useLuminaMemory(role !== 'viewer');
  const [selectedId, setSelectedId] = useState<string | null>(null);

  const byId = useMemo(() => {
    const m = new Map<string, Memory>();
    (results.data ?? []).forEach(r => m.set(r.id, r));
    return m;
  }, [results.data]);

  const selected = selectedId ? byId.get(selectedId) ?? null : null;
  // `selectedId` may point at a superseding record outside the current filtered/paginated
  // result set (§3.3's "row → drawer ... superseded_by link navigating the chain" doesn't
  // promise the chain stays within one filter view). `pendingId` distinguishes that in-flight
  // case (still resolving after a widen+refetch) from "drawer closed" for MemoryDrawer.
  const pendingId = selectedId && !selected ? selectedId : null;

  const handleNavigate = (id: string) => {
    setSelectedId(id);
    // The search API has no fetch-by-id (§7 only exposes search+stats), so the best available
    // recovery when the target isn't in `byId` is to widen the view as much as the contract
    // allows — clear all filters and take the largest result window — and let the refetch run;
    // `supersededChain` isn't a network primitive, it's the local-cycle-detection guard, not a
    // fetch mechanism, so widening + refetch is the correct fix here rather than calling it.
    if (!byId.has(id)) {
      setFilters({ ...DEFAULT_MEMORY_FILTERS, limit: 100 });
    }
  };

  if (role === 'viewer') {
    return (
      <div style={{ padding: 'var(--space-5)' }}>
        <CardTitle subtitle="/lumina/memory">Memory</CardTitle>
        <ViewerPlaceholder />
      </div>
    );
  }

  const columns: DataTableColumn<Memory>[] = [
    {
      key: 'content', header: 'Content', render: m => (
        <span
          style={{
            display: '-webkit-box', WebkitLineClamp: 2, WebkitBoxOrient: 'vertical', overflow: 'hidden',
            maxWidth: 420, fontSize: 'var(--fs-sm)',
          }}
        >
          {clampPreview(m.content)}
        </span>
      ),
    },
    { key: 'type', header: 'Type', render: m => <MemoryTypeBadge type={m.memory_type} /> },
    { key: 'sensitivity', header: 'Sensitivity', render: m => <SensitivityBadge sensitivity={m.sensitivity} /> },
    { key: 'confidence', header: 'Confidence', align: 'right', render: m => <span style={{ fontFamily: 'var(--font-mono)' }}>{m.confidence.toFixed(2)}</span> },
    { key: 'created_at', header: 'Created', render: m => <span style={{ fontFamily: 'var(--font-mono)', fontSize: 'var(--fs-mono-sm)' }}>{new Date(m.created_at).toLocaleDateString()}</span> },
    { key: 'access_count', header: 'Accesses', align: 'right', render: m => <span style={{ fontFamily: 'var(--font-mono)' }}>{m.access_count}</span> },
  ];

  const noStore = !stats.loading && stats.data?.total === 0;
  const storeError = !stats.loading && stats.data && !stats.data.store_ok;

  return (
    <div style={{ padding: 'var(--space-5)', display: 'flex', flexDirection: 'column', gap: 'var(--space-4)' }}>
      <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'flex-start', flexWrap: 'wrap', gap: 'var(--space-3)' }}>
        <CardTitle subtitle="Search, inspect, and audit the assistant's engram store — read-only">Memory</CardTitle>
        <MemoryTypeLegend />
      </div>

      {storeError ? (
        <Card variant="content" style={{ borderColor: 'var(--status-error)' }}>
          <CardTitle subtitle="The engram store failed to open">Store unavailable</CardTitle>
          <p style={{ color: 'var(--status-error)', fontSize: 'var(--fs-sm)', margin: 0 }}>
            Operator action required: check{' '}
            <code style={{ fontFamily: 'var(--font-mono)' }}>
              {stats.data?.security_violation_key ?? 'ENGRAM_DB_KEY'}
            </code>{' '}
            in the vault — the key is missing or a security violation was raised on open. Never
            paste the secret's value anywhere; only its presence/name is surfaced here.
          </p>
        </Card>
      ) : (
        <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(160px, 1fr))', gap: 'var(--space-3)' }}>
          <MetricCard label="Total memories" value={stats.data ? String(stats.data.total) : '—'} />
          <MetricCard label="DB size" value={stats.data ? formatBytes(stats.data.db_bytes) : '—'} />
          <MetricCard
            label="Embedding coverage"
            value={stats.data ? `${stats.data.embedded_pct.toFixed(1)}%` : '—'}
            valueColor={stats.data && stats.data.embedded_pct < 90 ? 'warning' : 'primary'}
          />
          <MetricCard label="Store health" value={stats.data ? (stats.data.store_ok ? 'OK' : 'ERROR') : '—'} valueColor={stats.data?.store_ok === false ? 'error' : 'success'} />
          <Card variant="content" style={{ gridColumn: 'span 2' }}>
            <CardTitle>By type</CardTitle>
            {stats.data ? <TypeMiniBars byType={stats.data.by_type} /> : <span style={{ color: 'var(--text-tertiary)', fontSize: 'var(--fs-sm)' }}>Loading…</span>}
          </Card>
        </div>
      )}

      {stats.error && (
        <div style={{ color: 'var(--status-warning)', fontSize: 'var(--fs-sm)' }}>Stats unavailable: {stats.error}</div>
      )}

      <Card variant="content">
        <FilterRow filters={filters} onChange={setFilters} />
      </Card>

      <Card variant="content">
        <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', marginBottom: 'var(--space-2)' }}>
          <CardTitle subtitle={results.loading ? undefined : `${results.data?.length ?? 0} result(s) · row opens the full record`}>
            Results
          </CardTitle>
          <button
            type="button"
            onClick={refetchAll}
            style={{
              background: 'transparent', border: '1px solid var(--border)', borderRadius: 'var(--radius-md)',
              color: 'var(--text-tertiary)', fontFamily: 'var(--font-mono)', fontSize: 'var(--fs-mono-sm)',
              padding: '3px 10px', cursor: 'pointer',
            }}
          >
            Refresh
          </button>
        </div>
        {noStore ? (
          <div style={{ textAlign: 'center', padding: 'var(--space-5)', color: 'var(--text-tertiary)' }}>
            <div style={{ fontSize: 'var(--fs-md)', color: 'var(--text-primary)', marginBottom: 'var(--space-2)' }}>
              No memories yet
            </div>
            <div style={{ fontSize: 'var(--fs-sm)' }}>
              The engram store is empty — the assistant builds memory as it converses. If this
              is a fresh install, finish the onboarding wizard (<code style={{ fontFamily: 'var(--font-mono)' }}>/lumina/setup</code>) to get
              it talking.
            </div>
          </div>
        ) : results.error ? (
          <div style={{ color: 'var(--status-error)', fontSize: 'var(--fs-sm)', padding: 'var(--space-3)' }}>
            Search failed: {results.error}
          </div>
        ) : (
          <DataTable
            columns={columns}
            rows={results.data ?? []}
            rowKey={m => m.id}
            emptyMessage={results.loading ? 'Loading…' : 'No memories match these filters'}
            onRowClick={m => setSelectedId(m.id)}
          />
        )}
      </Card>

      <MemoryDrawer
        memory={selected}
        lookup={id => byId.get(id) ?? null}
        onClose={() => setSelectedId(null)}
        onNavigate={handleNavigate}
        pendingId={pendingId}
      />
    </div>
  );
}
