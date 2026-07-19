// CONST-04: Session-based auth hook, adapted from harmony-web's useAuth.ts.
//
// Same session-cookie model (checks auth on mount, exposes login/logout), routed through the
// aggregation client instead of a direct fetch. Deliberately drops harmony-web's
// `localStorage['harmony_soma_api_key']` fallback and the `prompt()` API-key flow — no secret
// is ever held in browser storage here, only in-memory React state.
import { useState, useEffect, useCallback } from 'react';
import { getAggregationClient } from '../lib/aggregationClient';
import type { AuthRole } from '../lib/aggregationClient';

export interface AuthState {
  authenticated: boolean;
  username: string | null;
  loading: boolean;
  /** CONST-20: threaded through from `AuthMeResponse.role` ahead of CONST-27 landing it
   *  server-side for real — see `hooks/useAuthRole.ts` for the fallback policy when absent. */
  role?: AuthRole;
}

export function useAuth() {
  const [state, setState] = useState<AuthState>({
    authenticated: false,
    username: null,
    loading: true,
  });

  const checkAuth = useCallback(async () => {
    try {
      const client = getAggregationClient();
      const d = await client.auth.me();
      setState({ authenticated: d.authenticated, username: d.username, role: d.role, loading: false });
    } catch {
      setState({ authenticated: false, username: null, loading: false });
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
    setState({ authenticated: false, username: null, loading: false });
  }, []);

  return { ...state, login, logout, checkAuth };
}
