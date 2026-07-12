// GROW-06, ported for CONST-04: useTreeData — polls harmony's /tree/{project} every 5s and
// subscribes to WS `tree_update` events for sub-second reaction to stage transitions. Falls
// back to polling when WebSocket is disconnected (existing behaviour). Routed through the
// aggregation client — no direct fetch/WebSocket/window.location here.
import { useState, useEffect, useCallback, useRef } from 'react';
import type { TreeResponse } from '../types/tree';
import { useAuth } from './useAuth';
import { getAggregationClient } from '../lib/aggregationClient';

interface UseTreeDataResult {
  data: TreeResponse | null;
  loading: boolean;
  error: string | null;
  refetch: () => void;
}

// Debounce rapid tree_update events (e.g. multiple stage transitions in 500ms).
function useDebounce(fn: () => void, ms: number) {
  const timerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  return useCallback(() => {
    if (timerRef.current) clearTimeout(timerRef.current);
    timerRef.current = setTimeout(fn, ms);
  }, [fn, ms]);
}

export function useTreeData(project: string): UseTreeDataResult {
  const { authenticated } = useAuth();
  const [data, setData] = useState<TreeResponse | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [tick, setTick] = useState(0);

  const refetch = useCallback(() => setTick(t => t + 1), []);
  const debouncedRefetch = useDebounce(refetch, 500);

  // HTTP polling (5s interval + manual tick).
  useEffect(() => {
    if (!authenticated || !project) return;
    let cancelled = false;

    async function doFetch() {
      try {
        const json = await getAggregationClient().request<TreeResponse>(
          'harmony',
          `/tree/${encodeURIComponent(project)}`,
        );
        if (!cancelled) {
          setData(json);
          setError(null);
        }
      } catch (e) {
        if (!cancelled) setError(String(e));
      } finally {
        if (!cancelled) setLoading(false);
      }
    }

    doFetch();
    const interval = setInterval(doFetch, 5_000);
    return () => {
      cancelled = true;
      clearInterval(interval);
    };
  }, [authenticated, project, tick]);

  // WS subscription — triggers debounced refetch on tree_update events.
  useEffect(() => {
    if (!authenticated) return;
    const conn = getAggregationClient().ws.connect({
      onEvent: (msg) => {
        if ((msg as { type?: string })?.type === 'tree_update') debouncedRefetch();
      },
    });
    return () => conn.close();
  }, [authenticated, debouncedRefetch]);

  return { data, loading, error, refetch };
}
