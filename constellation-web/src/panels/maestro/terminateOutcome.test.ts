// MACT-07 (MUSE-127): the required, mutation-proven tests for this item's rendering rules —
// `stopped:false` is never a success, a 403 renders distinguishably from a transport error, and
// a 409 never collapses into the generic error copy. Each test below was verified by hand to
// FAIL against a version of `describeTerminateOutcome` that collapsed the distinction it checks
// (see the comment above each test for exactly what mutation was reverted to confirm this).
import { describe, it, expect } from 'vitest';
import {
  describeTerminateOutcome,
  confirmDescription,
  shouldIssueTerminateCall,
  OPERATOR_ROLE_REQUIRED,
} from './terminateOutcome';
import type { MuseTerminateResult } from '../../lib/aggregationClient';

// Review finding (MUSE-127 round 2): this used to also test a `'cancel'` action on
// `shouldIssueTerminateCall` — that branch was DEAD (see the function's doc in
// terminateOutcome.ts for the full story: `cancel()` never called it, so the mutation test
// that lived here "proved" a helper outside the path it claimed to protect). The function now
// only guards the one real call site (`confirm()`); "cancel never calls terminate" is proven
// at the component level instead — see `TerminateControl.interaction.test.tsx`.
describe('shouldIssueTerminateCall — the ONE real call site (confirm)', () => {
  // MUTATION-PROOF: reverting `return opts.canTarget && !opts.inFlight;` to `return true;` makes
  // the next two tests fail. Verified by hand.
  it('issues a call when targetable and not already in flight', () => {
    expect(shouldIssueTerminateCall({ canTarget: true, inFlight: false })).toBe(true);
  });

  it('is blocked without a session_key to target (canTarget:false)', () => {
    expect(shouldIssueTerminateCall({ canTarget: false, inFlight: false })).toBe(false);
  });

  it('is blocked while already in flight (double-submit guard)', () => {
    expect(shouldIssueTerminateCall({ canTarget: true, inFlight: true })).toBe(false);
  });
});

describe('describeTerminateOutcome — stopped:false is never a success', () => {
  it('stopped:true renders success', () => {
    const result: MuseTerminateResult = { kind: 'ok', stopped: true, backend: 'plex', reason_delivered: true };
    const outcome = describeTerminateOutcome(result);
    expect(outcome.isSuccess).toBe(true);
    expect(outcome.tone).toBe('success');
    expect(outcome.message.toLowerCase()).toContain('stopped');
  });

  // MUTATION-PROOF: reverting this file to `isSuccess: result.kind === 'ok'` (ignoring
  // `stopped`) makes this test fail — that IS the regression MACT-02's three review cycles
  // exist to prevent. Verified by hand.
  it('stopped:false is NOT rendered as success, and says the player did not stop', () => {
    const result: MuseTerminateResult = { kind: 'ok', stopped: false, backend: 'plex', reason_delivered: true };
    const outcome = describeTerminateOutcome(result);
    expect(outcome.isSuccess).toBe(false);
    expect(outcome.tone).not.toBe('success');
    expect(outcome.message.toLowerCase()).toContain('did not stop');
  });

  it('reason_delivered never factors into success — false reason_delivered with stopped:true is still success', () => {
    const result: MuseTerminateResult = { kind: 'ok', stopped: true, backend: 'plex', reason_delivered: false };
    expect(describeTerminateOutcome(result).isSuccess).toBe(true);
  });

  it('reason_delivered:true cannot rescue a stopped:false outcome into success', () => {
    const result: MuseTerminateResult = { kind: 'ok', stopped: false, backend: 'plex', reason_delivered: true };
    expect(describeTerminateOutcome(result).isSuccess).toBe(false);
  });
});

describe('describeTerminateOutcome — a 403 renders distinguishably from a transport error', () => {
  // MUTATION-PROOF: reverting the `forbidden` arm to `return describeTerminateOutcome({...result,
  // kind: 'error'})`-style collapse (i.e. routing forbidden through the generic error message)
  // makes this test fail, since the message would then start with "Could not reach the stream
  // controller" instead of the exact operator-role string. Verified by hand.
  it('forbidden renders exactly "operator role required"', () => {
    const result: MuseTerminateResult = { kind: 'forbidden', detail: 'forbidden' };
    const outcome = describeTerminateOutcome(result);
    expect(outcome.message).toBe(OPERATOR_ROLE_REQUIRED);
    expect(outcome.isSuccess).toBe(false);
  });

  it('a genuine transport error renders DIFFERENT copy than forbidden', () => {
    const forbidden = describeTerminateOutcome({ kind: 'forbidden', detail: 'forbidden' });
    const transportError = describeTerminateOutcome({ kind: 'error', detail: 'network down' });
    expect(transportError.message).not.toBe(forbidden.message);
    expect(transportError.message).not.toContain(OPERATOR_ROLE_REQUIRED);
    expect(forbidden.message).not.toContain('network down');
  });
});

describe('describeTerminateOutcome — a 409 conflict does not collapse into a generic error', () => {
  // MUTATION-PROOF: reverting the `conflict` arm to fall through to the `default`/`error` case
  // (deleting the dedicated `case 'conflict':` branch) makes this test fail, since the message
  // would then read "Could not reach the stream controller: ..." instead of naming the
  // ambiguous-match refusal. Verified by hand.
  it('conflict renders its own distinct message, not the generic transport-error copy', () => {
    const conflict = describeTerminateOutcome({ kind: 'conflict', detail: 'ambiguous session' });
    const transportError = describeTerminateOutcome({ kind: 'error', detail: 'ambiguous session' });
    expect(conflict.message).not.toBe(transportError.message);
    expect(conflict.message.toLowerCase()).toContain('more than one session');
    expect(conflict.message).not.toContain('Could not reach the stream controller');
  });

  it('conflict is not rendered as success and carries the backend detail', () => {
    const outcome = describeTerminateOutcome({ kind: 'conflict', detail: 'ambiguous target' });
    expect(outcome.isSuccess).toBe(false);
    expect(outcome.message).toContain('ambiguous target');
  });
});

describe('describeTerminateOutcome — the remaining refusal kinds each get their own honest copy', () => {
  it('not_found says the session already ended, not "failed"', () => {
    const outcome = describeTerminateOutcome({ kind: 'not_found', detail: 'no live session' });
    expect(outcome.isSuccess).toBe(false);
    expect(outcome.message.toLowerCase()).toContain('already ended');
    expect(outcome.message.toLowerCase()).not.toContain('failed');
  });

  it('unavailable names the missing playback controller, not a generic error', () => {
    const outcome = describeTerminateOutcome({ kind: 'unavailable', detail: 'no controller' });
    expect(outcome.isSuccess).toBe(false);
    expect(outcome.message.toLowerCase()).toContain('no playback controller');
  });
});

describe('confirmDescription — names the account, the title, and the position', () => {
  it('includes all three plus the stop framing', () => {
    const text = confirmDescription('<operator>', 'The Martian', '21:24 / 2:22:00');
    expect(text).toContain('<operator>');
    expect(text).toContain('The Martian');
    expect(text).toContain('21:24 / 2:22:00');
    expect(text.toLowerCase()).toContain('stop');
  });
});
