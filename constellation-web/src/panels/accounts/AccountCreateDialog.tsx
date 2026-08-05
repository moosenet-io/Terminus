// TERM #654: the account creation flow, including the ONE call that can run before any account
// exists.
//
// ── THE PASSWORD IS NEVER SHOWN BACK, AND THERE IS NOTHING TO SHOW ────────────────────────────
//
// This is deliberately NOT `ClientCreateDialog`'s "shown once" ceremony, and the difference is
// worth stating so nobody adds one by analogy. A client secret is minted by the SERVER, so the
// creating operator is the only person who will ever see it and the ceremony is what stops it
// being lost. A password is the operator's OWN input: they already have it, the server keeps only
// an argon2id hash, and echoing it back would add a disclosure that buys nothing.
//
// So the value lives in this component's state for the life of one submit and is cleared on every
// exit path — success, cancel, and failure alike. It is never put in `client.prefs` (the app's
// only browser storage), never in a URL, never in a toast, never in the success copy, and never
// re-read: no tool returns it. The confirmation names the ACCOUNT, never the credential.
//
// ── THE FIRST VISIT IS THE HARD CASE ──────────────────────────────────────────────────────────
//
// On a door with no accounts this dialog IS the bootstrap: the server permits an unauthenticated
// first-account creation exactly while `rmcp_account` is empty, and the account it creates is
// always an operator. The dialog says so, in those words, because an operator who does not
// realise this is the one-shot path may spend it on a delegated account — which it will not do,
// but only because the SERVER forces the operator flag, not because the form does.
//
// The operator toggle is therefore hidden (not merely disabled) while bootstrapping: offering a
// control whose value the server overrides would be a lie about who is deciding.
//
// ── `actor` IS ASKED, NEVER INFERRED (TERM #647's rule, applied here) ─────────────────────────
//
// The server requires an explicit `actor` when several operators are active, and refuses rather
// than picking one. This dialog asks in exactly that case and leaves the field EMPTY — no
// default, no most-recently-used, nothing read from the session, and no auto-fill from a sole
// candidate. There is no `owner` field here, so the specific auto-copy defect TERM #647 fixed
// cannot recur; the rule it established (a value the server refuses to guess must not be guessed
// by the GUI either) is what carries over.
import { useCallback, useEffect, useRef, useState } from 'react';
import { Badge } from '../../components/Badge';
import { Button } from '../../components/Button';
import { createAccount, describeRmcpError } from '../../lib/rmcpClient';
import type { RmcpAccount, RmcpAccountCreated } from '../../types/rmcp';
import {
  accountNameProblem,
  actorIsAmbiguous,
  actorSuggestions,
  MIN_PASSWORD_LEN,
  passwordProblem,
} from './accountForm';

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

export interface AccountCreateDialogProps {
  open: boolean;
  /** The current accounts, used ONLY to decide whether to ask for an actor and what to suggest.
   *  Never to decide what may be done — that is the server's. */
  accounts: RmcpAccount[];
  /** True when this door has never had an account, so this call is the one-shot bootstrap. */
  bootstrap: boolean;
  onDone: (created: RmcpAccountCreated) => void;
  onCancel: () => void;
}

export function AccountCreateDialog({
  open,
  accounts,
  bootstrap,
  onDone,
  onCancel,
}: AccountCreateDialogProps) {
  const [name, setName] = useState('');
  const [password, setPassword] = useState('');
  const [operator, setOperator] = useState(false);
  // EMPTY, and stays empty until the operator says otherwise. See the module doc.
  const [actor, setActor] = useState('');
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<string | null>(null);
  const [created, setCreated] = useState<RmcpAccountCreated | null>(null);
  // Discriminates a resolved in-flight create from an abandoned one, exactly as
  // ClientCreateDialog does: this component stays MOUNTED while closed (`open` gates rendering),
  // so without this a create that landed after cancel would write back into a dialog the
  // operator had walked away from.
  const generation = useRef(0);

  const reset = useCallback(() => {
    generation.current += 1;
    setName('');
    // The one that matters: the plaintext does not outlive the flow, on ANY exit path.
    setPassword('');
    setOperator(false);
    setActor('');
    setBusy(false);
    setFailure(null);
    setCreated(null);
  }, []);

  // Closing from anywhere clears everything. Written as an effect on `open` rather than in the
  // cancel handler so a close driven by the parent (a route change, a reload) clears it too.
  useEffect(() => {
    if (!open) reset();
  }, [open, reset]);

  if (!open) return null;

  const askActor = actorIsAmbiguous(accounts);
  const suggestions = actorSuggestions(accounts);
  const nameProblem = accountNameProblem(name);
  const pwProblem = passwordProblem(password);
  // The actor requirement is the SERVER's; this only avoids a round trip that would certainly
  // be refused. Leaving it out would produce a correct refusal, not a wrong account.
  const actorProblem = askActor && actor.trim().length === 0
    ? 'Several operators are active, so this action must name the one performing it.'
    : null;
  const blocked = nameProblem ?? pwProblem ?? actorProblem;

  function submit() {
    if (blocked || busy) return;
    setBusy(true);
    setFailure(null);
    const attempt = ++generation.current;
    createAccount({
      actor: actor.trim() || undefined,
      account: name.trim(),
      password,
      operator: bootstrap ? true : operator,
    })
      .then(result => {
        if (attempt !== generation.current) return;
        // Cleared the moment it is no longer needed, before anything renders the result.
        setPassword('');
        setCreated(result);
      })
      .catch(e => {
        if (attempt !== generation.current) return;
        // CLEARED HERE TOO. Round 2 (codex): the module doc promised the plaintext does not
        // outlive the flow on any exit path, and the rejection path did not clear it — a claim
        // written twice that disagreed with itself. It costs the operator a retype after a
        // refusal (usually a duplicate name), which is the right side of that trade: the
        // invariant is worth more than the keystrokes, and a rule with one quiet exception is
        // not a rule.
        setPassword('');
        // A refusal is SHOWN. The server's message may name the account or the requirement; it
        // never carries the password, because nothing sent it one to carry.
        setFailure(describeRmcpError(e).message);
      })
      .finally(() => {
        if (attempt !== generation.current) return;
        setBusy(false);
      });
  }

  return (
    <div
      role="dialog"
      aria-modal="true"
      aria-label={bootstrap ? 'Create the first operator account' : 'Create an account'}
      style={{
        position: 'fixed',
        inset: 0,
        background: 'var(--overlay, rgba(0,0,0,0.5))',
        display: 'flex',
        alignItems: 'center',
        justifyContent: 'center',
        padding: 'var(--space-4)',
        zIndex: 50,
      }}
    >
      <div
        style={{
          background: 'var(--bg-surface)',
          border: 'var(--border-width) solid var(--border)',
          borderRadius: 'var(--radius-md)',
          padding: 'var(--space-5)',
          width: 'min(560px, 100%)',
          maxHeight: '90vh',
          overflowY: 'auto',
          display: 'flex',
          flexDirection: 'column',
          gap: 'var(--space-4)',
        }}
      >
        <div style={{ display: 'flex', alignItems: 'center', gap: 'var(--space-3)' }}>
          <h2 style={{ margin: 0, fontSize: 'var(--fs-lg)', color: 'var(--text-100)' }}>
            {bootstrap ? 'Create the first operator account' : 'Create an account'}
          </h2>
          {bootstrap && <Badge tone="amber" dot>one time only</Badge>}
        </div>

        {created ? (
          <>
            {/* Names the ACCOUNT. There is deliberately no credential here to reveal. */}
            <p style={{ margin: 0, fontSize: 'var(--fs-sm)', color: 'var(--text-200)' }}>
              <code style={{ fontFamily: 'var(--font-mono)' }}>{created.account}</code>{' '}
              {created.operator ? 'was created as an operator.' : 'was created.'}{' '}
              {created.bootstrap
                ? 'This deployment now has an operator, and the first-account path is closed permanently — every further account must be created by an operator.'
                : 'It can sign in at the OAuth door and be named as a connector owner.'}
            </p>
            <p style={{ margin: 0, fontSize: 'var(--fs-sm)', color: 'var(--text-400)' }}>
              The password is stored only as an argon2id hash and is not recoverable from here or
              anywhere else. If it is lost, an operator must set a new one.
            </p>
            <div style={{ display: 'flex', justifyContent: 'flex-end' }}>
              <Button variant="primary" onClick={() => onDone(created)}>Done</Button>
            </div>
          </>
        ) : (
          <>
            {bootstrap && (
              <p style={{ margin: 0, fontSize: 'var(--fs-sm)', color: 'var(--text-200)' }}>
                This door has no accounts, so this one may be created without an operator — and it
                is the last time that is true. It is created as an operator, and from then on every
                account is created by an operator.
              </p>
            )}

            <div style={{ display: 'flex', flexDirection: 'column', gap: 'var(--space-1)' }}>
              <label htmlFor="account-name" style={labelStyle}>Account name</label>
              <input
                id="account-name"
                value={name}
                onChange={e => setName(e.target.value)}
                disabled={busy}
                spellCheck={false}
                autoComplete="off"
                style={inputStyle}
              />
              <span style={{ fontSize: 'var(--fs-sm)', color: 'var(--text-400)' }}>
                What this person types when signing in, and the name a connector is owned by.
              </span>
            </div>

            <div style={{ display: 'flex', flexDirection: 'column', gap: 'var(--space-1)' }}>
              <label htmlFor="account-password" style={labelStyle}>Password</label>
              <input
                id="account-password"
                type="password"
                value={password}
                onChange={e => setPassword(e.target.value)}
                disabled={busy}
                spellCheck={false}
                // Explicitly opted out of every browser mechanism that would persist or
                // suggest this value: it is a credential being SET, not one being entered.
                autoComplete="new-password"
                style={inputStyle}
              />
              <span
                style={{
                  fontSize: 'var(--fs-sm)',
                  color: pwProblem && password.length > 0 ? 'var(--status-warning)' : 'var(--text-400)',
                }}
              >
                {password.length > 0 && pwProblem
                  ? pwProblem
                  : `At least ${MIN_PASSWORD_LEN} characters. Hashed with argon2id before storage; it is never shown again.`}
              </span>
            </div>

            {!bootstrap && (
              <label style={{ display: 'flex', alignItems: 'center', gap: 'var(--space-2)' }}>
                <input
                  type="checkbox"
                  checked={operator}
                  onChange={e => setOperator(e.target.checked)}
                  disabled={busy}
                />
                <span style={{ fontSize: 'var(--fs-sm)', color: 'var(--text-200)' }}>
                  Grant fleet-operator authority (can administer accounts, connectors and
                  delegations). Leave unchecked for an ordinary account.
                </span>
              </label>
            )}

            {askActor && (
              <div style={{ display: 'flex', flexDirection: 'column', gap: 'var(--space-1)' }}>
                <label htmlFor="account-actor" style={labelStyle}>Acting as</label>
                <input
                  id="account-actor"
                  value={actor}
                  onChange={e => setActor(e.target.value)}
                  disabled={busy}
                  spellCheck={false}
                  autoComplete="off"
                  style={inputStyle}
                />
                <span style={{ fontSize: 'var(--fs-sm)', color: 'var(--text-400)' }}>
                  Several operators are active, so this action must say which one is performing it.
                  It is recorded against that account.
                </span>
                {suggestions.length > 0 && (
                  <div style={{ display: 'flex', flexWrap: 'wrap', gap: 'var(--space-2)', alignItems: 'center' }}>
                    <span style={{ fontSize: 'var(--fs-sm)', color: 'var(--text-500)' }}>operators:</span>
                    {suggestions.map(s => (
                      <Button
                        key={s}
                        size="sm"
                        variant={actor === s ? 'secondary' : 'ghost'}
                        disabled={busy}
                        onClick={() => setActor(s)}
                        aria-pressed={actor === s}
                      >
                        {s}
                      </Button>
                    ))}
                  </div>
                )}
              </div>
            )}

            {failure && (
              <span style={{ fontSize: 'var(--fs-sm)', color: 'var(--status-error)' }}>{failure}</span>
            )}

            <div style={{ display: 'flex', justifyContent: 'flex-end', gap: 'var(--space-2)' }}>
              <Button variant="ghost" onClick={onCancel} disabled={busy}>Cancel</Button>
              <Button variant="primary" onClick={submit} disabled={!!blocked || busy}>
                {busy ? 'Creating…' : bootstrap ? 'Create the first operator' : 'Create account'}
              </Button>
            </div>
            {blocked && (
              <span style={{ fontSize: 'var(--fs-sm)', color: 'var(--text-500)', textAlign: 'right' }}>
                {blocked}
              </span>
            )}
          </>
        )}
      </div>
    </div>
  );
}
