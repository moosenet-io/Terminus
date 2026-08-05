// TERM #654: the Accounts page's pure logic — no React, no I/O, so the rules below are testable
// as rules rather than through a rendered tree.
//
// ── WHAT IS AND IS NOT DECIDED HERE ───────────────────────────────────────────────────────────
//
// Nothing in this file is a control. Every function is a *reflection* of a server rule, used to
// decide what to render and what to say — and the distinction is the whole reason the file has a
// module doc. This sprint shipped eight guards that could not fail; a disabled button is the
// easiest one of those to write by accident, because it LOOKS like enforcement from every angle
// except the one that matters.
//
// Concretely: `wouldStrandTheDoor` is used to disable a control and to explain WHY. It is not
// what stops the deployment being stranded. The server refuses to remove the last active
// operator inside the transaction that would have done it, and it refuses whether or not this
// function was ever called — so a page that shipped with this function deleted would be uglier
// and equally safe. If that ever stops being true, the bug is in the server, and no amount of
// this file can cover for it.
//
// The password floor is the same shape. `passwordProblem` exists so the operator learns the
// requirement before submitting, not so the requirement is enforced; the server applies its own
// floor and a caller that skips this page is bound by it identically.
import type { RmcpAccount } from '../../types/rmcp';

/**
 * The minimum password length the SERVER enforces, restated so the form can say so up front.
 *
 * Restated, not owned. If the two ever disagree the server wins and the operator sees its
 * refusal — which is the correct failure, and the reason this is not written as though the
 * number lived here.
 */
export const MIN_PASSWORD_LEN = 12;

/**
 * Why this password cannot be submitted yet, or `null` when it can.
 *
 * Counts CHARACTERS, matching the server. **Never trims**: leading and trailing whitespace are
 * legitimate password characters, and a form that quietly trimmed would create an account whose
 * owner cannot log in with the exact string they were given. The server does not trim either.
 */
export function passwordProblem(password: string): string | null {
  if (password.length === 0) return 'A password is required.';
  if ([...password].length < MIN_PASSWORD_LEN) {
    return `At least ${MIN_PASSWORD_LEN} characters (this one has ${[...password].length}).`;
  }
  return null;
}

/** Why this account name cannot be submitted yet, or `null`. Trimmed, unlike the password —
 *  a name is an identifier the server itself trims, so accepting stray spaces here would only
 *  produce a confusing mismatch between what was typed and what exists. */
export function accountNameProblem(name: string): string | null {
  return name.trim().length === 0 ? 'An account name is required.' : null;
}

/** The active operators among these accounts. Absence is the empty set: an account list that
 *  arrived empty has no operators, which is exactly what an empty array says. */
export function activeOperators(accounts: RmcpAccount[]): RmcpAccount[] {
  return accounts.filter(a => a.operator && !a.disabled);
}

/**
 * Whether removing `account`'s operator authority — by demotion or by disabling it — would leave
 * the deployment with no active operator.
 *
 * **A reflection of the server's guard, not the guard.** See the module doc. It exists to render
 * a disabled control with an honest reason attached, because a control that is enabled and then
 * fails is a worse experience than one that explains itself; the refusal it predicts is issued by
 * the server regardless.
 *
 * Note it asks about the ACTIVE set, matching the server: an operator that is already disabled is
 * not one for this purpose, so a door with two operator rows and one of them disabled correctly
 * reports that the remaining one cannot be touched.
 */
export function wouldStrandTheDoor(accounts: RmcpAccount[], account: RmcpAccount): boolean {
  if (!account.operator || account.disabled) return false;
  return activeOperators(accounts).length <= 1;
}

/** Whether several operators exist, i.e. whether the server will require an explicit `actor`.
 *  Used to decide whether the page must ask; it never picks one. */
export function actorIsAmbiguous(accounts: RmcpAccount[]): boolean {
  return activeOperators(accounts).length > 1;
}

/**
 * The names the page may offer as `actor` suggestions.
 *
 * Offered for CLICKING, never auto-selected — the same rule the connector dialog landed on in
 * TERM #647, and for the same reason: a one-element list silently selected is a guess wearing a
 * menu's clothes, and here the value it would guess is whose name an administrative action is
 * recorded under.
 */
export function actorSuggestions(accounts: RmcpAccount[]): string[] {
  return activeOperators(accounts)
    .map(a => a.account)
    .sort((a, b) => a.localeCompare(b));
}
