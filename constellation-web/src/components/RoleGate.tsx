// CONST-27 (§3.4): cosmetic-only client-side gate for mutating controls. Wraps a control
// (button, toggle, slider, palette command, …) and, for a viewer session, renders it disabled
// with an "operator role required" tooltip instead of removing it — the operator can still
// see what exists, just not use it.
//
// This is DELIBERATELY not the enforcement: the server's `enforce_viewer_role_gate`
// middleware (`src/constellation/auth.rs`) rejects a viewer's mutating request with
// `403 {"error":"forbidden","required_role":"operator"}` regardless of whether a control is
// wrapped in `RoleGate` at all — see that module's doc, and the acceptance criterion "proven
// by direct POST as viewer". A caller with dev tools open (or curl) bypasses this gate
// trivially; that's expected and fine, because it can never bypass the server-side one.
import type { ReactNode } from 'react';
import { useAuthRole } from '../hooks/AuthRoleContext';

const TOOLTIP = 'operator role required';

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

  if (role !== 'viewer') {
    return <>{children}</>;
  }

  return (
    <span
      title={TOOLTIP}
      aria-disabled="true"
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
    </span>
  );
}
