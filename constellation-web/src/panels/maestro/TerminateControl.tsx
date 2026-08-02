// MACT-07 (MUSE-127): the stop-this-stream control on each now-playing card — the panel's first
// mutation, and the one that interrupts a real person mid-film. Treated with that seriousness:
// confirmed (naming who/what/where), rendered honestly (never an optimistic success), and
// gated for real server-side, not just cosmetically.
//
// ── SECURITY MODEL (read before touching this file) ─────────────────────────────────────────
//   - `RoleGate` below is COSMETIC ONLY. Its own doc comment says so explicitly: a viewer sees
//     the button disabled with an "operator role required" tooltip, but curl/dev-tools bypass
//     it trivially. It exists purely so an operator UI doesn't dangle a control a viewer's
//     click would only ever 403 on.
//   - The REAL enforcement is server-side: `enforce_viewer_role_gate`
//     (`Terminus/src/constellation/auth.rs`), layered on `protected_router` — the router the
//     `/api/muse/*path` proxy arm lives on (`Terminus/src/constellation/mod.rs`, see
//     `constellation_router`'s comment on layering order). A viewer's POST to the terminate
//     route is rejected `403 {"error":"forbidden","required_role":"operator"}` BEFORE it is
//     proxied to Muse at all. Proven directly (not merely asserted) by
//     `viewer_role_gate_denies_a_mutating_request` in `auth.rs`, which drives a real POST
//     through a router wearing exactly this middleware and asserts the 403 — see this repo's
//     Rust test suite; a live capture against a running deployment is recorded in the PR per
//     this item's acceptance criteria.
//   - This module's own `terminateOutcome.ts` renders a `kind: 'forbidden'` result with the
//     SAME wording as `RoleGate`'s tooltip ("operator role required") — a viewer who bypasses
//     the cosmetic gate sees the identical reason reflected back from the real one.
//
// ── NO BULK "TERMINATE ALL" ──────────────────────────────────────────────────────────────────
// Deliberately absent, and not an oversight — a one-click mass-stop on a household media server
// is a footgun with no matching use case (nobody wants "stop everyone's stream" as a workflow).
// This control is per-session and individually confirmed only, exactly per this item's explicit
// scope. If a future change is tempted to add a "stop all" button here, don't — file it as its
// own reviewed item with its own justification instead of folding it into this one quietly.
//
// ── RENDER THE REAL OUTCOME ───────────────────────────────────────────────────────────────────
// `stopped: false` is not a success (see `terminateOutcome.ts`'s module doc for the full
// rationale — MACT-02 spent three review cycles making `stopped` honest at the API layer). This
// control never removes the card optimistically; after ANY resolved outcome (success, honest
// failure, or refusal) it calls `onTerminated` so the LIVE pane refetches from the server and
// the panel shows the truth rather than what was hoped for.
import { useState } from 'react';
import { Button } from '../../components/Button';
import { RoleGate } from '../../components/RoleGate';
import { ConfirmDialog } from '../../components/ConfirmDialog';
import { useMuseTerminateSession } from '../../hooks/useMuse';
import type { LiveSession, MuseTerminateResult } from '../../lib/aggregationClient';
import { accountLabel, itemTitle, progressInfo } from './nowPlaying';
import { describeTerminateOutcome, confirmDescription, shouldIssueTerminateCall } from './terminateOutcome';

/** Pure banner rendering an already-resolved [`MuseTerminateResult`] — split out from the
 *  stateful control below so the rendering rules in `terminateOutcome.ts` (honest failure,
 *  distinguishable 403/409) are directly exercisable with `renderToStaticMarkup` and plain
 *  props, no hooks/fetch mocking involved (same convention as `ActivityPanel.tsx`'s
 *  `LivePane`/`HistoryPane`). Renders nothing when there is no result yet. */
export function TerminateOutcomeBanner({ result }: { result: MuseTerminateResult | null }) {
  if (result == null) return null;
  const outcome = describeTerminateOutcome(result);
  const color =
    outcome.tone === 'success' ? 'var(--status-success)'
    : outcome.tone === 'error' ? 'var(--status-error)'
    : outcome.tone === 'warn' ? 'var(--status-warning)'
    : 'var(--text-muted)';
  return (
    <div
      role="status"
      style={{ fontSize: 'var(--fs-xs)', color, marginTop: 'var(--space-1)' }}
    >
      {outcome.message}
    </div>
  );
}

export interface TerminateControlProps {
  session: LiveSession;
  /** Called after ANY resolved terminate attempt (success, honest failure, or refusal) so the
   *  caller can refetch the LIVE pane from the server — this control never removes the card
   *  itself. Optional only so existing tests that render `LiveSessionCard` without wiring a
   *  live LIVE-pane refetch keep working unchanged. */
  onTerminated?: () => void;
}

/** The stop-this-stream control: `RoleGate`-wrapped trigger → `ConfirmDialog` (names account,
 *  title, position, optional reason) → the typed `aggregationClient` `terminate()` mutation
 *  (never a direct `fetch` — CONST-04's rule) → an honest outcome banner. */
export function TerminateControl({ session, onTerminated }: TerminateControlProps) {
  const [open, setOpen] = useState(false);
  const [reason, setReason] = useState('');
  const [result, setResult] = useState<MuseTerminateResult | null>(null);
  const { terminate, inFlight } = useMuseTerminateSession();

  // Edge case (this item's spec): no `session_key` means there is nothing safe to target — the
  // backend resolves a terminate target BY session_key (see `MuseTerminateResult`'s doc on
  // `AmbiguousSession`/`NotFound`), so a null key here is a "this session can't be stopped from
  // here" state, not a confirm-then-fail round trip.
  const canTarget = session.session_key != null;

  const positionLabel = progressInfo(session.view_offset_ms, session.duration_ms, session.progress_pct).combinedLabel;
  const description = confirmDescription(accountLabel(session.account), itemTitle(session.item), positionLabel);

  function openConfirm() {
    setResult(null);
    setReason('');
    setOpen(true);
  }

  function cancel() {
    // `shouldIssueTerminateCall('cancel', …)` is always `false` by construction (see its doc in
    // terminateOutcome.ts) — cancel never reaches `terminate()`, full stop. Nothing to gate here.
    setOpen(false);
  }

  async function confirm() {
    if (!shouldIssueTerminateCall('confirm', { canTarget, inFlight })) return;
    const key = session.session_key as string;
    const outcome = await terminate(key, reason.trim() || undefined);
    setResult(outcome);
    setOpen(false);
    // Refresh from the server regardless of outcome kind — including a refusal or an honest
    // "did not stop" — so the panel never shows a state it merely hoped was true.
    onTerminated?.();
  }

  return (
    <div>
      <RoleGate>
        <Button
          variant="danger"
          size="sm"
          onClick={openConfirm}
          disabled={!canTarget || inFlight}
          title={canTarget ? undefined : 'session key unavailable — cannot target a stop'}
        >
          {inFlight ? 'Stopping…' : 'Stop'}
        </Button>
      </RoleGate>

      <TerminateOutcomeBanner result={result} />

      <ConfirmDialog
        open={open}
        title="Stop this stream?"
        description={description}
        confirmLabel="Stop stream"
        destructive
        busy={inFlight}
        onConfirm={confirm}
        onCancel={cancel}
      >
        <label style={{ display: 'flex', flexDirection: 'column', gap: 'var(--space-1)', fontSize: 'var(--fs-xs)', color: 'var(--text-muted)' }}>
          Reason (optional, shown to the viewer when the player supports it)
          <textarea
            value={reason}
            onChange={e => setReason(e.target.value)}
            rows={2}
            disabled={inFlight}
            style={{
              resize: 'vertical',
              background: 'var(--surface-recessed, var(--space-700))',
              border: 'var(--border-width) solid var(--border)',
              borderRadius: 'var(--radius-sm)',
              color: 'var(--text-primary)',
              padding: 'var(--space-2)',
              fontFamily: 'inherit',
              fontSize: 'var(--fs-sm)',
            }}
          />
        </label>
      </ConfirmDialog>
    </div>
  );
}
