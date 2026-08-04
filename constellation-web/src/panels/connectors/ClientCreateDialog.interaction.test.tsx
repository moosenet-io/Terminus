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
import type { RmcpClient, RmcpClientCreated } from '../../types/rmcp';

// This repo does not wire jest-dom matchers, so assertions read the DOM directly (same posture
// as the other interaction test in src/panels/maestro/).
const nameField = () => screen.getByPlaceholderText('Reading assistant') as HTMLInputElement;
const button = (name: string) => screen.getByRole('button', { name }) as HTMLButtonElement;

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

    await user.type(nameField(), 'Late connector');
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

    await user.type(nameField(), 'Shown once');
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

    await user.type(nameField(), 'Once');
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
