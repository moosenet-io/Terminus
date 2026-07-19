// CONST-20 SEAM (temporary, clearly marked per the spec item's own instruction): CONST-27
// (auth.rs `role` JWT claim + `GET /api/auth/me` gaining a `role` field, §3.4) has NOT merged
// on main as of this build. Muse's channel compose/maintenance actions are spec'd (§5.4) as
// operator-RoleGated -- rather than block CONST-20 on CONST-27 landing first, this hook reads
// `role` off `AuthMeResponse` when present (zero-change cutover the day CONST-27 lands it) and
// falls back to 'operator' whenever a session is authenticated but the field is absent, i.e.
// "enabled for operator by default" exactly as the CONST-20 task brief calls for.
//
// IMPORTANT: this is a UI courtesy only, never enforcement -- per spec §3.4 "UI RoleGate is a
// courtesy, never the enforcement," the real gate is server-side (403 on mutating routes for a
// viewer session) and stays true regardless of what this hook returns. Delete the fallback
// default (keep the shape) once CONST-27 merges and `role` is always present on the response.
import { useAuth } from './useAuth';

export type AuthRole = 'viewer' | 'operator';

export function useAuthRole(): AuthRole {
  const { authenticated, role } = useAuth();
  if (!authenticated) return 'viewer';
  return role ?? 'operator';
}
