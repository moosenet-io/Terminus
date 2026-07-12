// TRIAGE-09, ported for CONST-04: Hook for fetching escalation analytics from harmony's
// GET /analytics/escalation, routed through the aggregation client (system 'harmony').
import { useState, useEffect, useCallback } from 'react';
import { useAuth } from './useAuth';
import { getAggregationClient } from '../lib/aggregationClient';

export interface EscalationAnalytics {
  total_tasks: number;
  pass_rate_by_tier: Record<string, number>;
  failure_mode_counts: Record<string, number>;
  complexity_distribution: Record<string, number>;
  enrichment_quality: Record<string, number>;
  problem_specs: [string, number][];
}

interface UseEscalationDataResult {
  data: EscalationAnalytics | null;
  loading: boolean;
  error: string | null;
  refetch: () => void;
}

export function useEscalationData(): UseEscalationDataResult {
  const { authenticated } = useAuth();
  const [data, setData] = useState<EscalationAnalytics | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [tick, setTick] = useState(0);

  const refetch = useCallback(() => setTick(t => t + 1), []);

  useEffect(() => {
    if (!authenticated) return;
    let cancelled = false;

    async function doFetch() {
      try {
        const json = await getAggregationClient().request<EscalationAnalytics>('harmony', '/analytics/escalation');
        if (!cancelled) { setData(json); setError(null); }
      } catch (e) {
        if (!cancelled) setError(String(e));
      } finally {
        if (!cancelled) setLoading(false);
      }
    }

    doFetch();
    return () => { cancelled = true; };
  }, [authenticated, tick]);

  return { data, loading, error, refetch };
}
