// TERM #654: the Accounts page — the humans the OAuth door authenticates, and the surface that
// brings the first one into existence.
//
// Every other connector surface presupposes an account: one signs in at `/oauth/login`, one
// grants consent, and `rmcp_client_create` names one as a connector's owner. S132 shipped all of
// that with no way to create an account at all, so this page is where a fresh deployment starts.
//
// ── THE FIRST VISIT IS THE DESIGNED CASE, NOT THE EDGE CASE ───────────────────────────────────
//
// The very first time anyone opens this page, there are zero accounts and no operator. That is
// the state the page is built around: it explains the one-shot first-operator path in the empty
// state rather than rendering a blank table, because "no accounts" and "this feature is broken"
// look identical otherwise — and the operator has no other route in.
//
// It is registered `available: true` for the same reason the Connectors panel is: when the
// backing tools are not deployed, the PAGE says so. A nav entry that disappears looks like the
// feature does not exist, which is the worst possible signal for the one page that fixes an
// unusable door.
//
// ── EVERY RULE ON THIS PAGE IS THE SERVER'S ───────────────────────────────────────────────────
//
// Two of them are worth naming because a UI is the natural place to accidentally re-implement
// them:
//
//  1. **The last-operator guard.** The server refuses to demote or disable the last active
//     operator, inside the transaction that would have done it. This page DISABLES those controls
//     and says why — but that is a courtesy, not the guard. Delete every check in
//     `accountForm.ts` and the deployment is exactly as safe; the button would simply fail with
//     the server's refusal instead of explaining itself first.
//  2. **The bootstrap gate.** Whether the first-account path is open is `bootstrapAvailable` from
//     the server — it is NOT inferred from `accounts.length === 0`. The two differ in the state
//     that matters: a door whose accounts exist but whose operators are all disabled has an empty
//     *visible* list and is NOT bootstrappable, and offering the bootstrap there would send the
//     operator to run a call that is refused.
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
import { ResultCount, SearchInput, Toolbar } from '../components/Toolbar';
import {
  describeRmcpError,
  listAccounts,
  setAccountDisabled,
  setAccountOperator,
} from '../lib/rmcpClient';
import type { RmcpErrorKind } from '../lib/rmcpClient';
import type { RmcpAccount, RmcpAccountsView } from '../types/rmcp';
import { AccountCreateDialog } from '../panels/accounts/AccountCreateDialog';
import { actorIsAmbiguous, actorSuggestions, wouldStrandTheDoor } from '../panels/accounts/accountForm';

const mono = { fontFamily: 'var(--font-mono)', fontSize: 'var(--fs-mono-sm)' } as const;

export function Accounts() {
  const [view, setView] = useState<RmcpAccountsView | null>(null);
  const [failure, setFailure] = useState<{ kind: RmcpErrorKind; message: string } | null>(null);
  const [loading, setLoading] = useState(true);
  const [creating, setCreating] = useState(false);
  const [query, setQuery] = useState('');
  const [busyAccount, setBusyAccount] = useState<string | null>(null);
  const [actionFailure, setActionFailure] = useState<string | null>(null);
  // Only asked for when the server would require it. Never defaulted, never remembered.
  const [actor, setActor] = useState('');

  const load = useCallback(() => {
    setLoading(true);
    listAccounts()
      .then(v => {
        setView(v);
        setFailure(null);
      })
      .catch(e => {
        setView(null);
        setFailure(describeRmcpError(e));
      })
      .finally(() => setLoading(false));
  }, []);

  useEffect(() => {
    load();
  }, [load]);

  const accounts = view?.accounts ?? [];
  const askActor = actorIsAmbiguous(accounts);
  const operatorNames = actorSuggestions(accounts);

  // Round 2 (codex): a selected actor survived the condition that required it.
  // Demote or disable the operator you were acting as, or drop back to a single
  // operator, and the picker disappears while the stale name was still being
  // sent — so every later action was refused until the page remounted. The
  // selection is dropped as soon as it is no longer REQUIRED or no longer a
  // valid choice; nothing is auto-selected in its place.
  useEffect(() => {
    if (actor && (!askActor || !operatorNames.includes(actor))) setActor('');
  }, [actor, askActor, operatorNames]);

  const filtered = useMemo(() => {
    const q = query.trim().toLowerCase();
    if (!q) return accounts;
    return accounts.filter(a => a.account.toLowerCase().includes(q));
  }, [accounts, query]);

  // One place both mutating controls funnel through, so the actor rule, the busy handling and
  // the refusal surfacing exist once. A refusal is always SHOWN — a control that fails silently
  // is how an operator concludes the page is broken and reaches for the CLI.
  const mutate = useCallback(
    (account: RmcpAccount, run: (actor?: string) => Promise<void>) => {
      if (askActor && actor.trim().length === 0) {
        setActionFailure(
          'Several operators are active, so this action must name the one performing it. Choose an operator above first.',
        );
        return;
      }
      setBusyAccount(account.account);
      setActionFailure(null);
      // Sent ONLY while the server would require it. Passing a name the server
      // did not ask for is how a stale selection outlives its reason.
      run(askActor ? actor.trim() || undefined : undefined)
        .then(load)
        .catch(e => setActionFailure(describeRmcpError(e).message))
        .finally(() => setBusyAccount(null));
    },
    [actor, askActor, load],
  );

  const columns: SortableColumn<RmcpAccount>[] = [
    {
      key: 'account',
      header: 'Account',
      sortable: true,
      sortValue: a => a.account,
      width: '34%',
      render: a => (
        <div style={{ display: 'flex', flexDirection: 'column' }}>
          <span style={{ fontSize: 'var(--fs-sm)', color: 'var(--text-100)' }}>{a.account}</span>
          <code style={{ ...mono, color: 'var(--text-400)' }}>{a.id}</code>
        </div>
      ),
    },
    {
      key: 'authority',
      header: 'Authority',
      sortable: true,
      sortValue: a => (a.operator ? 1 : 0),
      render: a =>
        a.operator ? (
          <Badge tone="violet" mono>operator</Badge>
        ) : (
          <span style={{ fontSize: 'var(--fs-sm)', color: 'var(--text-400)' }}>delegated</span>
        ),
    },
    {
      key: 'state',
      header: 'State',
      sortable: true,
      sortValue: a => (a.disabled ? 0 : 1),
      render: a => (
        <StatusPill
          state={a.disabled ? 'idle' : 'online'}
          label={a.disabled ? 'disabled' : 'enabled'}
        />
      ),
    },
    {
      key: 'created',
      header: 'Created',
      sortable: true,
      sortValue: a => a.createdAt,
      render: a => (
        <span style={{ fontSize: 'var(--fs-sm)', color: 'var(--text-400)' }}>
          {a.createdAt.slice(0, 10)}
        </span>
      ),
    },
    {
      key: 'actions',
      header: '',
      align: 'right',
      render: a => {
        // Both controls remove authority when they act on the last active operator, so both
        // consult the same reflection of the same server rule.
        const strands = wouldStrandTheDoor(accounts, a);
        const busy = busyAccount === a.account;
        const lastOperatorReason =
          'This is the last active operator. The server refuses to remove it — a door with no operator cannot administer itself and cannot be bootstrapped again.';
        // The server resolves a promotion target through its active-account
        // lookup, so a DISABLED account is NotFound there — the authority
        // control can never succeed on one. Round 3 (codex): it was left
        // enabled, offering an action the page already knew would be refused.
        // Enable the account first; that control is right beside it.
        const authorityReason = a.disabled
          ? 'This account is disabled. Authority cannot be changed on an account that cannot sign in — enable it first.'
          : strands
            ? lastOperatorReason
            : undefined;
        return (
          <RoleGate>
            <div style={{ display: 'flex', gap: 'var(--space-2)', justifyContent: 'flex-end' }}>
              <Button
                size="sm"
                variant="ghost"
                disabled={busy || a.disabled || (a.operator && strands)}
                title={authorityReason}
                onClick={() => mutate(a, act => setAccountOperator(a.account, !a.operator, act))}
              >
                {a.operator ? 'Demote' : 'Promote'}
              </Button>
              <Button
                size="sm"
                variant="ghost"
                disabled={busy || strands}
                title={strands ? lastOperatorReason : undefined}
                onClick={() => mutate(a, act => setAccountDisabled(a.account, !a.disabled, act))}
              >
                {a.disabled ? 'Enable' : 'Disable'}
              </Button>
            </div>
          </RoleGate>
        );
      },
    },
  ];

  return (
    <PanelRoot style={{ padding: 'var(--space-5)', display: 'flex', flexDirection: 'column', gap: 'var(--space-4)' }}>
      <CardTitle subtitle="The people this server's OAuth door authenticates — who may sign in, who may administer it, and how the first one comes to exist">
        Terminus — Accounts
      </CardTitle>

      {/* The `rmcp_account_*` tools and the dispatch endpoint that reaches them land alongside
          this page. Until they are deployed, the read answers `tool_unavailable` and this
          explains it rather than rendering an error — the same posture Connectors takes. */}
      {failure?.kind === 'tool_unavailable' && (
        <Card variant="content">
          <EmptyState
            title="This page cannot reach the account tools yet"
            message="The rmcp_account_* tools are not reachable from this server — either the web bridge this page calls is not deployed, or it answered that it does not serve those tools. Either way this view can tell you nothing about the deployment's accounts, so it is showing you that rather than an empty list. The tools may well be live on the Terminus door itself, where an operator can create the first account with rmcp_account_create in the meantime."
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

      {!failure && loading && view === null && (
        <Card variant="content">
          <SkeletonList rows={4} />
        </Card>
      )}

      {/* STRANDED: accounts exist, none can administer the door, and the first-account path will
          NOT reopen. Distinct from "no accounts" because the remedy is completely different, and
          a page that showed the bootstrap here would send the operator to a refusal. */}
      {!failure && view?.stranded && (
        <Card variant="content">
          <EmptyState
            title="This deployment has no active operator"
            message="Accounts exist here, but none of them holds operator authority, so nothing can administer the door. The first-account path does not reopen — it is gated on whether any account exists, not on whether an operator does. Re-enable or re-promote an operator account directly against the server's database."
          />
        </Card>
      )}

      {!failure && view && !view.stranded && accounts.length === 0 && (
        <Card variant="content">
          {view.bootstrapAvailable ? (
            <EmptyState
              title="No accounts yet — create the first operator"
              message="An account is a person this server's OAuth door can authenticate: they sign in, grant consent, and own connectors. This door has never had one, so this first account can be created without an operator and is created as an operator itself. That is a one-time path — once any account exists it closes permanently."
              action={{ label: 'Create the first operator', onClick: () => setCreating(true) }}
            />
          ) : (
            <EmptyState
              title="No accounts"
              message="This view is empty. The first-account path is not open on this deployment, so an existing operator must create an account."
            />
          )}
        </Card>
      )}

      {!failure && view && !view.stranded && accounts.length > 0 && (
        <>
          {askActor && (
            <Card variant="content">
              <div style={{ display: 'flex', flexDirection: 'column', gap: 'var(--space-2)' }}>
                <span style={{ fontSize: 'var(--fs-sm)', color: 'var(--text-200)' }}>
                  Several operators are active on this deployment, so every administrative action
                  must say which one is performing it. It is recorded against that account and is
                  never guessed.
                </span>
                <div style={{ display: 'flex', flexWrap: 'wrap', gap: 'var(--space-2)', alignItems: 'center' }}>
                  <span style={{ fontSize: 'var(--fs-sm)', color: 'var(--text-500)' }}>acting as:</span>
                  {operatorNames.map(n => (
                    <Button
                      key={n}
                      size="sm"
                      variant={actor === n ? 'secondary' : 'ghost'}
                      onClick={() => setActor(n)}
                      aria-pressed={actor === n}
                    >
                      {n}
                    </Button>
                  ))}
                </div>
              </div>
            </Card>
          )}

          {actionFailure && (
            <Card variant="content">
              <span style={{ fontSize: 'var(--fs-sm)', color: 'var(--status-error)' }}>{actionFailure}</span>
            </Card>
          )}

          <Toolbar right={<ResultCount count={filtered.length} noun="account" />}>
            <SearchInput value={query} onChange={setQuery} placeholder="Search accounts…" ariaLabel="Search accounts" />
            <RoleGate>
              <Button size="sm" variant="primary" onClick={() => setCreating(true)}>New account</Button>
            </RoleGate>
          </Toolbar>
          <Card variant="content" padding="var(--space-2)">
            <SortableTable
              columns={columns}
              rows={filtered}
              rowKey={a => a.id}
              initialSort={{ key: 'account', dir: 'asc' }}
              pageSize={25}
              emptyMessage="No account matches this search"
            />
          </Card>
        </>
      )}

      <AccountCreateDialog
        open={creating}
        accounts={accounts}
        bootstrap={view?.bootstrapAvailable ?? false}
        onCancel={() => setCreating(false)}
        onDone={() => {
          setCreating(false);
          // Reload rather than splicing the created account in: the create may have been the
          // bootstrap, which changes `bootstrapAvailable` and the actor requirement as well as
          // the list. Re-asking is the only way those stay consistent with the server.
          load();
        }}
      />
    </PanelRoot>
  );
}
