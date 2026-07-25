// CGUI-05 (TERM #528): Terminus module self — the deep TOOL CATALOG.
// S127 TGUI2 POL-06/07/M6: rebuilt from a sparse grouped card-list into a real, dense, sortable
// DATA TABLE — the density template for every list surface. Columns (Tool · Kind · Module ·
// Status · Last call · Latency) sort on click; each row expands (compact chevron) to the full
// schema/config detail. A contextual toolbar (search + neutral module/kind dropdowns + a live
// count) replaces the old colored filter-chip rows, and every per-row tag is now a NEUTRAL
// outline chip with only a tiny semantic leading dot — the single saturated token per row is the
// genuine enabled/disabled status pill.
//
// DATA PROVENANCE (real vs placeholder — the item requires calling it out):
//   • tool NAME, MODULE, ENABLED state  — REAL, from the aggregation client
//     (`terminus.configSummary()` → modules[].tools / modules[].enabled).
//   • category, description, params(schema), rate-limit, auth, last-invocation, latency — DERIVED /
//     representative placeholder in toolCatalog.ts (deterministic), each field noted there as
//     pending the CGUI-08 data client's real per-tool metadata + invocation stream.
//
// Tokens only (var(--…)); the DS primitives (Card/Badge/StatusPill/SortableTable) carry the brand.
// Inter + JetBrains Mono via the font tokens; no emoji.
import { useEffect, useMemo, useState } from 'react';
import { Card, CardTitle } from '../../components/Card';
import { PanelRoot } from '../../components/PanelRoot';
import { Badge } from '../../components/Badge';
import { StatusPill } from '../../components/StatusPill';
import { SkeletonList } from '../../components/Skeleton';
import { DataTable } from '../../components/DataTable';
import type { DataTableColumn } from '../../components/DataTable';
import { SortableTable } from '../../components/SortableTable';
import type { SortableColumn } from '../../components/SortableTable';
import { Toolbar, SearchInput, FilterSelect, ResultCount } from '../../components/Toolbar';
import { getAggregationClient } from '../../lib/aggregationClient';
import type { TerminusConfigSummary } from '../../lib/aggregationClient';
import {
  deriveToolDetail, CATEGORY_DOT_COLOR, fmtLatency,
  type ToolDetail, type ToolCategory, type ToolParam,
} from './toolCatalog';

const CATEGORIES: ToolCategory[] = ['read', 'write', 'search', 'admin'];
const PAGE_SIZE = 25;

// mono cell styling reused across the detail — a code fragment / figure in JetBrains Mono.
const monoSm = { fontFamily: 'var(--font-mono)', fontSize: 'var(--fs-mono-sm)' } as const;

// The neutral kind chip: neutral outline body + a 6px leading dot in the category's semantic ink.
function KindChip({ category }: { category: ToolCategory }) {
  return <Badge tone="neutral" dot dotColor={CATEGORY_DOT_COLOR[category]} mono>{category}</Badge>;
}

export function ToolsPanel() {
  const [config, setConfig] = useState<TerminusConfigSummary | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [query, setQuery] = useState('');
  const [moduleFilter, setModuleFilter] = useState('');
  const [categoryFilter, setCategoryFilter] = useState('');

  useEffect(() => {
    let cancelled = false;
    setLoading(true);
    getAggregationClient()
      .terminus.configSummary()
      .then(d => { if (!cancelled) setConfig(d); })
      .catch(e => { if (!cancelled) setError(e instanceof Error ? e.message : 'Failed to load'); })
      .finally(() => { if (!cancelled) setLoading(false); });
    return () => { cancelled = true; };
  }, []);

  // Real facts (name/module/enabled) → full derived per-tool depth. Memoised once per config
  // load so telemetry stamps stay stable (no flicker) between renders.
  const allTools: ToolDetail[] = useMemo(() => {
    if (!config) return [];
    const out: ToolDetail[] = [];
    for (const m of config.modules) {
      for (const tool of m.tools ?? []) {
        out.push(deriveToolDetail(m.name, tool, m.enabled));
      }
    }
    return out;
  }, [config]);

  const moduleNames = useMemo(
    () => Array.from(new Set(allTools.map(t => t.module))).sort(),
    [allTools],
  );

  const filtered = useMemo(() => {
    const q = query.trim().toLowerCase();
    return allTools.filter(t => {
      if (moduleFilter && t.module !== moduleFilter) return false;
      if (categoryFilter && t.category !== categoryFilter) return false;
      if (!q) return true;
      return (
        t.name.toLowerCase().includes(q) ||
        t.module.toLowerCase().includes(q) ||
        t.description.toLowerCase().includes(q)
      );
    });
  }, [allTools, query, moduleFilter, categoryFilter]);

  const columns: SortableColumn<ToolDetail>[] = useMemo(() => [
    {
      key: 'name', header: 'Tool', sortable: true, sortValue: t => t.name, width: '30%',
      render: t => <code style={{ ...monoSm, color: 'var(--text-100)' }}>{t.name}</code>,
    },
    {
      key: 'kind', header: 'Kind', sortable: true, sortValue: t => t.category,
      render: t => <KindChip category={t.category} />,
    },
    {
      key: 'module', header: 'Module', sortable: true, sortValue: t => t.module,
      render: t => <span style={{ ...monoSm, color: 'var(--text-400)' }}>{t.module}</span>,
    },
    {
      key: 'status', header: 'Status', sortable: true, sortValue: t => (t.enabled ? 1 : 0),
      render: t => <StatusPill state={t.enabled ? 'online' : 'idle'} label={t.enabled ? 'enabled' : 'disabled'} />,
    },
    {
      key: 'lastcall', header: 'Last call', align: 'right', sortable: true,
      sortValue: t => (t.lastInvocation ? Date.parse(t.lastInvocation.ts) : 0),
      render: t => {
        const inv = t.lastInvocation;
        if (!inv) return <span style={{ ...monoSm, color: 'var(--text-500)' }}>never</span>;
        return (
          <span style={{ ...monoSm, color: inv.result === 'error' ? 'var(--flux-rose)' : 'var(--text-400)' }}>
            {inv.result === 'error' ? 'error · ' : ''}{inv.ago}
          </span>
        );
      },
    },
    {
      key: 'latency', header: 'Latency', align: 'right', sortable: true,
      sortValue: t => t.lastInvocation?.latencyMs ?? -1,
      render: t => (
        <span style={{ ...monoSm, color: t.lastInvocation ? 'var(--text-100)' : 'var(--text-500)' }}>
          {t.lastInvocation ? fmtLatency(t.lastInvocation.latencyMs) : '—'}
        </span>
      ),
    },
  ], []);

  return (
    <PanelRoot style={{ padding: 'var(--space-5)', display: 'flex', flexDirection: 'column', gap: 'var(--space-4)' }}>
      <CardTitle subtitle="Every registered tool with its schema, limits, auth identity and last-call telemetry — sortable, searchable, expand a row for depth">
        Terminus — Tools
      </CardTitle>

      {error && (
        <Card variant="content">
          <span style={{ color: 'var(--status-error)' }}>{error}</span>
        </Card>
      )}

      {loading && !error && (
        <Card variant="content">
          <SkeletonList rows={6} />
        </Card>
      )}

      {!loading && !error && (
        <>
          <Toolbar right={<ResultCount count={filtered.length} noun="tool" />}>
            <SearchInput value={query} onChange={setQuery} placeholder="Search tools…" ariaLabel="Search tools" />
            <FilterSelect
              label="Module"
              value={moduleFilter}
              onChange={setModuleFilter}
              allLabel={`All modules (${allTools.length})`}
              options={moduleNames.map(name => ({ value: name, label: `${name} (${allTools.filter(t => t.module === name).length})` }))}
            />
            <FilterSelect
              label="Kind"
              value={categoryFilter}
              onChange={setCategoryFilter}
              allLabel="All kinds"
              options={CATEGORIES.map(cat => ({ value: cat, label: cat }))}
            />
          </Toolbar>

          <Card variant="content" padding="var(--space-2)">
            <SortableTable
              columns={columns}
              rows={filtered}
              rowKey={t => t.name}
              initialSort={{ key: 'name', dir: 'asc' }}
              pageSize={PAGE_SIZE}
              expandable={t => <ToolDetailBody tool={t} />}
              emptyMessage={allTools.length === 0 ? 'No tools registered' : 'No tools match this filter'}
            />
          </Card>
        </>
      )}
    </PanelRoot>
  );
}

// ── expanded detail: description + schema table + config strip ─────────────────────────────────
function ToolDetailBody({ tool }: { tool: ToolDetail }) {
  const paramColumns: DataTableColumn<ToolParam>[] = [
    { key: 'name', header: 'Argument', render: p => <code style={{ ...monoSm, color: 'var(--text-100)' }}>{p.name}</code> },
    { key: 'type', header: 'Type', render: p => <code style={{ ...monoSm, color: 'var(--text-300)' }}>{p.type}</code> },
    { key: 'required', header: 'Req', render: p => p.required
        ? <Badge tone="neutral" dot dotColor="var(--flux-amber)">required</Badge>
        : <span style={{ ...monoSm, color: 'var(--text-500)' }}>optional</span> },
    { key: 'desc', header: 'Description', render: p => <span style={{ fontSize: 'var(--fs-sm)', color: 'var(--text-300)' }}>{p.desc}</span> },
  ];

  return (
    <div style={{ display: 'flex', flexDirection: 'column', gap: 'var(--space-3)' }}>
      {/* description (derived from the tool name — representative, CGUI-08) */}
      <div style={{ fontSize: 'var(--fs-sm)', color: 'var(--text-200)' }}>
        {tool.description}
        <span style={{ ...monoSm, color: 'var(--text-500)' }}> — <code>{tool.name}</code></span>
      </div>

      {/* schema / params */}
      <div>
        <div style={{ fontFamily: 'var(--font-mono)', fontSize: 'var(--fs-mono-sm)', letterSpacing: 'var(--ls-mono)', textTransform: 'uppercase', color: 'var(--text-500)', marginBottom: 'var(--space-2)' }}>
          Schema · arguments
        </div>
        <DataTable columns={paramColumns} rows={tool.params} rowKey={p => p.name} emptyMessage="No arguments" />
      </div>

      {/* config strip: rate limit · auth · state */}
      <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(9.5rem, 1fr))', gap: 'var(--space-3)' }}>
        <ConfigCell label="rate limit" value={tool.rateLimit} />
        <ConfigCell label="auth identity" value={tool.auth} mono />
        <ConfigCell label="state" node={<StatusPill state={tool.enabled ? 'online' : 'idle'} label={tool.enabled ? 'enabled' : 'disabled'} />} />
      </div>
    </div>
  );
}

function ConfigCell({ label, value, node, mono }: { label: string; value?: string; node?: React.ReactNode; mono?: boolean }) {
  return (
    <div style={{ display: 'flex', flexDirection: 'column', gap: 'var(--space-1)' }}>
      <span style={{ fontFamily: 'var(--font-mono)', fontSize: 'var(--fs-mono-sm)', letterSpacing: 'var(--ls-mono)', textTransform: 'uppercase', color: 'var(--text-500)' }}>{label}</span>
      {node ?? (
        <span style={mono ? { ...monoSm, color: 'var(--text-100)' } : { fontSize: 'var(--fs-sm)', color: 'var(--text-100)' }}>
          {value}
        </span>
      )}
    </div>
  );
}
