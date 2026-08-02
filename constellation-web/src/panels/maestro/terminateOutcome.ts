// MACT-07 (MUSE-127): pure rendering rules for `MuseTerminateResult` — kept DOM-free so the
// honesty rules below are directly unit-testable, same convention as `nowPlaying.ts`.
//
// Three rules this file exists to enforce (each is a named failure mode in this item's brief):
//   1. `stopped: false` is NOT a success. MACT-02 spent three review cycles making `stopped`
//      honest at the API layer (a player that ignored a best-effort stop reports `false`, never
//      an optimistic `true`) — this file must not throw that away at render time by treating
//      any 2xx `terminate()` response as "it worked". `reason_delivered` never factors into
//      success either: a viewer not seeing an explanation is a different fact from the player
//      still running (see `MuseTerminateResult`'s own doc comment in aggregationClient.ts).
//   2. A `403` (`kind: 'forbidden'`) renders EXACTLY "operator role required" — the same
//      wording `RoleGate`'s tooltip uses (`components/RoleGate.tsx`) — and must render
//      distinguishably from a genuine transport failure (`kind: 'error'`), never the same copy.
//   3. A `409` (`kind: 'conflict'`) renders its own distinct message — never the generic
//      transport-error copy — so an operator can tell "ambiguous target, refused" from
//      "actually broken".
import type { MuseTerminateResult } from '../../lib/aggregationClient';

export type TerminateOutcomeTone = 'success' | 'warn' | 'error' | 'neutral';

export interface TerminateOutcome {
  /** Never `true` for `stopped: false` — see this module's rule 1. */
  isSuccess: boolean;
  message: string;
  tone: TerminateOutcomeTone;
}

/** The exact literal `RoleGate`'s tooltip uses (`components/RoleGate.tsx`'s `TOOLTIP` const) —
 *  a 403 here and the cosmetic disabled-button tooltip must read as the SAME reason, not two
 *  different explanations of the same fact. Kept as a named export so a test can assert this
 *  file and `RoleGate` never drift apart, rather than two independently-typed string literals. */
export const OPERATOR_ROLE_REQUIRED = 'operator role required';

/** Rejected approach (do not reinstate): collapsing `forbidden`/`conflict`/`unavailable` into
 *  the same `'error'` bucket as a real transport failure was the original draft of this
 *  function. It reads simpler, but it is exactly the defect this item's acceptance criteria
 *  calls out by name — a 403 and a dropped network connection are different facts an operator
 *  needs to act on differently (fix a role vs. retry vs. do nothing), and MACT-03 already pays
 *  for the typed discriminated union specifically so callers don't have to re-merge it here. */
export function describeTerminateOutcome(result: MuseTerminateResult): TerminateOutcome {
  switch (result.kind) {
    case 'ok':
      return result.stopped
        ? { isSuccess: true, tone: 'success', message: `Stopped (${result.backend}).` }
        : {
            isSuccess: false,
            tone: 'warn',
            message: `The player did not stop (${result.backend} reported no change) — it may still be playing.`,
          };
    case 'forbidden':
      return { isSuccess: false, tone: 'error', message: OPERATOR_ROLE_REQUIRED };
    case 'not_found':
      // Edge case (this item's spec): session ends between render and confirm — say so, not
      // "failed". The panel refetches regardless (see TerminateControl), so the card for this
      // session simply disappears on refresh; this message explains why before that happens.
      return { isSuccess: false, tone: 'neutral', message: 'This session already ended.' };
    case 'conflict':
      return {
        isSuccess: false,
        tone: 'warn',
        message: `More than one session matched — refusing to guess which to stop (${result.detail}).`,
      };
    case 'unavailable':
      return { isSuccess: false, tone: 'neutral', message: 'No playback controller configured for this session.' };
    case 'error':
    default:
      return { isSuccess: false, tone: 'error', message: `Could not reach the stream controller: ${result.detail}` };
  }
}

/** Confirm-dialog copy: names the Muse account, the title, and the position the stream will be
 *  stopped at — per this item's requirement that the confirmation names what it is about to
 *  interrupt, not a bare "are you sure?". */
export function confirmDescription(accountText: string, titleText: string, positionLabel: string): string {
  return `${accountText} is watching "${titleText}" at ${positionLabel}. This will stop the stream now.`;
}

/** Pure gate deciding whether a confirm-dialog action should actually issue the terminate
 *  mutation. Split out of `TerminateControl.tsx` so "confirm issues a call, cancel never does"
 *  (this item's required test) is a directly provable fact about plain logic rather than
 *  something only observable by rendering and clicking a live DOM — this repo has no
 *  jsdom/testing-library configured (see every other `*.test.tsx` in this directory, all of
 *  which use `renderToStaticMarkup` for exactly this reason; `TerminateControl.test.tsx`'s
 *  module doc says so explicitly). `cancel` NEVER issues a call, full stop, regardless of
 *  `canTarget`/`inFlight` — those only gate `confirm`. */
export function shouldIssueTerminateCall(
  action: 'confirm' | 'cancel',
  opts: { canTarget: boolean; inFlight: boolean },
): boolean {
  if (action === 'cancel') return false;
  return opts.canTarget && !opts.inFlight;
}
