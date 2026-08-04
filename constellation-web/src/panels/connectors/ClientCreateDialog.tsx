// RMCP-13 (TERM-624): the creation flow, including the ONE showing of the client secret.
//
// THE SECRET IS SHOWN EXACTLY ONCE, and this component is built around making that true rather
// than merely stating it:
//   • The value exists only in this component's state, only for the life of the "created"
//     step, and is dropped when the dialog closes. It is never written to `client.prefs`
//     (the app's only browser storage, which is allowlisted to two non-secret keys), never put
//     in a URL, and never re-read: no read tool returns it, because the server stores only an
//     argon2id hash (RMCP-08).
//   • Closing requires an explicit acknowledgement, so it cannot be dismissed by reflex before
//     the secret has been copied.
//   • CANCELLING CLEARS EVERYTHING, and an in-flight create that resolves after the dialog closed
//     is DISCARDED rather than written back (review round 2). Two bugs hid there: the component
//     is kept mounted while closed (`open` gates rendering, not mounting), so without an explicit
//     reset a cancelled flow's secret stayed in state and could reappear on reopen; and a create
//     that landed after close would have set that state from a stale promise. A one-time secret
//     surviving a flow the operator abandoned is the last thing you want lingering in memory.
//   • The statement "this is the only time you will see this" is shown next to the value, not
//     buried in help text, because a secret the operator did not save means minting a new one.
//
// A public client (no secret) skips that step entirely — inventing a ceremony for a value that
// does not exist would teach the operator to click through the one that matters.
import { useCallback, useEffect, useRef, useState } from 'react';
import { Badge } from '../../components/Badge';
import { Button } from '../../components/Button';
import { describeRmcpError, createClient } from '../../lib/rmcpClient';
import type { RmcpClient, RmcpServer, RmcpToolGroup } from '../../types/rmcp';
import { MultiSelect } from './MultiSelect';
import type { MultiSelectOption } from './MultiSelect';
import { parseLines, redirectUriHints, serverUnassignableReason } from './connectorForm';

const mono = { fontFamily: 'var(--font-mono)', fontSize: 'var(--fs-mono-sm)' } as const;

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

const labelStyle = {
  fontFamily: 'var(--font-mono)',
  fontSize: 'var(--fs-mono-sm)',
  letterSpacing: 'var(--ls-mono)',
  textTransform: 'uppercase',
  color: 'var(--text-500)',
} as const;

export interface ClientCreateDialogProps {
  open: boolean;
  groups: RmcpToolGroup[];
  servers: RmcpServer[];
  /** Called once the operator has acknowledged the result, with the created client. */
  onDone: (created: RmcpClient) => void;
  onCancel: () => void;
}

export function ClientCreateDialog({ open, groups, servers, onDone, onCancel }: ClientCreateDialogProps) {
  const [name, setName] = useState('');
  const [redirectText, setRedirectText] = useState('');
  const [confidential, setConfidential] = useState(false);
  const [groupIds, setGroupIds] = useState<string[]>([]);
  const [namespaces, setNamespaces] = useState<string[]>([]);
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<string | null>(null);
  const [created, setCreated] = useState<RmcpClient | null>(null);
  // Held in component state only, for the life of this step. See the module doc.
  const [secret, setSecret] = useState<string | null>(null);
  const [acknowledged, setAcknowledged] = useState(false);
  const [copied, setCopied] = useState(false);
  /** Bumped on every close/cancel. A create resolving with a stale generation is discarded —
   *  including its secret, which is never written into state. */
  const generation = useRef(0);

  const reset = useCallback(() => {
    setName('');
    setRedirectText('');
    setConfidential(false);
    setGroupIds([]);
    setNamespaces([]);
    setCreated(null);
    setSecret(null);
    setAcknowledged(false);
    setCopied(false);
    setFailure(null);
    setBusy(false);
  }, []);

  /** Cancel: invalidate any in-flight create FIRST, then clear, then tell the parent. Order
   *  matters — bumping after the reset would let a promise that resolves in between repopulate
   *  the state we just cleared. */
  const cancel = useCallback(() => {
    generation.current += 1;
    reset();
    onCancel();
  }, [reset, onCancel]);

  // The dialog stays MOUNTED while closed (see `if (!open) return null` below — that is a render
  // guard, not an unmount), so closing by any route other than `cancel` (the parent flipping
  // `open`, an Escape handled upstream, a re-render after the parent's own state change) must
  // still clear. This is the backstop that makes "cleared on close" true for every route.
  useEffect(() => {
    if (!open) {
      generation.current += 1;
      reset();
    }
  }, [open, reset]);

  // Unmount: invalidate so a late resolution cannot call setState on a dead component.
  useEffect(() => () => { generation.current += 1; }, []);

  if (!open) return null;

  const redirectUris = parseLines(redirectText);
  const hints = redirectUriHints(redirectUris);

  const submit = () => {
    const attempt = generation.current;
    setBusy(true);
    setFailure(null);
    createClient({ name: name.trim(), redirectUris, confidential, toolGroupIds: groupIds, namespaces })
      .then(result => {
        // Cancelled (or unmounted) while this was in flight: drop the result on the floor. The
        // connector still exists server-side — the operator will find it in the list — but its
        // secret is deliberately NOT surfaced here, because a secret shown outside the
        // acknowledge-once flow is a secret nobody stored. Never written to state either way.
        if (attempt !== generation.current) return;
        setCreated(result.client);
        setSecret(result.clientSecret);
      })
      .catch(e => {
        if (attempt !== generation.current) return;
        setFailure(describeRmcpError(e).message);
      })
      .finally(() => {
        if (attempt !== generation.current) return;
        setBusy(false);
      });
  };

  const finish = () => {
    const c = created;
    generation.current += 1;
    reset();
    if (c) onDone(c);
  };

  const copySecret = () => {
    if (!secret) return;
    // Clipboard access can be denied (insecure context, permission) — the value stays visible
    // and selectable either way, so a failed copy is never a lost secret.
    navigator.clipboard?.writeText(secret).then(
      () => setCopied(true),
      () => setCopied(false),
    );
  };

  const groupOptions: MultiSelectOption[] = groups.map(g => ({
    value: g.id,
    label: g.name,
    detail: g.patterns.join(' · ') || 'no patterns — matches nothing',
  }));
  const serverOptions: MultiSelectOption[] = servers.map(s => ({
    value: s.namespace,
    label: s.namespace,
    detail: s.available ? `${s.toolCount ?? 0} tools` : 'upstream not answering',
    unavailable: !s.available,
    disabledReason: serverUnassignableReason(s),
  }));

  return (
    <div
      role="presentation"
      onClick={created ? undefined : cancel}
      style={{
        position: 'fixed',
        inset: 0,
        background: 'rgba(13,11,26,0.65)',
        display: 'flex',
        alignItems: 'center',
        justifyContent: 'center',
        zIndex: 1000,
        padding: 'var(--space-4)',
      }}
    >
      <div
        role="dialog"
        aria-modal="true"
        aria-label="Create connector"
        onClick={e => e.stopPropagation()}
        style={{
          width: 'min(44rem, 100%)',
          maxHeight: '85vh',
          overflowY: 'auto',
          background: 'var(--grad-card)',
          border: 'var(--border-width) solid var(--border-strong)',
          borderRadius: 'var(--radius-lg)',
          boxShadow: 'var(--shadow-lg), var(--inset-hi)',
          padding: 'var(--space-5)',
          display: 'flex',
          flexDirection: 'column',
          gap: 'var(--space-4)',
        }}
      >
        {!created ? (
          <>
            <div style={{ fontSize: 'var(--fs-h4)', color: 'var(--text-100)' }}>New connector</div>
            <div style={{ fontSize: 'var(--fs-sm)', color: 'var(--text-300)' }}>
              A connector reaches a tool only when it is assigned BOTH a tool group that matches the tool and
              the server that publishes it. Leave either empty and it reaches nothing — you can scope it later.
            </div>

            <label style={{ display: 'flex', flexDirection: 'column', gap: 'var(--space-1)' }}>
              <span style={labelStyle}>name</span>
              <input value={name} onChange={e => setName(e.target.value)} style={inputStyle} placeholder="Reading assistant" />
            </label>

            <label style={{ display: 'flex', flexDirection: 'column', gap: 'var(--space-1)' }}>
              <span style={labelStyle}>redirect URIs (one per line)</span>
              <textarea
                value={redirectText}
                onChange={e => setRedirectText(e.target.value)}
                rows={3}
                spellCheck={false}
                style={{ ...inputStyle, resize: 'vertical' }}
              />
            </label>
            {hints.length > 0 && (
              <ul style={{ margin: 0, paddingLeft: 'var(--space-4)', color: 'var(--status-warning)', fontSize: 'var(--fs-sm)' }}>
                {hints.map(h => (
                  <li key={h.uri}>
                    <code style={mono}>{h.uri}</code> — {h.hint}
                  </li>
                ))}
              </ul>
            )}

            <label style={{ display: 'flex', alignItems: 'center', gap: 'var(--space-2)', fontSize: 'var(--fs-sm)', color: 'var(--text-200)' }}>
              <input type="checkbox" checked={confidential} onChange={e => setConfidential(e.target.checked)} style={{ accentColor: 'var(--accent)' }} />
              Mint a client secret (confidential client). Leave off for a public client that uses PKCE only.
            </label>

            <MultiSelect
              legend="tool groups"
              options={groupOptions}
              selected={groupIds}
              onChange={setGroupIds}
              emptyMessage="No tool groups exist yet — create one on the Tool groups tab first."
            />
            <MultiSelect
              legend="servers / namespaces"
              options={serverOptions}
              selected={namespaces}
              onChange={setNamespaces}
              emptyMessage="No servers are visible to this session."
            />

            {failure && <div style={{ color: 'var(--status-error)', fontSize: 'var(--fs-sm)' }}>{failure}</div>}

            <div style={{ display: 'flex', gap: 'var(--space-3)', justifyContent: 'flex-end' }}>
              <Button variant="ghost" onClick={cancel} disabled={busy}>Cancel</Button>
              <Button variant="primary" onClick={submit} disabled={busy || name.trim().length === 0}>
                {busy ? 'Creating…' : 'Create connector'}
              </Button>
            </div>
          </>
        ) : (
          <>
            <div style={{ fontSize: 'var(--fs-h4)', color: 'var(--text-100)' }}>{created.name} created</div>

            <div style={{ display: 'flex', flexDirection: 'column', gap: 'var(--space-1)' }}>
              <span style={labelStyle}>client id</span>
              <code style={{ ...mono, color: 'var(--text-100)' }}>{created.clientId}</code>
            </div>

            {secret ? (
              <div
                style={{
                  display: 'flex',
                  flexDirection: 'column',
                  gap: 'var(--space-2)',
                  padding: 'var(--space-3)',
                  borderRadius: 'var(--radius-sm)',
                  border: 'var(--border-width) solid var(--line-accent)',
                  background: 'var(--accent-soft)',
                }}
              >
                <div style={{ display: 'flex', alignItems: 'center', gap: 'var(--space-2)' }}>
                  <Badge tone="amber" dot>shown once</Badge>
                  <span style={{ fontSize: 'var(--fs-sm)', color: 'var(--text-100)' }}>
                    This is the only time you will see this client secret. It is stored only as a hash and
                    cannot be retrieved again — if you lose it, you must create a new connector.
                  </span>
                </div>
                <code
                  style={{
                    ...mono,
                    color: 'var(--text-100)',
                    wordBreak: 'break-all',
                    userSelect: 'all',
                    padding: 'var(--space-2)',
                    borderRadius: 'var(--radius-xs)',
                    background: 'var(--bg-elevated)',
                  }}
                >
                  {secret}
                </code>
                <div style={{ display: 'flex', alignItems: 'center', gap: 'var(--space-3)' }}>
                  <Button size="sm" variant="secondary" onClick={copySecret}>Copy secret</Button>
                  {copied && <span style={{ fontSize: 'var(--fs-sm)', color: 'var(--status-success)' }}>Copied to clipboard</span>}
                </div>
                <label style={{ display: 'flex', alignItems: 'center', gap: 'var(--space-2)', fontSize: 'var(--fs-sm)', color: 'var(--text-200)' }}>
                  <input
                    type="checkbox"
                    checked={acknowledged}
                    onChange={e => setAcknowledged(e.target.checked)}
                    style={{ accentColor: 'var(--accent)' }}
                  />
                  I have stored this secret somewhere safe.
                </label>
              </div>
            ) : (
              <div style={{ fontSize: 'var(--fs-sm)', color: 'var(--text-300)' }}>
                This is a public client, so no secret was minted — it authenticates with PKCE alone.
              </div>
            )}

            <div style={{ display: 'flex', justifyContent: 'flex-end' }}>
              <Button variant="primary" onClick={finish} disabled={Boolean(secret) && !acknowledged}>
                Done
              </Button>
            </div>
          </>
        )}
      </div>
    </div>
  );
}
