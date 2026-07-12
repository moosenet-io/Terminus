// Ported for CONST-04: live provider list, routed through the aggregation client.
// Grouped under Chord (inference/providers/routing/models/serving/analytics/playground)
// per the CONST-04 endpoint→system mapping.
import { useState, useEffect } from 'react';
import type { Provider } from '../types/provider';
import { useAuth } from './useAuth';
import { getAggregationClient } from '../lib/aggregationClient';

interface UseProvidersResult {
  providers: Provider[] | null;
  loading: boolean;
  error: string | null;
  refetch: () => void;
}

export function useProviders(): UseProvidersResult {
  const { authenticated } = useAuth();
  const [providers, setProviders] = useState<Provider[] | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [tick, setTick] = useState(0);

  useEffect(() => {
    if (!authenticated) return;
    let cancelled = false;

    async function load() {
      try {
        const data = await getAggregationClient().request<Provider[]>('chord', '/providers');
        if (!cancelled) {
          setProviders(Array.isArray(data) ? data : []);
          setError(null);
        }
      } catch (e) {
        if (!cancelled) setError(String(e));
      } finally {
        if (!cancelled) setLoading(false);
      }
    }

    load();
    const interval = setInterval(load, 30_000);
    return () => {
      cancelled = true;
      clearInterval(interval);
    };
  }, [authenticated, tick]);

  return { providers, loading, error, refetch: () => setTick(t => t + 1) };
}
