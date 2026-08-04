// RMCP-13 (TERM-624): THE resolved preview — the most valuable element on the Connectors page.
//
// It answers, concretely, "what can this connector actually call right now?" A grant expressed
// as groups × namespaces is an abstraction; a human cannot verify an abstraction. This turns it
// into a list of names they can read, with the group and the pattern that put each one there,
// so a wrong grant is visible BEFORE a connector uses it rather than after.
//
// It is the SERVER'S answer. The component calls `resolveClientScope` (RMCP-07's single
// `effective(...)`, the same function `tools/list` and `tools/call` use) and renders what comes
// back. There is no matching logic in this file. If the preview and the real behaviour could
// ever disagree, the preview would be worse than nothing — an operator would trust it and be
// wrong — so the only implementation allowed to produce it is the one that also enforces it.
//
// Three states this deliberately distinguishes, because collapsing them loses the diagnosis:
//   • reaches nothing      — a real, valid, fail-closed configuration (no groups, or no servers)
//   • in scope, UNAVAILABLE — the tool is granted but its upstream is down. NOT an error: the
//                             config is correct and the mesh is not. Shown as a state, and the
//                             page does not paint red for it.
//   • could not resolve     — the call itself failed. Shown as a failure with its reason.
import { useCallback, useEffect, useState } from 'react';
import { Badge } from '../../components/Badge';
import { Button } from '../../components/Button';
import { EmptyState } from '../../components/EmptyState';
import { SkeletonList } from '../../components/Skeleton';
import { SortableTable } from '../../components/SortableTable';
import type { SortableColumn } from '../../components/SortableTable';
import { SearchInput, Toolbar, ResultCount } from '../../components/Toolbar';
import { describeRmcpError, resolveClientScope } from '../../lib/rmcpClient';
import type { RmcpClient, RmcpResolvedScope, RmcpResolvedTool } from '../../types/rmcp';
import { reachesNothing } from './connectorForm';

/** Rows per page. The server is also asked for a bounded window (see PAGE_FETCH below), so a
 *  very large catalog is bounded on the wire as well as in the DOM. */
const PAGE_SIZE = 25;
/** How many resolved tools to pull per request. One page-fetch backs several rendered pages so
 *  paging feels instant without ever loading an unbounded catalog. */
const PAGE_FETCH = 250;

const mono = { fontFamily: 'var(--font-mono)', fontSize: 'var(--fs-mono-sm)' } as const;

export interface ResolvedToolPreviewProps {
  client: RmcpClient;
  /** Bumped by the editor after a successful save, so the preview re-resolves against the newly
   *  saved scope rather than showing the pre-save answer. */
  refreshKey?: number;
}

export function ResolvedToolPreview({ client, refreshKey = 0 }: ResolvedToolPreviewProps) {
  const [scope, setScope] = useState<RmcpResolvedScope | null>(null);
  const [failure, setFailure] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [query, setQuery] = useState('');
  const [offset, setOffset] = useState(0);

  const empty = reachesNothing(client);

  const load = useCallback(
    (nextOffset: number) => {
      if (empty) {
        setScope(null);
        setFailure(null);
        return () => {};
      }
      let cancelled = false;
      setLoading(true);
      setFailure(null);
      resolveClientScope(client.id, { limit: PAGE_FETCH, offset: nextOffset })
        .then(s => {
          if (!cancelled) setScope(s);
        })
        .catch(e => {
          if (!cancelled) {
            setScope(null);
            setFailure(describeRmcpError(e).message);
          }
        })
        .finally(() => {
          if (!cancelled) setLoading(false);
        });
      return () => {
        cancelled = true;
      };
    },
    [client.id, empty],
  );

  useEffect(() => {
    setOffset(0);
  }, [client.id, refreshKey]);

  useEffect(() => load(offset), [load, offset, refreshKey]);

  const tools = scope?.tools ?? [];
  const q = query.trim().toLowerCase();
  const filtered = q
    ? tools.filter(t => t.name.toLowerCase().includes(q) || t.matchedGroup.toLowerCase().includes(q))
    : tools;

  const columns: SortableColumn<RmcpResolvedTool>[] = [
    {
      key: 'name',
      header: 'Tool',
      sortable: true,
      sortValue: t => t.name,
      width: '46%',
      render: t => <code style={{ ...mono, color: 'var(--text-100)' }}>{t.name}</code>,
    },
    {
      key: 'group',
      header: 'Via group',
      sortable: true,
      sortValue: t => t.matchedGroup,
      render: t => <span style={{ fontSize: 'var(--fs-sm)', color: 'var(--text-200)' }}>{t.matchedGroup}</span>,
    },
    {
      key: 'pattern',
      header: 'Matched pattern',
      sortable: true,
      sortValue: t => t.matchedPattern,
      render: t => <code style={{ ...mono, color: 'var(--text-400)' }}>{t.matchedPattern}</code>,
    },
    {
      key: 'state',
      header: 'State',
      align: 'right',
      sortable: true,
      sortValue: t => (t.available ? 1 : 0),
      render: t =>
        t.available ? (
          <Badge tone="green" dot>reachable</Badge>
        ) : (
          // An upstream that is down is a state of the mesh, not a misconfiguration of the
          // connector — amber (a condition), never rose (a fault).
          <Badge tone="amber" dot>unavailable</Badge>
        ),
    },
  ];

  return (
    <div style={{ display: 'flex', flexDirection: 'column', gap: 'var(--space-3)' }}>
      <div style={{ display: 'flex', alignItems: 'baseline', justifyContent: 'space-between', gap: 'var(--space-3)', flexWrap: 'wrap' }}>
        <div>
          <div style={{ fontSize: 'var(--fs-body)', color: 'var(--text-100)' }}>Tools this connector can reach</div>
          <div style={{ fontSize: 'var(--fs-sm)', color: 'var(--text-400)' }}>
            Resolved by the server against the live catalog — the same decision that gates an actual call.
          </div>
        </div>
        {scope && (
          <span style={{ ...mono, color: 'var(--text-500)' }}>catalog {scope.catalogGeneration}</span>
        )}
      </div>

      {empty && (
        <EmptyState
          title="Reaches nothing"
          message={
            client.enabled
              ? 'This connector has no tool groups, no servers, or neither. A connector with an incomplete scope reaches nothing at all — assign both to give it access.'
              : 'This connector is disabled, so it reaches nothing regardless of its scope.'
          }
          compact
        />
      )}

      {!empty && failure && (
        <EmptyState title="Could not resolve" message={failure} tone="var(--status-warning)" compact />
      )}

      {!empty && !failure && loading && !scope && <SkeletonList rows={5} />}

      {!empty && !failure && scope && (
        <>
          {scope.unavailableNamespaces.length > 0 && (
            <div
              style={{
                display: 'flex',
                alignItems: 'center',
                gap: 'var(--space-2)',
                padding: 'var(--space-2) var(--space-3)',
                borderRadius: 'var(--radius-sm)',
                border: `var(--border-width) solid var(--border)`,
                background: 'var(--surface-chip)',
                fontSize: 'var(--fs-sm)',
                color: 'var(--text-200)',
              }}
            >
              <Badge tone="amber" dot>unavailable</Badge>
              <span>
                {scope.unavailableNamespaces.join(', ')} — assigned, but the upstream is not answering right now.
                Tools from it stay in scope and become reachable again when it returns.
              </span>
            </div>
          )}

          <Toolbar right={<ResultCount count={filtered.length} noun="tool" />}>
            <SearchInput value={query} onChange={setQuery} placeholder="Filter resolved tools…" ariaLabel="Filter resolved tools" />
          </Toolbar>

          <SortableTable
            columns={columns}
            rows={filtered}
            rowKey={t => t.name}
            initialSort={{ key: 'name', dir: 'asc' }}
            pageSize={PAGE_SIZE}
            emptyMessage={
              tools.length === 0
                ? 'The server resolved this scope to no tools — the assigned groups match nothing in the current catalog.'
                : 'No resolved tool matches this filter'
            }
          />

          {(scope.truncated || offset > 0) && (
            <div style={{ display: 'flex', alignItems: 'center', gap: 'var(--space-3)' }}>
              <Button
                size="sm"
                variant="ghost"
                disabled={offset === 0 || loading}
                onClick={() => setOffset(Math.max(0, offset - PAGE_FETCH))}
              >
                Previous {PAGE_FETCH}
              </Button>
              <Button size="sm" variant="ghost" disabled={!scope.truncated || loading} onClick={() => setOffset(offset + PAGE_FETCH)}>
                Next {PAGE_FETCH}
              </Button>
              <span style={{ ...mono, color: 'var(--text-500)' }}>
                showing {offset + 1}–{offset + tools.length}
                {scope.truncated ? ' of more' : ''}
              </span>
            </div>
          )}
        </>
      )}
    </div>
  );
}
