// CONST-27 (§3.4): a tiny React context carrying the current session's `AuthRole`, so
// `RoleGate` (`../components/RoleGate.tsx`) — used deep inside panels that the router mounts
// with no props (`panels/registerPanels.ts` renders `<Component />`) — can read the role
// without threading it through every panel's props. `useAuth()` itself stays the single
// source of truth (one call site, in `App.tsx`); this context just republishes its `role`
// field for anything below `<Shell>` that doesn't already have it.
//
// Cosmetic only: this NEVER gates a request. The server's `enforce_viewer_role_gate`
// middleware is the actual enforcement (403 on every mutating method for a viewer session)
// regardless of what this context — or anything reading it — renders.
import { createContext, useContext } from 'react';
import type { ReactNode } from 'react';
import type { AuthRole } from '../lib/aggregationClient';

const AuthRoleContext = createContext<AuthRole>(null);

export function AuthRoleProvider({ role, children }: { role: AuthRole; children: ReactNode }) {
  return <AuthRoleContext.Provider value={role}>{children}</AuthRoleContext.Provider>;
}

/** The current session's role (`'operator'` | `'viewer'` | `null` pre-auth). Consumed by
 *  `RoleGate`; safe to call from anywhere under `<AuthRoleProvider>` (i.e. anywhere under
 *  `<Shell>` — the app never renders panels before authenticating, see `App.tsx`). */
export function useAuthRole(): AuthRole {
  return useContext(AuthRoleContext);
}
