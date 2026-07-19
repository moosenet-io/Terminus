// CONST-04: Session-based auth hook, adapted from harmony-web's useAuth.ts.
//
// Same session-cookie model (checks auth on mount, exposes login/logout), routed through the
// aggregation client instead of a direct fetch. Deliberately drops harmony-web's
// `localStorage['harmony_soma_api_key']` fallback and the `prompt()` API-key flow — no secret
// is ever held in browser storage here, only in-memory React state.
//
// CONST-27: also tracks `role` (§3.4) — surfaced purely so the shell/panels can render a
// cosmetic `RoleGate` (`../components/RoleGate.tsx`) on mutating controls for a viewer
// session. This is NEVER the enforcement: the server's `enforce_viewer_role_gate` rejects a
// viewer's mutating request with 403 regardless of what this hook (or the UI built on it)
// shows.
import { useState, useEffect, useCallback } from 'react';
import { getAggregationClient } from '../lib/aggregationClient';
import type { AuthRole } from '../lib/aggregationClient';

export interface AuthState {
  authenticated: boolean;
  username: string | null;
  role: AuthRole;
  loading: boolean;
}

export function useAuth() {
  const [state, setState] = useState<AuthState>({
    authenticated: false,
    username: null,
    role: null,
    loading: true,
  });

  const checkAuth = useCallback(async () => {
    try {
      const client = getAggregationClient();
      const d = await client.auth.me();
      setState({ authenticated: d.authenticated, username: d.username, role: d.role, loading: false });
    } catch {
      setState({ authenticated: false, username: null, role: null, loading: false });
    }
  }, []);

  useEffect(() => {
    checkAuth();
  }, [checkAuth]);

  const login = useCallback(async (username: string, password: string) => {
    const client = getAggregationClient();
    const d = await client.auth.login(username, password);
    setState({ authenticated: d.authenticated, username: d.username, role: d.role, loading: false });
    return d;
  }, []);

  const logout = useCallback(async () => {
    const client = getAggregationClient();
    await client.auth.logout().catch(() => {});
    setState({ authenticated: false, username: null, role: null, loading: false });
  }, []);

  return { ...state, login, logout, checkAuth };
}
