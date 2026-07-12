// SGUI-08: Derive routing state from executor data
import { useMemo } from 'react';
import type { Worker } from '../types/api';

export interface RouteConnection {
  from: string;
  to: string;
  active: boolean;
  waiting: boolean;
  exhausted: boolean;
}

export function useRoutingState(workers: Worker[]): RouteConnection[] {
  return useMemo(() => {
    const providers = ['local', 'claude', 'codex', 'gemini'];
    return providers.map(provider => {
      const worker = workers.find(w =>
        w.provider.toLowerCase().includes(provider.toLowerCase())
      );
      return {
        from: 'conductor',
        to: provider,
        active: worker?.status === 'working',
        waiting: worker?.status === 'waiting',
        exhausted: provider === 'gemini' && worker?.status === 'idle',
      };
    });
  }, [workers]);
}
