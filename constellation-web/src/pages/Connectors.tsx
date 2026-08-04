// RMCP-13 (TERM-624): the Connectors page — the configuration surface an operator actually
// uses to run the OAuth/MCP connector door (S132).
//
// Three tabs, matching the three questions the surface has to answer:
//   Connectors   — who may connect, and what can they reach? (list → detail + resolved preview)
//   Tool groups  — what does a named grouping actually select right now? (live match preview)
//   Sessions     — who is connected, and how do I cut them off? (per-row + bulk revoke)
//
// It is a `terminus` module panel (registered in `registerPanels.ts` as `terminus.connectors`)
// and follows the existing IA: PanelRoot + CardTitle + design tokens, no Tailwind, no CDN.
//
// EVERY authorization decision on this page is the server's. The UI hides a control a delegated
// owner cannot use (RMCP-12 ownership arrives as `editable`/`ownedByMe` flags on the records
// themselves), but hiding is never the enforcement: the same write is refused by the server
// whether or not a button was rendered, and the reads are already scoped server-side, so a
// delegated owner never receives another owner's objects to hide in the first place.
import { useCallback, useEffect, useMemo, useState } from 'react';
import { Badge } from '../components/Badge';
import { Button } from '../components/Button';
import { Card, CardTitle } from '../components/Card';
import { EmptyState } from '../components/EmptyState';
import { PanelRoot } from '../components/PanelRoot';
import { RoleGate } from '../components/RoleGate';
import { SkeletonList } from '../components/Skeleton';
import { SortableTable } from '../components/SortableTable';
import type { SortableColumn } from '../components/SortableTable';
import { StatusPill } from '../components/StatusPill';
import { Tabs } from '../components/Tabs';
import { ResultCount, SearchInput, Toolbar } from '../components/Toolbar';
import { describeRmcpError, listClients, listGroups, listServers } from '../lib/rmcpClient';
import type { RmcpErrorKind } from '../lib/rmcpClient';
import type { RmcpClient, RmcpServer, RmcpToolGroup } from '../types/rmcp';
import { ClientCreateDialog } from '../panels/connectors/ClientCreateDialog';
import { ClientEditor } from '../panels/connectors/ClientEditor';
import { GroupEditor } from '../panels/connectors/GroupEditor';
import { SessionList } from '../panels/connectors/SessionList';
import { scopeSummary } from '../panels/connectors/connectorForm';

type TabId = 'clients' | 'groups' | 'sessions';

const mono = { fontFamily: 'var(--font-mono)', fontSize: 'var(--fs-mono-sm)' } as const;

export function Connectors() {
  const [tab, setTab] = useState<TabId>('clients');
  const [clients, setClients] = useState<RmcpClient[] | null>(null);
  const [groups, setGroups] = useState<RmcpToolGroup[]>([]);
  const [servers, setServers] = useState<RmcpServer[]>([]);
  const [failure, setFailure] = useState<{ kind: RmcpErrorKind; message: string } | null>(null);
  const [loading, setLoading] = useState(true);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [creating, setCreating] = useState(false);
  const [editingGroup, setEditingGroup] = useState<{ group: RmcpToolGroup | null } | null>(null);
  const [query, setQuery] = useState('');

  const load = useCallback(() => {
    setLoading(true);
    // Groups and servers are needed by the editors as well as the lists, so they are loaded
    // with the clients rather than lazily per editor — three reads, once, is cheaper than the
    // same reads repeated on every selection, and it keeps the pickers consistent with the
    // list they were opened from.
    Promise.all([listClients(), listGroups(), listServers()])
      .then(([c, g, s]) => {
        setClients(c);
        setGroups(g);
        setServers(s);
        setFailure(null);
      })
      .catch(e => {
        setClients(null);
        setFailure(describeRmcpError(e));
      })
      .finally(() => setLoading(false));
  }, []);

  useEffect(() => {
    load();
  }, [load]);

  const selected = useMemo(
    () => (selectedId ? (clients ?? []).find(c => c.id === selectedId) ?? null : null),
    [clients, selectedId],
  );

  const filtered = useMemo(() => {
    const q = query.trim().toLowerCase();
    const all = clients ?? [];
    if (!q) return all;
    return all.filter(c => c.name.toLowerCase().includes(q) || c.clientId.toLowerCase().includes(q));
  }, [clients, query]);

  const columns: SortableColumn<RmcpClient>[] = [
    {
      key: 'name',
      header: 'Connector',
      sortable: true,
      sortValue: c => c.name,
      width: '28%',
      render: c => (
        <div style={{ display: 'flex', flexDirection: 'column' }}>
          <span style={{ fontSize: 'var(--fs-sm)', color: 'var(--text-100)' }}>{c.name}</span>
          <code style={{ ...mono, color: 'var(--text-400)' }}>{c.clientId}</code>
        </div>
      ),
    },
    {
      key: 'source',
      header: 'Registered',
      sortable: true,
      sortValue: c => c.registrationSource,
      render: c => (
        <Badge tone={c.registrationSource === 'operator' ? 'violet' : 'blue'} mono>
          {c.registrationSource === 'operator' ? 'operator' : 'dcr'}
        </Badge>
      ),
    },
    {
      key: 'scope',
      header: 'Scope',
      sortable: true,
      sortValue: c => scopeSummary(c),
      render: c => {
        const summary = scopeSummary(c);
        const nothing = summary.includes('reaches nothing');
        return (
          <span style={{ fontSize: 'var(--fs-sm)', color: nothing ? 'var(--status-warning)' : 'var(--text-200)' }}>
            {summary}
          </span>
        );
      },
    },
    {
      key: 'state',
      header: 'State',
      sortable: true,
      sortValue: c => (c.enabled ? 1 : 0),
      render: c => <StatusPill state={c.enabled ? 'online' : 'idle'} label={c.enabled ? 'enabled' : 'disabled'} />,
    },
    {
      key: 'open',
      header: '',
      align: 'right',
      render: c => (
        <Button size="sm" variant="ghost" onClick={() => setSelectedId(c.id)}>
          {c.editable ? 'Manage' : 'View'}
        </Button>
      ),
    },
  ];

  const tabs = [
    { id: 'clients', label: 'Connectors' },
    { id: 'groups', label: 'Tool groups' },
    { id: 'sessions', label: 'Sessions' },
  ];

  return (
    <PanelRoot style={{ padding: 'var(--space-5)', display: 'flex', flexDirection: 'column', gap: 'var(--space-4)' }}>
      <CardTitle subtitle="OAuth connectors, what each one is scoped to reach, and the live grants you can cut off">
        Terminus — Connectors
      </CardTitle>

      {/* The tools backing this page land alongside it (RMCP-05/06/08/11/12). Until they are
          deployed, every read answers `tool_unavailable` and this explains that rather than
          rendering an error — the same posture the Activity panel took toward CONST-26. */}
      {failure?.kind === 'tool_unavailable' && (
        <Card variant="content">
          <EmptyState
            title="Connector administration is not live on this server yet"
            message="The rmcp_* tools that back this page are not deployed here. Nothing is wrong with this view — it will populate as soon as they are."
            compact
          />
        </Card>
      )}

      {failure && failure.kind !== 'tool_unavailable' && (
        <Card variant="content">
          <div style={{ display: 'flex', alignItems: 'center', gap: 'var(--space-3)', flexWrap: 'wrap' }}>
            <span style={{ color: 'var(--status-error)', fontSize: 'var(--fs-sm)' }}>{failure.message}</span>
            <Button size="sm" variant="secondary" onClick={load}>Retry</Button>
          </div>
        </Card>
      )}

      {!failure && (
        <Tabs tabs={tabs} activeId={tab} onSelect={id => setTab(id as TabId)} idBase="connectors" aria-label="Connector administration" />
      )}

      {!failure && loading && clients === null && (
        <Card variant="content">
          <SkeletonList rows={5} />
        </Card>
      )}

      {!failure && clients !== null && tab === 'clients' && (
        selected ? (
          <ClientEditor
            client={selected}
            groups={groups}
            servers={servers}
            onSaved={updated => setClients(prev => (prev ?? []).map(c => (c.id === updated.id ? updated : c)))}
            onRevoked={id => {
              setSelectedId(null);
              setClients(prev => (prev ?? []).filter(c => c.id !== id));
            }}
            onBack={() => setSelectedId(null)}
            onReload={load}
          />
        ) : clients.length === 0 ? (
          // Empty state guides the first creation instead of showing a blank table: the first
          // connector is the hardest one, because nothing on screen yet says what a connector is.
          <Card variant="content">
            <EmptyState
              title="No connectors yet"
              message="A connector is an application you allow to reach a scoped subset of your tools over OAuth. Create one, assign it a tool group and the servers it may use, and check the resolved preview before handing out its credentials."
              action={{ label: 'Create the first connector', onClick: () => setCreating(true) }}
            />
          </Card>
        ) : (
          <>
            <Toolbar right={<ResultCount count={filtered.length} noun="connector" />}>
              <SearchInput value={query} onChange={setQuery} placeholder="Search connectors…" ariaLabel="Search connectors" />
              <RoleGate>
                <Button size="sm" variant="primary" onClick={() => setCreating(true)}>New connector</Button>
              </RoleGate>
            </Toolbar>
            <Card variant="content" padding="var(--space-2)">
              <SortableTable
                columns={columns}
                rows={filtered}
                rowKey={c => c.id}
                initialSort={{ key: 'name', dir: 'asc' }}
                pageSize={25}
                emptyMessage="No connector matches this search"
              />
            </Card>
          </>
        )
      )}

      {!failure && clients !== null && tab === 'groups' && (
        editingGroup ? (
          <GroupEditor
            group={editingGroup.group}
            onSaved={saved => {
              setGroups(prev => {
                const exists = prev.some(g => g.id === saved.id);
                return exists ? prev.map(g => (g.id === saved.id ? saved : g)) : [...prev, saved];
              });
              setEditingGroup(null);
            }}
            onCancel={() => setEditingGroup(null)}
            onReload={load}
          />
        ) : (
          <>
            <Toolbar right={<ResultCount count={groups.length} noun="group" />}>
              <RoleGate>
                <Button size="sm" variant="primary" onClick={() => setEditingGroup({ group: null })}>New tool group</Button>
              </RoleGate>
            </Toolbar>
            {groups.length === 0 ? (
              <Card variant="content">
                <EmptyState
                  title="No tool groups yet"
                  message="A tool group names a set of patterns over the tool catalog, so a connector is scoped in human terms rather than as a list of tool names."
                  action={{ label: 'Create a tool group', onClick: () => setEditingGroup({ group: null }) }}
                />
              </Card>
            ) : (
              <Card variant="content" padding="var(--space-2)">
                <SortableTable
                  columns={[
                    {
                      key: 'name',
                      header: 'Group',
                      sortable: true,
                      sortValue: g => g.name,
                      render: g => (
                        <div style={{ display: 'flex', flexDirection: 'column' }}>
                          <span style={{ fontSize: 'var(--fs-sm)', color: 'var(--text-100)' }}>{g.name}</span>
                          <span style={{ fontSize: 'var(--fs-sm)', color: 'var(--text-400)' }}>{g.description}</span>
                        </div>
                      ),
                    },
                    {
                      key: 'patterns',
                      header: 'Patterns',
                      render: g =>
                        g.patterns.length === 0 ? (
                          <span style={{ fontSize: 'var(--fs-sm)', color: 'var(--status-warning)' }}>none — matches nothing</span>
                        ) : (
                          <code style={{ ...mono, color: 'var(--text-300)' }}>{g.patterns.join(' · ')}</code>
                        ),
                    },
                    {
                      key: 'edit',
                      header: '',
                      align: 'right',
                      render: g => (
                        <Button size="sm" variant="ghost" onClick={() => setEditingGroup({ group: g })}>
                          {g.editable ? 'Edit' : 'View'}
                        </Button>
                      ),
                    },
                  ] as SortableColumn<RmcpToolGroup>[]}
                  rows={groups}
                  rowKey={g => g.id}
                  initialSort={{ key: 'name', dir: 'asc' }}
                  pageSize={25}
                  emptyMessage="No tool groups"
                />
              </Card>
            )}
          </>
        )
      )}

      {!failure && clients !== null && tab === 'sessions' && (
        <Card variant="content">
          <SessionList />
        </Card>
      )}

      <ClientCreateDialog
        open={creating}
        groups={groups}
        servers={servers}
        onCancel={() => setCreating(false)}
        onDone={created => {
          setCreating(false);
          setClients(prev => [...(prev ?? []), created]);
          setSelectedId(created.id);
        }}
      />
    </PanelRoot>
  );
}
