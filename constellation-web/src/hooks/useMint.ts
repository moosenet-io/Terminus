// CONST-23: MINT module data hook. Every read goes through the aggregation client (per-domain
// hook convention, §9) — no direct fetch. Each of the five section endpoints degrades
// independently (item brief edge cases) so one dead endpoint collapses only its own section,
// never the whole page: each accessor below carries its own {data, loading, degraded} triple.
import { useCallback, useEffect, useRef, useState } from 'react';
import { getAggregationClient } from '../lib/aggregationClient';
import type {
  MintSummary,
  MintDimensionsResponse,
  MintMatrixResponse,
  MintContextProfilesResponse,
  MintActivityResponse,
  MintParetoResponse,
  MintBoxResponse,
  MintRunsResponse,
  MintFailuresResponse,
  MintTradeoffsResponse,
} from '../lib/aggregationClient';

export interface MintFilters {
  epoch: string; // 'current' | 'all' | a specific epoch id
  taskCategory: 'all' | 'blitz' | 'multi_file' | 'deep';
  backendTag: 'all' | 'gpu' | 'cpu';
  models: string[]; // <=4, drives emphasis/series assignment everywhere
}

export const DEFAULT_MINT_FILTERS: MintFilters = {
  epoch: 'current',
  taskCategory: 'all',
  backendTag: 'all',
  models: [],
};

interface Slice<T> {
  data: T | null;
  loading: boolean;
  /** {detail} on a fetch failure — the section renders ChartCard's degraded state, not a crash. */
  degraded: { detail?: string } | false;
}

function initialSlice<T>(): Slice<T> {
  return { data: null, loading: true, degraded: false };
}

function filtersToQuery(filters: MintFilters, extra?: Record<string, string>): string {
  const q = new URLSearchParams();
  if (filters.epoch && filters.epoch !== 'current') q.set('epoch', filters.epoch);
  if (filters.taskCategory !== 'all') q.set('task_category', filters.taskCategory);
  if (filters.backendTag !== 'all') q.set('backend_tag', filters.backendTag);
  if (filters.models.length > 0) q.set('models', filters.models.join(','));
  if (extra) for (const [k, v] of Object.entries(extra)) q.set(k, v);
  const s = q.toString();
  return s ? `?${s}` : '';
}

/** Generic per-endpoint fetch: independent loading/degraded state, re-fetches on filter change.
 *  `path` + `extraQuery` are recomputed by the caller on every render (cheap), so this hook
 *  just needs a stable request function reference keyed by the resolved query string. */
function useMintEndpoint<T>(path: string, filters: MintFilters, extra?: Record<string, string>): Slice<T> & { refetch: () => void } {
  const [slice, setSlice] = useState<Slice<T>>(() => initialSlice<T>());
  const query = filtersToQuery(filters, extra);
  // Guards a fetch whose response arrives after a newer one was already issued (fast filter
  // changes) from clobbering the newer result.
  const requestId = useRef(0);

  const load = useCallback(() => {
    const id = ++requestId.current;
    setSlice(prev => ({ ...prev, loading: true }));
    getAggregationClient()
      .request<T>('terminus', `${path}${query}`)
      .then(data => {
        if (id !== requestId.current) return;
        setSlice({ data, loading: false, degraded: false });
      })
      .catch((e: unknown) => {
        if (id !== requestId.current) return;
        setSlice({
          data: null,
          loading: false,
          degraded: { detail: e instanceof Error ? e.message : 'unavailable' },
        });
      });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [path, query]);

  useEffect(() => {
    load();
  }, [load]);

  return { ...slice, refetch: load };
}

export function useMintSummary(filters: MintFilters) {
  return useMintEndpoint<MintSummary>('/mint/summary', filters);
}

export function useMintDimensions(filters: MintFilters) {
  return useMintEndpoint<MintDimensionsResponse>('/mint/dimensions', filters);
}

export function useMintMatrix(filters: MintFilters) {
  return useMintEndpoint<MintMatrixResponse>('/mint/matrix', filters);
}

export function useMintContextProfiles(filters: MintFilters) {
  return useMintEndpoint<MintContextProfilesResponse>('/mint/context-profiles', filters);
}

export function useMintActivity(filters: MintFilters, range: '30d' | '90d' | 'all') {
  return useMintEndpoint<MintActivityResponse>('/mint/activity', filters, { range });
}

export function useMintPareto(filters: MintFilters, language?: string) {
  return useMintEndpoint<MintParetoResponse>('/mint/pareto', filters, mintLanguageExtra(language));
}

// ── CONST-24 additions (C3/C5/C6/C9) ─────────────────────────────────────────
// The Coder section's language control is DELIBERATELY not part of the global `MintFilters`/
// URL-deep-linked object (§7.1: "language (Coder section only -- the one documented scoping
// exception)") -- it's threaded through as an extra query param instead, exactly like the
// existing `range` param on useMintActivity above.

function mintLanguageExtra(language?: string): Record<string, string> | undefined {
  return language && language !== 'all' ? { language } : undefined;
}

export function useMintBox(filters: MintFilters, metric: 'total_time_ms' | 'code_quality_score', language?: string) {
  return useMintEndpoint<MintBoxResponse>('/mint/box', filters, { metric, ...mintLanguageExtra(language) });
}

export function useMintRuns(filters: MintFilters, opts?: { language?: string; failureClass?: string }) {
  const extra: Record<string, string> = {};
  if (opts?.language && opts.language !== 'all') extra.language = opts.language;
  if (opts?.failureClass && opts.failureClass !== 'all') extra.failure_class = opts.failureClass;
  return useMintEndpoint<MintRunsResponse>('/mint/runs', filters, extra);
}

export function useMintFailures(filters: MintFilters) {
  return useMintEndpoint<MintFailuresResponse>('/mint/failures', filters);
}

export function useMintTradeoffs(filters: MintFilters) {
  return useMintEndpoint<MintTradeoffsResponse>('/mint/tradeoffs', filters);
}
