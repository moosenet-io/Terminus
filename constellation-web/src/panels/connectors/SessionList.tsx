// RMCP-13 (TERM-624): live grants (consents + their token families), with per-row revoke and a
// global "revoke every session for this client".
//
// Revocation is the page's emergency control, so it is built to be usable in a hurry and hard
// to fire by accident: both paths go through an explicit confirmation that names exactly what
// is about to stop working, and the bulk one names the client and the count. Revoking takes
// effect at the next dispatch (RMCP-11), which the confirmation says out loud — an operator who
// believes a token dies instantly may stop investigating too early.
//
// A revoked row stays visible, marked, rather than disappearing. The list is an audit surface
// as much as a control: "what did this client have, and when was it cut off" is the question
// asked after an incident, and a row that vanishes cannot answer it.
import { useCallback, useEffect, useState } from 'react';
import { Badge } from '../../components/Badge';
import { Button } from '../../components/Button';
import { ConfirmDialog } from '../../components/ConfirmDialog';
import { EmptyState } from '../../components/EmptyState';
import { RoleGate } from '../../components/RoleGate';
import { SkeletonList } from '../../components/Skeleton';
import { SortableTable } from '../../components/SortableTable';
import type { SortableColumn } from '../../components/SortableTable';
import { describeRmcpError, listSessions, revokeSessions } from '../../lib/rmcpClient';
import type { RmcpSession } from '../../types/rmcp';

const mono = { fontFamily: 'var(--font-mono)', fontSize: 'var(--fs-mono-sm)' } as const;

export interface SessionListProps {
  /** Scope the list to one client, or omit for every session this session may see. */
  clientRowId?: string;
  /** Shown in the bulk-revoke confirmation. */
  clientName?: string;
}

export function SessionList({ clientRowId, clientName }: SessionListProps) {
  const [sessions, setSessions] = useState<RmcpSession[] | null>(null);
  const [failure, setFailure] = useState<string | null>(null);
  const [pending, setPending] = useState<{ kind: 'one'; session: RmcpSession } | { kind: 'all' } | null>(null);
  const [busy, setBusy] = useState(false);

  const load = useCallback(() => {
    let cancelled = false;
    setFailure(null);
    listSessions(clientRowId)
      .then(s => {
        if (!cancelled) setSessions(s);
      })
      .catch(e => {
        if (!cancelled) {
          setSessions(null);
          setFailure(describeRmcpError(e).message);
        }
      });
    return () => {
      cancelled = true;
    };
  }, [clientRowId]);

  useEffect(() => load(), [load]);

  const confirmRevoke = () => {
    if (!pending) return;
    setBusy(true);
    const call =
      pending.kind === 'one'
        ? revokeSessions({ sessionId: pending.session.id })
        : revokeSessions({ clientRowId: clientRowId as string });
    call
      .then(() => {
        setPending(null);
        load();
      })
      .catch(e => setFailure(describeRmcpError(e).message))
      .finally(() => setBusy(false));
  };

  const active = (sessions ?? []).filter(s => !s.revokedAt);

  const columns: SortableColumn<RmcpSession>[] = [
    {
      key: 'account',
      header: 'Account',
      sortable: true,
      sortValue: s => s.accountName,
      render: s => <span style={{ fontSize: 'var(--fs-sm)', color: 'var(--text-100)' }}>{s.accountName}</span>,
    },
    {
      key: 'client',
      header: 'Connector',
      sortable: true,
      sortValue: s => s.clientName,
      render: s => <span style={{ fontSize: 'var(--fs-sm)', color: 'var(--text-200)' }}>{s.clientName}</span>,
    },
    {
      key: 'scope',
      header: 'Scope',
      sortable: true,
      sortValue: s => s.scope,
      render: s => <code style={{ ...mono, color: 'var(--text-400)' }}>{s.scope}</code>,
    },
    {
      key: 'granted',
      header: 'Granted',
      sortable: true,
      sortValue: s => Date.parse(s.grantedAt),
      render: s => <span style={{ ...mono, color: 'var(--text-400)' }}>{s.grantedAt}</span>,
    },
    {
      key: 'used',
      header: 'Last used',
      sortable: true,
      sortValue: s => (s.lastUsedAt ? Date.parse(s.lastUsedAt) : 0),
      render: s => <span style={{ ...mono, color: s.lastUsedAt ? 'var(--text-400)' : 'var(--text-500)' }}>{s.lastUsedAt ?? 'never'}</span>,
    },
    {
      key: 'state',
      header: 'State',
      sortable: true,
      sortValue: s => (s.revokedAt ? 0 : s.activeFamilies),
      render: s =>
        s.revokedAt ? (
          <Badge tone="neutral" dot>revoked</Badge>
        ) : (
          <Badge tone="green" dot>{s.activeFamilies} live token {s.activeFamilies === 1 ? 'family' : 'families'}</Badge>
        ),
    },
    {
      key: 'action',
      header: '',
      align: 'right',
      render: s =>
        s.revokedAt ? (
          <span style={{ ...mono, color: 'var(--text-500)' }}>{s.revokedAt}</span>
        ) : (
          <RoleGate>
            <Button size="sm" variant="danger" onClick={() => setPending({ kind: 'one', session: s })}>
              Revoke
            </Button>
          </RoleGate>
        ),
    },
  ];

  return (
    <div style={{ display: 'flex', flexDirection: 'column', gap: 'var(--space-3)' }}>
      <div style={{ display: 'flex', alignItems: 'baseline', justifyContent: 'space-between', gap: 'var(--space-3)', flexWrap: 'wrap' }}>
        <div>
          <div style={{ fontSize: 'var(--fs-body)', color: 'var(--text-100)' }}>Sessions &amp; consents</div>
          <div style={{ fontSize: 'var(--fs-sm)', color: 'var(--text-400)' }}>
            Every approval a human has given, and the token families still live under it.
          </div>
        </div>
        {clientRowId && active.length > 0 && (
          <RoleGate>
            <Button size="sm" variant="danger" onClick={() => setPending({ kind: 'all' })}>
              Revoke all for this connector
            </Button>
          </RoleGate>
        )}
      </div>

      {failure && <EmptyState title="Sessions unavailable" message={failure} compact />}

      {!failure && sessions === null && <SkeletonList rows={4} />}

      {!failure && sessions !== null && sessions.length === 0 && (
        <EmptyState
          title="No sessions"
          message={
            clientRowId
              ? 'Nobody has authorized this connector yet, so it holds no tokens.'
              : 'No account has authorized a connector yet.'
          }
          compact
        />
      )}

      {!failure && sessions !== null && sessions.length > 0 && (
        <SortableTable
          columns={columns}
          rows={sessions}
          rowKey={s => s.id}
          initialSort={{ key: 'granted', dir: 'desc' }}
          pageSize={20}
          emptyMessage="No sessions"
        />
      )}

      <ConfirmDialog
        open={pending !== null}
        title={pending?.kind === 'all' ? `Revoke every session for ${clientName ?? 'this connector'}?` : 'Revoke this session?'}
        description={
          pending?.kind === 'all'
            ? `${active.length} live session${active.length === 1 ? '' : 's'} will be cut off. Their access and refresh tokens stop working at the next call — anyone using this connector must authorize again.`
            : 'This consent and its token families are revoked. Access stops at the next call, not at token expiry.'
        }
        confirmLabel="Revoke"
        destructive
        busy={busy}
        onConfirm={confirmRevoke}
        onCancel={() => setPending(null)}
      />
    </div>
  );
}
