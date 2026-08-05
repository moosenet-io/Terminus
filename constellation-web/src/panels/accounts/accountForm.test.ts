// TERM #654: the account form's rules, tested as rules.
//
// Each test below names the mutation that turns it red. The one property these CANNOT establish
// is the one that matters most — that the deployment cannot be stranded — because that is the
// server's guard and is tested in `src/oauth/store.rs`
// (`demoting_the_last_operator_is_refused_and_rolled_back`,
// `the_disable_path_cannot_be_used_to_reopen_the_bootstrap`). What is tested here is only that
// the page REFLECTS it accurately. Saying so explicitly is the point: a UI test that appeared to
// cover the stranding rule would be exactly the fake guard this file's module doc warns about.
import { describe, expect, it } from 'vitest';
import {
  accountNameProblem,
  actorIsAmbiguous,
  actorSuggestions,
  activeOperators,
  MIN_PASSWORD_LEN,
  passwordProblem,
  wouldStrandTheDoor,
} from './accountForm';
import type { RmcpAccount } from '../../types/rmcp';

function account(over: Partial<RmcpAccount> & { account: string }): RmcpAccount {
  return {
    id: over.account,
    operator: false,
    disabled: false,
    createdAt: '2026-01-01T00:00:00Z',
    ...over,
  };
}

describe('passwordProblem', () => {
  it('refuses an empty password and one below the floor, and accepts one at it', () => {
    expect(passwordProblem('')).toMatch(/required/);
    expect(passwordProblem('x'.repeat(MIN_PASSWORD_LEN - 1))).toMatch(/At least/);
    // Exactly at the boundary is ACCEPTED — without this the test passes against an
    // off-by-one floor, or against any floor at all.
    expect(passwordProblem('x'.repeat(MIN_PASSWORD_LEN))).toBeNull();
  });

  it('does not trim, so a password of spaces is judged on its real length', () => {
    // Mutation-verify: add `.trim()` in `passwordProblem` and this goes red. A trimming form
    // would ACCEPT this (12 chars) and then create an account whose owner cannot log in with
    // the string they were given, because the server does not trim.
    expect(passwordProblem(' '.repeat(MIN_PASSWORD_LEN))).toBeNull();
    expect(passwordProblem(`  ${'x'.repeat(MIN_PASSWORD_LEN - 4)}  `)).toBeNull();
  });

  it('counts characters, not bytes', () => {
    // A byte floor would accept fewer non-ASCII characters than ASCII ones, for no reason.
    expect(passwordProblem('é'.repeat(MIN_PASSWORD_LEN - 1))).toMatch(/At least/);
    expect(passwordProblem('é'.repeat(MIN_PASSWORD_LEN))).toBeNull();
  });

  it('never quotes the submitted password back', () => {
    const secret = 'correct-horse-battery';
    expect(passwordProblem(secret) ?? '').not.toContain(secret);
    expect(passwordProblem('short') ?? '').not.toContain('short');
  });
});

describe('accountNameProblem', () => {
  it('requires a non-blank name', () => {
    expect(accountNameProblem('')).toMatch(/required/);
    expect(accountNameProblem('   ')).toMatch(/required/);
    expect(accountNameProblem('moose')).toBeNull();
  });
});

describe('activeOperators / actorIsAmbiguous / actorSuggestions', () => {
  it('treats a DISABLED operator as not an operator', () => {
    // Mutation-verify: drop `&& !a.disabled` from `activeOperators` and every assertion in this
    // block that involves `sidelined` goes red. It is the same rule the server applies, and
    // getting it wrong here would tell an operator they have a spare when they do not.
    const accounts = [
      account({ account: 'boss', operator: true }),
      account({ account: 'sidelined', operator: true, disabled: true }),
      account({ account: 'friend' }),
    ];
    expect(activeOperators(accounts).map(a => a.account)).toEqual(['boss']);
    expect(actorIsAmbiguous(accounts)).toBe(false);
    expect(actorSuggestions(accounts)).toEqual(['boss']);
  });

  it('reports ambiguity only when two or more operators are actually active', () => {
    const one = [account({ account: 'boss', operator: true })];
    const two = [...one, account({ account: 'aux', operator: true })];
    expect(actorIsAmbiguous(one)).toBe(false);
    expect(actorIsAmbiguous(two)).toBe(true);
    // Sorted, so the suggestion order does not depend on list order from the wire.
    expect(actorSuggestions(two)).toEqual(['aux', 'boss']);
  });

  it('reports the empty set for an empty list, not a default', () => {
    expect(activeOperators([])).toEqual([]);
    expect(actorSuggestions([])).toEqual([]);
    expect(actorIsAmbiguous([])).toBe(false);
  });
});

describe('wouldStrandTheDoor', () => {
  it('is true only for the LAST active operator', () => {
    const boss = account({ account: 'boss', operator: true });
    const aux = account({ account: 'aux', operator: true });
    const friend = account({ account: 'friend' });

    expect(wouldStrandTheDoor([boss, friend], boss)).toBe(true);
    // With a second active operator it is safe.
    expect(wouldStrandTheDoor([boss, aux, friend], boss)).toBe(false);
    // A delegated account never strands anything.
    expect(wouldStrandTheDoor([boss, friend], friend)).toBe(false);
  });

  it('does not count a disabled operator as the spare', () => {
    // Mutation-verify: change `activeOperators` to count every operator row and this goes red.
    // The false answer here is the dangerous direction — it would enable a control whose action
    // the server then refuses, teaching the operator that the page lies.
    const boss = account({ account: 'boss', operator: true });
    const sidelined = account({ account: 'sidelined', operator: true, disabled: true });
    expect(wouldStrandTheDoor([boss, sidelined], boss)).toBe(true);
  });

  it('is false for an already-disabled operator, which has nothing left to remove', () => {
    const sidelined = account({ account: 'sidelined', operator: true, disabled: true });
    const boss = account({ account: 'boss', operator: true });
    expect(wouldStrandTheDoor([boss, sidelined], sidelined)).toBe(false);
  });
});
