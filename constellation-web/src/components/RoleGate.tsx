// CONST-20 SEAM (temporary, clearly marked per the spec item's own instruction): CONST-27 is
// the item that builds the real, shell-level `RoleGate` (§2.3 lists it as a restyle target of
// a component that doesn't exist in this repo yet). Muse's channel compose/maintenance
// actions need role gating NOW (spec §5.4), so this is a minimal stand-in with the same
// intended API (`minRole` + children + optional `fallback`) -- CONST-27 can drop this file in
// favor of its own without touching any call site.
//
// Enforcement is ALWAYS server-side (spec §3.4: "UI RoleGate is a courtesy, never the
// enforcement") -- this component only hides/shows UI affordances; a viewer session hitting a
// mutating muse route still gets a structural 403 from the backend regardless of what this
// renders.
import type { ReactNode } from 'react';
import { useAuthRole, type AuthRole } from '../hooks/useAuthRole';

const ROLE_RANK: Record<AuthRole, number> = { viewer: 0, operator: 1 };

interface RoleGateProps {
  minRole: AuthRole;
  children: ReactNode;
  /** Rendered instead of children when the current role doesn't meet `minRole`. Defaults to
   *  rendering nothing (the common case: hide the action entirely for a viewer). */
  fallback?: ReactNode;
}

export function RoleGate({ minRole, children, fallback = null }: RoleGateProps) {
  const role = useAuthRole();
  if (ROLE_RANK[role] < ROLE_RANK[minRole]) return <>{fallback}</>;
  return <>{children}</>;
}
