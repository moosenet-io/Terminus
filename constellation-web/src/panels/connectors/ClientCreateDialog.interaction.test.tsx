// RMCP-13 (TERM-624), review round 2, finding 3: the one-time client secret must not survive a
// cancelled flow, and a create that resolves AFTER the dialog closed must not write back.
//
// This has to be a component test. The claim is about the interaction between component state,
// an in-flight promise, and a close — none of which a pure-function test can reach, and the
// failure mode is invisible from the outside: a stale secret sitting in state, revealed the next
// time the dialog opens. That is precisely the class of finding a "well, the types prevent it"
// argument misses.
//
// Follows this repo's per-file jsdom convention (see TerminateControl.interaction.test.tsx):
// every other test file keeps the default `node` environment.
//
// @vitest-environment jsdom
import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { render, screen, cleanup } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { ClientCreateDialog } from './ClientCreateDialog';
import { RmcpError } from '../../lib/rmcpContract';
import type { RmcpClient, RmcpClientCreated, RmcpServer } from '../../types/rmcp';

// This repo does not wire jest-dom matchers, so assertions read the DOM directly (same posture
// as the other interaction test in src/panels/maestro/).
const nameField = () => screen.getByPlaceholderText('Reading assistant') as HTMLInputElement;
const ownerField = () => screen.getByLabelText('owner account') as HTMLInputElement;
const actorField = () => screen.getByLabelText('acting account') as HTMLInputElement;
const button = (name: string) => screen.getByRole('button', { name }) as HTMLButtonElement;

/** Fill the whole form. TERM-647 made `owner`/`actor` required, so every test that reaches the
 *  Create button has to state them — which is the behaviour, not a test-setup tax. */
async function fillForm(user: ReturnType<typeof userEvent.setup>, name: string) {
  await user.type(nameField(), name);
  await user.type(ownerField(), 'delegated-owner');
  await user.type(actorField(), 'delegated-owner');
}

const mockCreate = vi.fn();

// Only the one call this component makes is mocked — this file is about close/late-resolve
// wiring, not the transport (rmcpClient.test.ts covers that).
vi.mock('../../lib/rmcpClient', async () => {
  const actual = await vi.importActual<typeof import('../../lib/rmcpClient')>('../../lib/rmcpClient');
  return { ...actual, createClient: (...args: unknown[]) => mockCreate(...args) };
});

// A stand-in for the one-time value the create flow returns. Deliberately NOT shaped like a
// credential (no "secret"/"token" in the literal): it authenticates nothing, and a repo-wide PII
// gate is right to flag credential-shaped literals rather than have people tag exemptions.
const ONE_TIME_VALUE = 'shown-once-value-from-a-cancelled-flow';

function madeClient(): RmcpClient {
  return {
    id: 'c-new', clientId: 'cnx_new', name: 'New connector', registrationSource: 'operator',
    enabled: true, confidential: true, redirectUris: [], toolGroupIds: [], namespaces: [],
    createdAt: '2026-08-04T00:00:00Z', version: 1, editable: true,
  };
}

function created(): RmcpClientCreated {
  return { client: madeClient(), clientSecret: ONE_TIME_VALUE };
}

/** A promise plus its resolver, so a create can be left deliberately in flight across a cancel. */
function deferred<T>() {
  let resolve!: (v: T) => void;
  const promise = new Promise<T>(r => { resolve = r; });
  return { promise, resolve };
}

function Harness({ open, onCancel }: { open: boolean; onCancel: () => void }) {
  return <ClientCreateDialog open={open} groups={[]} servers={[]} onDone={() => {}} onCancel={onCancel} />;
}

beforeEach(() => mockCreate.mockReset());
afterEach(cleanup);

describe('RMCP-13: the create dialog does not leak a cancelled flow', () => {
  it('clears typed input when cancelled, so a reopen starts fresh', async () => {
    const user = userEvent.setup();
    const onCancel = vi.fn();
    const { rerender } = render(<Harness open onCancel={onCancel} />);

    await user.type(nameField(), 'Draft name');
    await user.click(button('Cancel'));
    expect(onCancel).toHaveBeenCalled();

    rerender(<Harness open={false} onCancel={onCancel} />);
    rerender(<Harness open onCancel={onCancel} />);
    expect(nameField().value).toBe('');
  });

  it('DISCARDS a create that resolves after cancel — the secret never reaches the screen', async () => {
    const user = userEvent.setup();
    const pending = deferred<RmcpClientCreated>();
    mockCreate.mockReturnValue(pending.promise);
    const onCancel = vi.fn();
    const { rerender } = render(<Harness open onCancel={onCancel} />);

    await fillForm(user, 'Late connector');
    await user.click(button('Create connector'));
    expect(mockCreate).toHaveBeenCalledTimes(1);

    // Operator cancels while the request is still out.
    await user.click(button('Cancel'));

    // ...and only THEN does the server answer, with a secret.
    pending.resolve(created());
    await Promise.resolve();
    await Promise.resolve();

    // Nothing from that response is on screen, and reopening shows a fresh form — not the
    // created client's success step and not its secret.
    rerender(<Harness open={false} onCancel={onCancel} />);
    rerender(<Harness open onCancel={onCancel} />);
    expect(screen.queryByText(ONE_TIME_VALUE)).toBeNull();
    expect(screen.queryByText(/only time you will see this/i)).toBeNull();
    expect(nameField().value).toBe('');
  });

  it('closing without pressing Cancel also clears — the render guard is not an unmount', async () => {
    const user = userEvent.setup();
    mockCreate.mockResolvedValue(created());
    const onCancel = vi.fn();
    const { rerender } = render(<Harness open onCancel={onCancel} />);

    await fillForm(user, 'Shown once');
    await user.click(button('Create connector'));
    expect(await screen.findByText(ONE_TIME_VALUE)).toBeTruthy();

    // The PARENT closes the dialog (no Cancel click) — the component stays mounted.
    rerender(<Harness open={false} onCancel={onCancel} />);
    rerender(<Harness open onCancel={onCancel} />);

    expect(screen.queryByText(ONE_TIME_VALUE)).toBeNull();
    expect(nameField().value).toBe('');
  });

  it('shows the secret exactly once, gated behind an explicit acknowledgement', async () => {
    const user = userEvent.setup();
    mockCreate.mockResolvedValue(created());
    render(<Harness open onCancel={vi.fn()} />);

    await fillForm(user, 'Once');
    await user.click(button('Create connector'));

    expect(await screen.findByText(ONE_TIME_VALUE)).toBeTruthy();
    expect(screen.getByText(/only time you will see this/i)).toBeTruthy();
    // Done stays disabled until the operator states they have stored it.
    const done = button('Done');
    expect(done.disabled).toBe(true);
    await user.click(screen.getByRole('checkbox', { name: /stored this secret/i }));
    expect(done.disabled).toBe(false);
  });
});

// ── TERM-647: the owner is CHOSEN, never guessed ─────────────────────────────────────────────
//
// The tool requires `owner` and `actor` and refuses to default either, because this surface
// authenticates a mesh principal rather than an account and so has nothing to infer an owner
// from. The dialog sent neither, and every create failed.
//
// These tests are about the SHAPE of the fix rather than the presence of a field. The tempting
// implementations — default it, preselect the only candidate, copy the owner into the actor —
// all make the create succeed while re-introducing exactly the guess the server refuses to make,
// so each is pinned as a separate failure.
function serverRow(namespace: string, ownerName: string | null): RmcpServer {
  return { namespace, ownerName, ownedByMe: ownerName === 'delegated-owner', available: true, toolCount: 1 };
}

function OwnedHarness({ servers }: { servers: RmcpServer[] }) {
  return <ClientCreateDialog open groups={[]} servers={servers} onDone={() => {}} onCancel={() => {}} />;
}

describe('TERM-647: creating a connector names its owner', () => {
  it('sends owner and actor, so the create is no longer missing a required argument', async () => {
    const user = userEvent.setup();
    mockCreate.mockResolvedValue({ client: madeClient(), clientSecret: null });
    render(<Harness open onCancel={vi.fn()} />);

    await user.type(nameField(), 'Reader');
    await user.type(ownerField(), 'delegated-owner');
    await user.type(actorField(), 'an-operator');
    await user.click(button('Create connector'));

    expect(mockCreate).toHaveBeenCalledTimes(1);
    expect(mockCreate.mock.calls[0][0]).toMatchObject({
      owner: 'delegated-owner',
      actor: 'an-operator',
      name: 'Reader',
    });
  });

  it('starts EMPTY and cannot be submitted until the operator names both accounts', async () => {
    const user = userEvent.setup();
    render(<Harness open onCancel={vi.fn()} />);

    // No default, nothing carried over, nothing read from a session.
    expect(ownerField().value).toBe('');
    expect(actorField().value).toBe('');

    await user.type(nameField(), 'Reader');
    expect(button('Create connector').disabled).toBe(true);
    await user.type(ownerField(), 'delegated-owner');
    expect(button('Create connector').disabled).toBe(true);
    await user.type(actorField(), 'delegated-owner');
    expect(button('Create connector').disabled).toBe(false);
    expect(mockCreate).not.toHaveBeenCalled();
  });

  it('does not accept whitespace as a choice', async () => {
    const user = userEvent.setup();
    render(<Harness open onCancel={vi.fn()} />);
    await user.type(nameField(), 'Reader');
    await user.type(ownerField(), '   ');
    await user.type(actorField(), '   ');
    expect(button('Create connector').disabled).toBe(true);
  });

  it('does NOT preselect a sole candidate — one option is still a choice to make', async () => {
    const user = userEvent.setup();
    render(<OwnedHarness servers={[serverRow('media', 'delegated-owner')]} />);

    // Exactly one account is known, which is the case most likely to be "helpfully" auto-filled.
    // It is offered, not applied: the request must carry a value the human actually picked.
    expect(ownerField().value).toBe('');
    const suggestions = screen.getAllByRole('button', { name: 'delegated-owner' });
    expect(suggestions.length).toBe(2); // one per account field

    await user.click(suggestions[0]);
    expect(ownerField().value).toBe('delegated-owner');
    // ...and the click filled ONLY the field it belongs to. Auto-filling the actor from the owner
    // would defeat the server's operator-authority check, which only bites when they differ.
    expect(actorField().value).toBe('');
  });

  it('offers known accounts without limiting the answer to them', async () => {
    const user = userEvent.setup();
    // The suggestions come from ownership the session can already see (`rmcp_server_owner_list`),
    // deduplicated; an UNCLAIMED namespace contributes no name. There is no account-listing tool,
    // so this list is routinely incomplete — a typed name has to work.
    render(<OwnedHarness servers={[
      serverRow('media', 'delegated-owner'),
      serverRow('home', 'delegated-owner'),
      serverRow('studio', 'studio-owner'),
      serverRow('lab', null),
    ]} />);
    expect(screen.getAllByRole('button', { name: 'delegated-owner' })).toHaveLength(2);
    expect(screen.getAllByRole('button', { name: 'studio-owner' })).toHaveLength(2);

    await user.type(ownerField(), 'someone-not-listed');
    expect(ownerField().value).toBe('someone-not-listed');
  });

  it('SURFACES a refusal instead of failing silently', async () => {
    const user = userEvent.setup();
    // `mockRejectedValueOnce`, NOT the persistent `mockRejectedValue`: the persistent form
    // builds its rejected promise eagerly at setup, so vitest flags it as unhandled before the
    // click attaches the component's `.catch` a task later. The failure looks like the component
    // swallowing an error and is nothing of the sort — verified by reducing it to a five-line
    // widget. The repo's other rejection test (useMuse.refetchContract) uses `Once` for the same
    // reason. One click, one rejection, so `Once` is also the honest description.
    mockCreate.mockRejectedValueOnce(new RmcpError('not_found', 'rmcp_client_create', 'no such account'));
    render(<Harness open onCancel={vi.fn()} />);

    await fillForm(user, 'Reader');
    await user.click(button('Create connector'));

    // Named in terms of the accounts the operator typed — the generic `not_found` copy ("this
    // object no longer exists, reload") would send them to re-check a connector list that is
    // fine, and leave the typo in the field they were looking at.
    expect(await screen.findByText(/No such account/i)).toBeTruthy();
    // Still on the form, with the input intact, so the fix is one keystroke away.
    expect(nameField().value).toBe('Reader');
    expect(button('Create connector').disabled).toBe(false);
  });

  it('explains a refused pairing as an authority problem, not a broken page', async () => {
    const user = userEvent.setup();
    mockCreate.mockRejectedValueOnce(
      new RmcpError('forbidden', 'rmcp_client_create', 'only an operator may create for another account'),
    );
    render(<Harness open onCancel={vi.fn()} />);

    await fillForm(user, 'Reader');
    await user.click(button('Create connector'));
    // Matched on the refusal's own opening words, not on "operator authority" alone — that
    // phrase also appears in the acting-account field's hint, so the looser query matches the
    // help text and would pass even if the refusal were never shown.
    expect(await screen.findByText(/The server refused this pairing/i)).toBeTruthy();
  });

  it('clears the chosen accounts on cancel, so the next flow re-asks', async () => {
    const user = userEvent.setup();
    const { rerender } = render(<Harness open onCancel={vi.fn()} />);
    await user.type(ownerField(), 'delegated-owner');
    await user.click(button('Cancel'));
    rerender(<Harness open={false} onCancel={vi.fn()} />);
    rerender(<Harness open onCancel={vi.fn()} />);
    // A previously chosen owner surviving into the next connector would be a default by another
    // name — and one the operator never re-confirmed.
    expect(ownerField().value).toBe('');
  });
});
