// CONST-28: Terminus module self — tool catalog. Module-prefix groups from the CONST-28-
// extended `/api/terminus/config` (modules[].tools), searchable + paged DataTable (edge case:
// huge tool catalog — e.g. `plane` with 30+ tools in the mock fixture).
//
// Palette-entity source seam: CONST-25 (command palette, parallel item) is NOT on this branch's
// base (origin/main predates it) — rather than importing a module that doesn't exist yet, the
// seam below is a clearly-marked TODO a future rebase/merge wires up. Do not add a speculative
// import for CONST-25's registry here.
import { useEffect, useMemo, useState } from 'react';
import { Card, CardTitle } from '../../components/Card';
import { Badge } from '../../components/Badge';
import { SkeletonList } from '../../components/Skeleton';
import { DataTable } from '../../components/DataTable';
import type { DataTableColumn } from '../../components/DataTable';
import { getAggregationClient } from '../../lib/aggregationClient';
import type { TerminusConfigSummary } from '../../lib/aggregationClient';

const PAGE_SIZE = 25;

interface ToolRow {
  tool: string;
  module: string;
}

// TODO(CONST-25 seam): once the command-palette entity registry lands (`registerPaletteSource`
// or equivalent, per CONST-25's spec item), register this catalog as a palette entity source
// here, e.g.:
//   useEffect(() => { registerPaletteSource('terminus.tools', () => rows.map(...)); }, [rows]);
// Left as a no-op TODO rather than an import of a not-yet-existing module so this branch
// typechecks/builds cleanly against origin/main; wire it up on the first rebase past CONST-25.

export function ToolsPanel() {
  const [config, setConfig] = useState<TerminusConfigSummary | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [query, setQuery] = useState('');
  const [moduleFilter, setModuleFilter] = useState<string | null>(null);
  const [page, setPage] = useState(0);

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

  const allRows: ToolRow[] = useMemo(() => {
    if (!config) return [];
    const rows: ToolRow[] = [];
    for (const m of config.modules) {
      for (const tool of m.tools ?? []) {
        rows.push({ tool, module: m.name });
      }
    }
    return rows;
  }, [config]);

  const moduleNames = useMemo(
    () => Array.from(new Set(allRows.map(r => r.module))).sort(),
    [allRows],
  );

  const filteredRows = useMemo(() => {
    const q = query.trim().toLowerCase();
    return allRows.filter(r => {
      if (moduleFilter && r.module !== moduleFilter) return false;
      if (!q) return true;
      return r.tool.toLowerCase().includes(q) || r.module.toLowerCase().includes(q);
    });
  }, [allRows, query, moduleFilter]);

  // Reset to page 0 whenever the filter set shrinks under the current page (edge case: a
  // narrower search/module filter would otherwise strand the view on an out-of-range page).
  const pageCount = Math.max(1, Math.ceil(filteredRows.length / PAGE_SIZE));
  const clampedPage = Math.min(page, pageCount - 1);
  const pageRows = filteredRows.slice(clampedPage * PAGE_SIZE, clampedPage * PAGE_SIZE + PAGE_SIZE);

  const columns: DataTableColumn<ToolRow>[] = [
    { key: 'module', header: 'Module', render: r => <Badge tone="violet">{r.module}</Badge> },
    { key: 'tool', header: 'Tool', render: r => <code style={{ fontFamily: 'var(--font-mono)', fontSize: 'var(--fs-mono-sm)' }}>{r.tool}</code> },
  ];

  return (
    <div style={{ padding: 'var(--space-5)', display: 'flex', flexDirection: 'column', gap: 'var(--space-4)' }}>
      <CardTitle subtitle="Every registered tool, grouped by module — searchable and paged for large catalogs">
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
          <div style={{ display: 'flex', gap: 'var(--space-2)', flexWrap: 'wrap', alignItems: 'center' }}>
            <input
              type="text"
              value={query}
              onChange={e => { setQuery(e.target.value); setPage(0); }}
              placeholder="Search tools…"
              aria-label="Search tools"
              style={{
                background: 'var(--space-700)',
                border: '1px solid var(--border)',
                borderRadius: 'var(--radius-md)',
                color: 'var(--text-primary)',
                padding: '6px 10px',
                fontSize: 'var(--fs-sm)',
                minWidth: 220,
              }}
            />
            <button
              type="button"
              onClick={() => { setModuleFilter(null); setPage(0); }}
              className={`h-badge ${moduleFilter === null ? 'h-badge-violet' : 'h-badge-neutral'}`}
              style={{ cursor: 'pointer', border: 'none' }}
            >
              all ({allRows.length})
            </button>
            {moduleNames.map(name => {
              const count = allRows.filter(r => r.module === name).length;
              return (
                <button
                  key={name}
                  type="button"
                  onClick={() => { setModuleFilter(name); setPage(0); }}
                  className={`h-badge ${moduleFilter === name ? 'h-badge-violet' : 'h-badge-neutral'}`}
                  style={{ cursor: 'pointer', border: 'none' }}
                >
                  {name} ({count})
                </button>
              );
            })}
          </div>

          <Card variant="content">
            <DataTable
              columns={columns}
              rows={pageRows}
              rowKey={r => r.tool}
              emptyMessage={allRows.length === 0 ? 'No tools registered' : 'No tools match this filter'}
            />
          </Card>

          {filteredRows.length > 0 && (
            <div style={{ display: 'flex', alignItems: 'center', gap: 'var(--space-2)', fontSize: 'var(--fs-xs)', color: 'var(--text-muted)' }}>
              <button
                type="button"
                disabled={clampedPage === 0}
                onClick={() => setPage(p => Math.max(0, p - 1))}
                style={{ opacity: clampedPage === 0 ? 0.4 : 1 }}
              >
                ← prev
              </button>
              <span>
                page {clampedPage + 1} / {pageCount} · {filteredRows.length} tool{filteredRows.length === 1 ? '' : 's'}
              </span>
              <button
                type="button"
                disabled={clampedPage >= pageCount - 1}
                onClick={() => setPage(p => Math.min(pageCount - 1, p + 1))}
                style={{ opacity: clampedPage >= pageCount - 1 ? 0.4 : 1 }}
              >
                next →
              </button>
            </div>
          )}
        </>
      )}
    </div>
  );
}
