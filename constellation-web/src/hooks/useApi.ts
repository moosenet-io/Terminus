// CONST-04: Generic data-fetch hook, adapted from harmony-web's useApi.ts.
//
// Unlike the original, this does NOT call `fetch` itself and holds NO secret in
// localStorage/sessionStorage — every request goes through the aggregation client
// (src/lib/aggregationClient.ts), which is the only module allowed to talk to the backend.
import { useState, useCallback } from 'react';
import { getAggregationClient } from '../lib/aggregationClient';
import type { SystemId } from '../lib/aggregationClient';

export function useApi<T>() {
  const [data, setData] = useState<T | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const fetch_ = useCallback(async (system: SystemId, path: string, options?: RequestInit) => {
    setLoading(true);
    setError(null);
    try {
      const client = getAggregationClient();
      const json = await client.request<T>(system, path, options);
      setData(json);
      return json;
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Unknown error');
      return null;
    } finally {
      setLoading(false);
    }
  }, []);

  return { data, loading, error, fetch: fetch_ };
}
