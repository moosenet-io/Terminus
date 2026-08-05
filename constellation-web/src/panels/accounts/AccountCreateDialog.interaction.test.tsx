// TERM #654, review round 4 finding 3: the password guarantees were stated in a module doc and
// pinned by nothing. Deleting the clearing on success or on failure, or rendering the value in
// the result or the error, would not have failed a single added test — the adapter tests only
// inspect fixture RESPONSES, which never carried it in the first place.
//
// That is the shape this item has been correcting all sprint: a claim written twice, where the
// second copy is a comment. So the claim moves into a test that can fail.
//
// It has to be a component test. "The plaintext does not outlive the flow" is a statement about
// component state and the DOM across a close, a success and a rejection — none of which a pure
// function can reach, and all of which are invisible from outside.
//
// Follows this repo's per-file jsdom convention (see ClientCreateDialog.interaction.test.tsx).
//
// @vitest-environment jsdom
import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { render, screen, cleanup } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { AccountCreateDialog } from './AccountCreateDialog';
import { RmcpError } from '../../lib/rmcpContract';
import type { RmcpAccount } from '../../types/rmcp';

// pii-test-fixture: a synthetic passphrase this test types into a form and then asserts is
// absent from the DOM. It authenticates nothing and exists only so the assertion has something
// recognisable to look for.
const SECRET = 'a-synthetic-passphrase-for-this-test'; // pii-test-fixture

const nameField = () => screen.getByLabelText('Account name') as HTMLInputElement;
const passwordField = () => screen.getByLabelText('Password') as HTMLInputElement;
const button = (name: string) => screen.getByRole('button', { name }) as HTMLButtonElement;

const mockCreate = vi.fn();

vi.mock('../../lib/rmcpClient', async () => {
  const contract = await import('../../lib/rmcpContract');
  return {
    createAccount: (...args: unknown[]) => mockCreate(...args),
    describeRmcpError: (e: unknown) => ({
      kind: e instanceof contract.RmcpError ? e.kind : 'error',
      message: e instanceof Error ? e.message : 'Unexpected failure',
    }),
  };
});

function accounts(): RmcpAccount[] {
  return [
    { id: 'a1', account: 'boss', operator: true, disabled: false, createdAt: '2026-01-01T00:00:00Z' },
  ];
}

/** Every password input currently in the document. The guarantee is about the DOM as well as
 *  about state — a cleared state that left the value in the input would still be a disclosure. */
function passwordValues(): string[] {
  return Array.from(document.querySelectorAll('input[type="password"]')).map(
    el => (el as HTMLInputElement).value,
  );
}

/** The whole rendered page text, for asserting the secret is nowhere in it. */
function documentText(): string {
  return document.body.textContent ?? '';
}

beforeEach(() => {
  mockCreate.mockReset();
});

afterEach(() => {
  cleanup();
});

describe('AccountCreateDialog — the password does not outlive the flow', () => {
  it('clears the password on SUCCESS and never renders it', async () => {
    const user = userEvent.setup();
    mockCreate.mockResolvedValue({ id: 'a2', account: 'new-person', operator: false, bootstrap: false });

    render(
      <AccountCreateDialog open accounts={accounts()} bootstrap={false} onDone={() => {}} onCancel={() => {}} />,
    );
    await user.type(nameField(), 'new-person');
    await user.type(passwordField(), SECRET);
    await user.click(button('Create account'));

    // The confirmation names the ACCOUNT.
    expect(await screen.findByText(/was created/i)).toBeTruthy();
    // WHAT THIS PINS, precisely — and the precision is the point, because two earlier versions
    // of this assertion claimed more than they delivered and I only found out by running the
    // mutation instead of trusting the comment:
    //
    //   • The success view does not render the credential. Genuinely pinned by the line below.
    //   • The success-path `setPassword('')` is NOT pinned here, and cannot be. Once
    //     `setCreated(...)` fires, the confirmation branch replaces the form, so the input is
    //     unmounted and no assertion in this state can observe the retained value —
    //     `passwordValues()` is `[]` and `[].every()` is vacuously true. Deleting that clear
    //     leaves every test in this file green (verified by doing it).
    //
    // That clear is therefore defence in depth rather than the guarantee: it drops the plaintext
    // a moment earlier than the close-reset would. The OBSERVABLE guarantees are this view not
    // showing it, plus the refusal and close paths below, which are pinned and which do go red.
    expect(documentText()).not.toContain(SECRET);
    expect(passwordValues()).toHaveLength(0);

    // It was sent exactly once, and only to the create call.
    expect(mockCreate).toHaveBeenCalledTimes(1);
    expect(mockCreate.mock.calls[0][0]).toMatchObject({ password: SECRET });
  });

  it('clears the password on a REFUSAL, and the refusal never quotes it', async () => {
    const user = userEvent.setup();
    mockCreate.mockRejectedValue(
      new RmcpError('conflict', 'rmcp_account_create', 'an account with that name already exists'),
    );

    render(
      <AccountCreateDialog open accounts={accounts()} bootstrap={false} onDone={() => {}} onCancel={() => {}} />,
    );
    await user.type(nameField(), 'taken');
    await user.type(passwordField(), SECRET);
    await user.click(button('Create account'));

    // The refusal is SHOWN — a dialog that fails silently is its own defect.
    expect(await screen.findByText(/already exists/i)).toBeTruthy();
    // Mutation-verify: delete `setPassword('')` from the `.catch` and this goes red. This is the
    // exact path round 3 found unguarded while the module doc claimed otherwise.
    expect(passwordValues().every(v => v === '')).toBe(true);
    expect(documentText()).not.toContain(SECRET);
  });

  it('clears the password when the dialog CLOSES, so it cannot reappear on reopen', async () => {
    const user = userEvent.setup();
    const view = render(
      <AccountCreateDialog open accounts={accounts()} bootstrap={false} onDone={() => {}} onCancel={() => {}} />,
    );
    await user.type(nameField(), 'abandoned');
    await user.type(passwordField(), SECRET);

    // The component stays MOUNTED while closed (`open` gates rendering), which is exactly why an
    // explicit reset is needed and why this is the interesting case.
    view.rerender(
      <AccountCreateDialog open={false} accounts={accounts()} bootstrap={false} onDone={() => {}} onCancel={() => {}} />,
    );
    view.rerender(
      <AccountCreateDialog open accounts={accounts()} bootstrap={false} onDone={() => {}} onCancel={() => {}} />,
    );

    expect(passwordValues().every(v => v === '')).toBe(true);
    expect(nameField().value).toBe('');
    expect(documentText()).not.toContain(SECRET);
    // Nothing was sent.
    expect(mockCreate).not.toHaveBeenCalled();
  });

  it('hides the operator toggle while bootstrapping, because the server decides that flag', async () => {
    const user = userEvent.setup();
    mockCreate.mockResolvedValue({ id: 'a1', account: 'first', operator: true, bootstrap: true });

    render(
      <AccountCreateDialog open accounts={[]} bootstrap onDone={() => {}} onCancel={() => {}} />,
    );
    // No checkbox to mislead the operator about who is deciding.
    expect(document.querySelectorAll('input[type="checkbox"]').length).toBe(0);

    await user.type(nameField(), 'first');
    await user.type(passwordField(), SECRET);
    await user.click(button('Create the first operator'));

    // `operator: true` is sent from the RULE, not from a control the form rendered.
    expect(mockCreate.mock.calls[0][0]).toMatchObject({ operator: true });
    expect(documentText()).not.toContain(SECRET);
  });

  it('refuses to submit a password below the floor, and sends nothing', async () => {
    const user = userEvent.setup();
    render(
      <AccountCreateDialog open accounts={accounts()} bootstrap={false} onDone={() => {}} onCancel={() => {}} />,
    );
    await user.type(nameField(), 'someone');
    await user.type(passwordField(), 'short');
    expect(button('Create account').disabled).toBe(true);
    await user.click(button('Create account'));
    expect(mockCreate).not.toHaveBeenCalled();
  });
});
