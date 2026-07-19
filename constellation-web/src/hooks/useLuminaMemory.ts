// LGUI-08 (§3.3 "lumina.memory — Engram browser"): data hook for the Memory browser panel.
// Deliberately its OWN hook (not an extension of LGUI-06's unmerged `useLumina.ts`) per this
// item's brief — sibling branches touch overlapping surface area, reconciliation happens at
// merge time. Two independent poll/fetch sections (stats + search results) so a slow/failing
// stats read never blanks the results table and vice versa, same "per-section degradation"
// convention as `useLumina.ts`.
import { useCallback, useEffect, useState } from 'react';
import { getAggregationClient } from '../lib/aggregationClient';
import { buildMemorySearchQuery } from '../panels/lumina/memorySearch';
import type {
  LuminaMemoryStats,
  Memory,
  MemorySearchParams,
  MemorySearchResponse,
} from '../types/luminaMemory';

const STATS_POLL_MS = 30_000;
const DEFAULT_LIMIT = 50;

interface SectionState<T> {
  data: T | null;
  loading: boolean;
  isRefetching: boolean;
  error: string | null;
}

function initialSection<T>(): SectionState<T> {
  return { data: null, loading: true, isRefetching: false, error: null };
}

export interface MemoryFilters {
  q: string;
  type: MemorySearchParams['type'] | '';
  sensitivity: MemorySearchParams['sensitivity'] | '';
  visibility: MemorySearchParams['visibility'] | '';
  user: string;
  limit: number;
}

export const DEFAULT_MEMORY_FILTERS: MemoryFilters = {
  q: '', type: '', sensitivity: '', visibility: '', user: '', limit: DEFAULT_LIMIT,
};

function toSearchParams(filters: MemoryFilters): MemorySearchParams {
  return {
    q: filters.q || undefined,
    type: filters.type || undefined,
    sensitivity: filters.sensitivity || undefined,
    visibility: filters.visibility || undefined,
    user: filters.user || undefined,
    limit: filters.limit,
  };
}

export interface UseLuminaMemoryResult {
  stats: SectionState<LuminaMemoryStats>;
  results: SectionState<Memory[]>;
  filters: MemoryFilters;
  setFilters: (next: MemoryFilters) => void;
  refetchAll: () => void;
}

/** §3.3: "SERVER-SIDE filtering only — the mock adapter must apply the query params; never
 *  client-mine a full dump." This hook only ever sends `filters` to the backend via
 *  `buildMemorySearchQuery` + `request()` — it never fetches an unfiltered dump and slices it
 *  client-side. The mock adapter's own server-side simulation lives in
 *  `aggregationClient.ts`'s `mockEngramSearch` (which itself calls the same
 *  `applyMemorySearchParams` helper this file's sibling `memorySearch.ts` exports). */
export function useLuminaMemory(): UseLuminaMemoryResult {
  const [filters, setFilters] = useState<MemoryFilters>(DEFAULT_MEMORY_FILTERS);
  const [stats, setStats] = useState<SectionState<LuminaMemoryStats>>(() => initialSection());
  const [results, setResults] = useState<SectionState<Memory[]>>(() => initialSection());

  const fetchStats = useCallback((isRefetch: boolean) => {
    setStats(s => ({ ...s, isRefetching: isRefetch }));
    getAggregationClient()
      .request<LuminaMemoryStats>('lumina', '/engram/stats')
      .then(data => setStats({ data, loading: false, isRefetching: false, error: null }))
      .catch(e => setStats(s => ({
        ...s, loading: false, isRefetching: false,
        error: e instanceof Error ? e.message : String(e),
      })));
  }, []);

  const fetchResults = useCallback((isRefetch: boolean) => {
    setResults(s => ({ ...s, isRefetching: isRefetch }));
    const query = buildMemorySearchQuery(toSearchParams(filters));
    getAggregationClient()
      .request<MemorySearchResponse>('lumina', query)
      .then(res => setResults({ data: res.results, loading: false, isRefetching: false, error: null }))
      .catch(e => setResults(s => ({
        ...s, loading: false, isRefetching: false,
        error: e instanceof Error ? e.message : String(e),
      })));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [filters]);

  useEffect(() => { fetchStats(false); }, [fetchStats]);
  useEffect(() => {
    const id = setInterval(() => fetchStats(true), STATS_POLL_MS);
    return () => clearInterval(id);
  }, [fetchStats]);

  // Every filter change re-runs the (server-side) search — no client-side re-filtering of a
  // cached dump, per §3.3.
  useEffect(() => { fetchResults(false); }, [fetchResults]);

  const refetchAll = useCallback(() => {
    fetchStats(false);
    fetchResults(false);
  }, [fetchStats, fetchResults]);

  return { stats, results, filters, setFilters, refetchAll };
}
