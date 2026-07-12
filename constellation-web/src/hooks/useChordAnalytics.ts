// WIRE-06, ported for CONST-04: Chord analytics hook — savings and cost data.
// Routed through the aggregation client (system 'chord') instead of a direct `/chord/api` fetch.
import { useState, useEffect } from 'react';
import { getAggregationClient } from '../lib/aggregationClient';

export interface SavingsData {
  period: string;
  total_tokens_local: number;
  total_tokens_cloud: number;
  actual_cost_usd: number;
  imputed_cloud_cost_usd: number;
  savings_usd: number;
}

export interface CostData {
  date: string;
  actual_cost: number;
  imputed_cost: number;
  tokens_local: number;
  tokens_cloud: number;
}

export function useChordAnalytics(period = '30d') {
  const [savings, setSavings] = useState<SavingsData | null>(null);
  const [costData, setCostData] = useState<CostData[]>([]);
  const [loading, setLoading] = useState(true);

  useEffect(() => {
    let cancelled = false;

    async function loadData() {
      try {
        const client = getAggregationClient();
        const [s, c] = await Promise.all([
          client.request<SavingsData | null>('chord', `/analytics/savings?period=${period}`).catch(() => null),
          client.request<CostData[]>('chord', `/analytics/cost?period=${period}`).catch(() => []),
        ]);
        if (!cancelled) {
          setSavings(s);
          setCostData(Array.isArray(c) ? c : []);
          setLoading(false);
        }
      } catch {
        if (!cancelled) setLoading(false);
      }
    }

    void loadData();
    return () => { cancelled = true; };
  }, [period]);

  return { savings, costData, loading };
}
