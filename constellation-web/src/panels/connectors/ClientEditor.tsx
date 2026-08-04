// RMCP-13 (TERM-624): the client detail/editor — identity, scoping, and the resolved preview.
//
// Editing is a SINGLE save of the whole scope (groups + servers + redirect URIs + enabled),
// carried with the `version` the form was loaded at. If another operator saved in between, the
// server answers `version_conflict` and this surfaces it as such: the edit is NOT retried with
// a fresh version, because that is precisely how one operator silently reverts another's
// change. The operator reloads, sees the other version, and decides.
//
// `client.editable` is the server's statement about this session (RMCP-12 ownership). It
// switches this view to read-only so a delegated owner is not offered a control that would be
// refused — a courtesy, not a control. The write is refused server-side whether or not this
// component renders a button, which is why the read-only branch still shows every field: seeing
// a connector you cannot edit is fine; being unable to tell why is not.
import { useEffect, useMemo, useState } from 'react';
import { Badge } from '../../components/Badge';
import { Button } from '../../components/Button';
import { Card } from '../../components/Card';
import { ConfirmDialog } from '../../components/ConfirmDialog';
import { RoleGate } from '../../components/RoleGate';
import { StatusPill } from '../../components/StatusPill';
import { describeRmcpError, revokeClient, updateClient } from '../../lib/rmcpClient';
import type { RmcpClient, RmcpServer, RmcpToolGroup } from '../../types/rmcp';
import { MultiSelect } from './MultiSelect';
import type { MultiSelectOption } from './MultiSelect';
import { ResolvedToolPreview } from './ResolvedToolPreview';
import { SessionList } from './SessionList';
import { parseLines, redirectUriHints, sameSet } from './connectorForm';

const mono = { fontFamily: 'var(--font-mono)', fontSize: 'var(--fs-mono-sm)' } as const;

const labelStyle = {
  fontFamily: 'var(--font-mono)',
  fontSize: 'var(--fs-mono-sm)',
  letterSpacing: 'var(--ls-mono)',
  textTransform: 'uppercase',
  color: 'var(--text-500)',
} as const;

const inputStyle = {
  width: '100%',
  padding: 'var(--space-2) var(--space-3)',
  borderRadius: 'var(--radius-sm)',
  border: 'var(--border-width) solid var(--border)',
  background: 'var(--bg-elevated)',
  color: 'var(--text-100)',
  fontFamily: 'var(--font-mono)',
  fontSize: 'var(--fs-mono)',
} as const;

function Field({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div style={{ display: 'flex', flexDirection: 'column', gap: 'var(--space-1)', minWidth: 0 }}>
      <span style={labelStyle}>{label}</span>
      {children}
    </div>
  );
}

export interface ClientEditorProps {
  client: RmcpClient;
  groups: RmcpToolGroup[];
  servers: RmcpServer[];
  /** Called with the server's updated record after a successful save. */
  onSaved: (client: RmcpClient) => void;
  onRevoked: (clientRowId: string) => void;
  onBack: () => void;
  /** Reload everything — offered when a concurrent edit is detected, so the operator can see
   *  the other version before deciding what to do with their own. */
  onReload: () => void;
}

export function ClientEditor({ client, groups, servers, onSaved, onRevoked, onBack, onReload }: ClientEditorProps) {
  const [enabled, setEnabled] = useState(client.enabled);
  const [groupIds, setGroupIds] = useState<string[]>(client.toolGroupIds);
  const [namespaces, setNamespaces] = useState<string[]>(client.namespaces);
  const [redirectText, setRedirectText] = useState(client.redirectUris.join('\n'));
  const [saving, setSaving] = useState(false);
  const [failure, setFailure] = useState<{ message: string; conflict: boolean } | null>(null);
  const [confirmRevoke, setConfirmRevoke] = useState(false);
  const [revoking, setRevoking] = useState(false);
  // Bumped after a save so the preview re-resolves against what was actually stored.
  const [resolvedKey, setResolvedKey] = useState(0);

  // Reset the form whenever a different client is selected, or the record is replaced by a
  // reload — otherwise a previous client's unsaved edits would appear to belong to this one.
  useEffect(() => {
    setEnabled(client.enabled);
    setGroupIds(client.toolGroupIds);
    setNamespaces(client.namespaces);
    setRedirectText(client.redirectUris.join('\n'));
    setFailure(null);
  }, [client]);

  const redirectUris = useMemo(() => parseLines(redirectText), [redirectText]);
  const uriHints = useMemo(() => redirectUriHints(redirectUris), [redirectUris]);

  const dirty =
    enabled !== client.enabled ||
    !sameSet(groupIds, client.toolGroupIds) ||
    !sameSet(namespaces, client.namespaces) ||
    !sameSet(redirectUris, client.redirectUris);

  const groupOptions: MultiSelectOption[] = groups.map(g => ({
    value: g.id,
    label: g.name,
    detail: g.patterns.length ? g.patterns.join(' · ') : 'no patterns — matches nothing',
  }));

  const serverOptions: MultiSelectOption[] = servers.map(s => ({
    value: s.namespace,
    label: s.namespace,
    detail: s.available ? `${s.toolCount ?? 0} tools · owner ${s.ownerName ?? 'unknown'}` : 'upstream not answering',
    unavailable: !s.available,
    // Assigning a namespace you do not own is refused server-side (RMCP-12); saying so here is
    // the difference between "broken" and "ask for the delegation".
    disabledReason: s.ownedByMe ? undefined : 'you do not own this server',
  }));

  const save = () => {
    setSaving(true);
    setFailure(null);
    updateClient({
      id: client.id,
      version: client.version,
      enabled,
      redirectUris,
      toolGroupIds: groupIds,
      namespaces,
    })
      .then(updated => {
        onSaved(updated);
        setResolvedKey(k => k + 1);
      })
      .catch(e => {
        const d = describeRmcpError(e);
        setFailure({ message: d.message, conflict: d.kind === 'conflict' });
      })
      .finally(() => setSaving(false));
  };

  const revoke = () => {
    setRevoking(true);
    revokeClient(client.id)
      .then(() => {
        setConfirmRevoke(false);
        onRevoked(client.id);
      })
      .catch(e => setFailure({ message: describeRmcpError(e).message, conflict: false }))
      .finally(() => setRevoking(false));
  };

  const readOnly = !client.editable;

  return (
    <div style={{ display: 'flex', flexDirection: 'column', gap: 'var(--space-4)' }}>
      <div style={{ display: 'flex', alignItems: 'center', gap: 'var(--space-3)', flexWrap: 'wrap' }}>
        <Button size="sm" variant="ghost" onClick={onBack}>← All connectors</Button>
        <span style={{ fontSize: 'var(--fs-h4)', color: 'var(--text-100)' }}>{client.name}</span>
        <StatusPill state={client.enabled ? 'online' : 'idle'} label={client.enabled ? 'enabled' : 'disabled'} />
        <Badge tone={client.registrationSource === 'operator' ? 'violet' : 'blue'} mono>
          {client.registrationSource === 'operator' ? 'operator-minted' : 'self-registered (DCR)'}
        </Badge>
        <Badge tone="neutral" mono>{client.confidential ? 'confidential' : 'public'}</Badge>
        {readOnly && <Badge tone="amber" dot>read-only — owned by another account</Badge>}
      </div>

      {client.registrationSource === 'dcr' && client.toolGroupIds.length === 0 && (
        <Card variant="content">
          <div style={{ fontSize: 'var(--fs-sm)', color: 'var(--text-200)' }}>
            This connector registered itself and has no scope yet, so it can reach nothing. Assign the tool
            groups and servers it should have — that approval is the whole point of the scoping model, and
            nothing is granted until you make it.
          </div>
        </Card>
      )}

      {failure && (
        <Card variant="content">
          <div style={{ display: 'flex', alignItems: 'center', gap: 'var(--space-3)', flexWrap: 'wrap' }}>
            <span style={{ color: failure.conflict ? 'var(--status-warning)' : 'var(--status-error)', fontSize: 'var(--fs-sm)' }}>
              {failure.message}
            </span>
            {failure.conflict && (
              <Button size="sm" variant="secondary" onClick={onReload}>Reload this connector</Button>
            )}
          </div>
        </Card>
      )}

      <Card variant="content">
        <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(14rem, 1fr))', gap: 'var(--space-4)' }}>
          <Field label="client id">
            <code style={{ ...mono, color: 'var(--text-100)' }}>{client.clientId}</code>
          </Field>
          <Field label="created">
            <span style={{ ...mono, color: 'var(--text-300)' }}>{client.createdAt}</span>
          </Field>
          <Field label="revision">
            <span style={{ ...mono, color: 'var(--text-300)' }}>v{client.version}</span>
          </Field>
          <Field label="access">
            <label style={{ display: 'flex', alignItems: 'center', gap: 'var(--space-2)', fontSize: 'var(--fs-sm)', color: 'var(--text-200)' }}>
              <input
                type="checkbox"
                checked={enabled}
                disabled={readOnly}
                onChange={e => setEnabled(e.target.checked)}
                style={{ accentColor: 'var(--accent)' }}
              />
              enabled
            </label>
          </Field>
        </div>
      </Card>

      <Card variant="content">
        <div style={{ display: 'flex', flexDirection: 'column', gap: 'var(--space-4)' }}>
          <Field label="redirect URIs (one per line)">
            <textarea
              value={redirectText}
              readOnly={readOnly}
              onChange={e => setRedirectText(e.target.value)}
              rows={3}
              spellCheck={false}
              aria-label="Redirect URIs"
              style={{ ...inputStyle, resize: 'vertical' }}
            />
            {uriHints.length > 0 && (
              <ul style={{ margin: 0, paddingLeft: 'var(--space-4)', color: 'var(--status-warning)', fontSize: 'var(--fs-sm)' }}>
                {uriHints.map(h => (
                  <li key={h.uri}>
                    <code style={mono}>{h.uri}</code> — {h.hint}
                  </li>
                ))}
              </ul>
            )}
          </Field>

          <MultiSelect
            legend="tool groups"
            options={groupOptions}
            selected={groupIds}
            onChange={setGroupIds}
            readOnly={readOnly}
            emptyMessage="No tool groups exist yet — create one on the Tool groups tab."
          />

          <MultiSelect
            legend="servers / namespaces"
            options={serverOptions}
            selected={namespaces}
            onChange={setNamespaces}
            readOnly={readOnly}
            emptyMessage="No servers are visible to this session."
          />

          {!readOnly && (
            <div style={{ display: 'flex', gap: 'var(--space-3)', alignItems: 'center', flexWrap: 'wrap' }}>
              <RoleGate>
                <Button variant="primary" disabled={!dirty || saving} onClick={save}>
                  {saving ? 'Saving…' : 'Save scope'}
                </Button>
              </RoleGate>
              <RoleGate>
                <Button variant="danger" disabled={saving} onClick={() => setConfirmRevoke(true)}>
                  Revoke connector
                </Button>
              </RoleGate>
              {dirty && !saving && (
                <span style={{ fontSize: 'var(--fs-sm)', color: 'var(--text-400)' }}>
                  Unsaved changes — the preview below still shows the saved scope.
                </span>
              )}
            </div>
          )}
        </div>
      </Card>

      <Card variant="content">
        <ResolvedToolPreview client={client} refreshKey={resolvedKey} />
      </Card>

      {/* The grants issued against THIS connector, with the same per-row and bulk revoke the
          Sessions tab offers fleet-wide — scoped here because "cut this connector off" is a
          decision made while looking at the connector, not while scrolling a global list. */}
      <Card variant="content">
        <SessionList clientRowId={client.id} clientName={client.name} />
      </Card>

      <ConfirmDialog
        open={confirmRevoke}
        title={`Revoke ${client.name}?`}
        description="The connector is deleted and its live tokens stop working at the next call. Anything using this client id loses access immediately. This cannot be undone."
        confirmLabel="Revoke"
        destructive
        busy={revoking}
        onConfirm={revoke}
        onCancel={() => setConfirmRevoke(false)}
      />
    </div>
  );
}
