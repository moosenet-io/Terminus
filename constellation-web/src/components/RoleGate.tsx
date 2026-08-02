// CONST-27 (§3.4): cosmetic-only client-side gate for mutating controls. Wraps a control
// (button, toggle, slider, palette command, …) and, for a viewer session, renders it disabled
// with the "operator role required" reason instead of removing it — the operator can still
// see what exists, just not use it.
//
// This is DELIBERATELY not the enforcement: the server's `enforce_viewer_role_gate`
// middleware (`src/constellation/auth.rs`) rejects a viewer's mutating request with
// `403 {"error":"forbidden","required_role":"operator"}` regardless of whether a control is
// wrapped in `RoleGate` at all — see that module's doc, and the acceptance criterion "proven
// by direct POST as viewer". A caller with dev tools open (or curl) bypasses this gate
// trivially; that's expected and fine, because it can never bypass the server-side one.
//
// MUSE-127 review fix (the twelfth instance of a recurring class — see DiscoverPanel.tsx's
// own local patch for an earlier one): a `title` attribute is HOVER-ONLY, so it is invisible
// on touch and unreliable for assistive tech — `aria-disabled` announces THAT a control is
// disabled but never WHY. Every consumer of this shared component inherited that gap. Fixed
// at the source (here) rather than per-caller: a visually-hidden span carries the reason as
// persistent, always-in-the-tree content, associated via `aria-describedby` — the same
// pattern DiscoverPanel/SensitivityBadge already use locally, generalised into the one place
// every `RoleGate` consumer shares. `title` is kept too (harmless bonus for a mouse hover),
// but the accessible/testable source of truth is now the described text, not the attribute.
import { useId, type ReactNode } from 'react';
import { useAuthRole } from '../hooks/AuthRoleContext';

/** The exact reason string — MUST match the 403 body's implied reason (`required_role:
 *  "operator"`) and `terminateOutcome.ts`'s `OPERATOR_ROLE_REQUIRED` (MACT-07/MUSE-127), so a
 *  viewer who bypasses this cosmetic gate sees the identical explanation reflected back by the
 *  real server-side one. */
export const OPERATOR_ROLE_REQUIRED_REASON = 'operator role required';

export interface RoleGateProps {
  children: ReactNode;
  /** Wrapper display mode — 'inline-flex' (default) for a control sitting in a flex row of
   *  buttons/toggles, 'block' for a standalone control (e.g. a full-width slider). */
  display?: 'inline-flex' | 'block';
}

/** Gates `children` to the operator role. A `null` role (unauthenticated — shouldn't normally
 *  render here at all, see `App.tsx`) is treated the same as `'operator'`: this component
 *  only ever narrows access for a CONFIRMED viewer session, never invents a stricter
 *  cosmetic state than the server itself would apply. */
export function RoleGate({ children, display = 'inline-flex' }: RoleGateProps) {
  const role = useAuthRole();
  // `useId` per instance — this component is mounted many times on one page (one per gated
  // control), so a hardcoded id would collide and `aria-describedby` would point every
  // instance at the same (or the wrong) element.
  const reasonId = useId();

  if (role !== 'viewer') {
    return <>{children}</>;
  }

  return (
    <span
      title={OPERATOR_ROLE_REQUIRED_REASON}
      aria-disabled="true"
      aria-describedby={reasonId}
      style={{
        display,
        opacity: 0.45,
        cursor: 'not-allowed',
        // Blocks all pointer interaction with the wrapped control(s) — the visual "disabled"
        // state a viewer sees, backed by the real 403 if this were somehow bypassed.
        pointerEvents: 'none',
      }}
    >
      {children}
      {/* Visually hidden, but present in the DOM/accessibility tree unconditionally — no
          hover, no focus, no gesture required. Standard clip-based sr-only pattern (matches
          SensitivityBadge.tsx's inline use of the same technique). */}
      <span
        id={reasonId}
        style={{
          position: 'absolute',
          width: 1,
          height: 1,
          overflow: 'hidden',
          clip: 'rect(0 0 0 0)',
          whiteSpace: 'nowrap',
        }}
      >
        {OPERATOR_ROLE_REQUIRED_REASON}
      </span>
    </span>
  );
}
