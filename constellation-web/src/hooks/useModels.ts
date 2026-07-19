// CONST-22: hooks for the Model Library module (`models`), routed through the aggregation
// client only (no direct fetch anywhere here — same rule as every other hook in this app).
import { useState, useEffect } from 'react';
import { getAggregationClient } from '../lib/aggregationClient';
import type {
  ModelsListParams,
  ModelsListResponse,
  ModelDetailResponse,
  MintDimensionsResponse,
} from '../types/models';

function buildQuery(params: Record<string, string | number | boolean | undefined>): string {
  const sp = new URLSearchParams();
  for (const [k, v] of Object.entries(params)) {
    if (v === undefined || v === '') continue;
    sp.set(k, String(v));
  }
  const qs = sp.toString();
  return qs ? `?${qs}` : '';
}

interface UseModelsListResult {
  data: ModelsListResponse | null;
  loading: boolean;
  isRefetching: boolean;
  error: string | null;
  refetch: () => void;
}

/** The paginated, filtered `models.browse` table read (`GET /api/terminus/models`). */
export function useModelsList(params: ModelsListParams): UseModelsListResult {
  const [data, setData] = useState<ModelsListResponse | null>(null);
  const [loading, setLoading] = useState(true);
  const [isRefetching, setIsRefetching] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [tick, setTick] = useState(0);

  // Stable key so effect deps don't churn on a new object identity every render.
  const key = JSON.stringify(params);

  useEffect(() => {
    let cancelled = false;
    const isFirstLoad = data === null;
    if (!isFirstLoad) setIsRefetching(true);

    async function load() {
      try {
        const qs = buildQuery(params as Record<string, string | number | boolean | undefined>);
        const res = await getAggregationClient().request<ModelsListResponse>('terminus', `/models${qs}`);
        if (!cancelled) {
          setData(res ?? { total: 0, refreshed_at: '', models: [] });
          setError(null);
        }
      } catch (e) {
        if (!cancelled) setError(String(e));
      } finally {
        if (!cancelled) {
          setLoading(false);
          setIsRefetching(false);
        }
      }
    }

    load();
    return () => { cancelled = true; };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [key, tick]);

  return { data, loading, isRefetching, error, refetch: () => setTick(t => t + 1) };
}

/** Header stat row (§6.1): fleet/brochure/serving-now counts + `refreshed_at`, computed
 *  from the FULL unfiltered set — deliberately independent of the current browse filters
 *  (no dedicated summary endpoint exists in §8; this reuses the same list endpoint with
 *  `scope=all&limit=500`, which is well within the mock's and any real fleet's size). */
export interface ModelsSummary {
  fleetCount: number;
  brochureCount: number;
  servingNowCount: number;
  refreshedAt: string | null;
}

export function useModelsSummary(): { summary: ModelsSummary | null; loading: boolean; error: string | null } {
  const [summary, setSummary] = useState<ModelsSummary | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    async function load() {
      try {
        const res = await getAggregationClient().request<ModelsListResponse>(
          'terminus',
          '/models?scope=all&limit=500',
        );
        if (cancelled) return;
        const models = res?.models ?? [];
        setSummary({
          fleetCount: models.filter(m => m.in_current_fleet).length,
          brochureCount: models.filter(m => m.brochure_status != null).length,
          servingNowCount: models.filter(m => m.serving_now).length,
          refreshedAt: res?.refreshed_at ?? null,
        });
        setError(null);
      } catch (e) {
        if (!cancelled) setError(String(e));
      } finally {
        if (!cancelled) setLoading(false);
      }
    }
    load();
    return () => { cancelled = true; };
  }, []);

  return { summary, loading, error };
}

interface UseModelDetailResult {
  data: ModelDetailResponse | null;
  loading: boolean;
  error: string | null;
  /** True only when the backend confirmed the model is unknown everywhere (§8: "404 only
   *  when unknown everywhere") — distinct from a plain fetch error. */
  notFound: boolean;
  /** Re-runs the fetch without a full remount — backs the §2.6 inline-error retry affordance. */
  refetch: () => void;
}

/** `models.detail` (`GET /api/terminus/models/{name}`, name pre-encoded by the caller). */
export function useModelDetail(name: string | undefined): UseModelDetailResult {
  const [data, setData] = useState<ModelDetailResponse | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [notFound, setNotFound] = useState(false);
  const [retryTick, setRetryTick] = useState(0);

  useEffect(() => {
    if (!name) {
      setLoading(false);
      return;
    }
    let cancelled = false;
    setLoading(true);
    setNotFound(false);
    setError(null);

    async function load() {
      try {
        const res = await getAggregationClient().request<ModelDetailResponse | null>(
          'terminus',
          `/models/${encodeURIComponent(name as string)}`,
        );
        if (cancelled) return;
        if (res == null) {
          setNotFound(true);
          setData(null);
        } else {
          setData(res);
        }
        setError(null);
      } catch (e) {
        if (!cancelled) setError(String(e));
      } finally {
        if (!cancelled) setLoading(false);
      }
    }

    load();
    return () => { cancelled = true; };
  }, [name, retryTick]);

  return { data, loading, error, notFound, refetch: () => setRetryTick(t => t + 1) };
}

/** MINT dimension scores for a set of models (radar overlays in `models.detail` and
 *  `models.compare`), `GET /api/terminus/mint/dimensions?models=`. */
export function useMintDimensions(modelIds: string[]): {
  data: MintDimensionsResponse | null;
  loading: boolean;
  error: string | null;
} {
  const [data, setData] = useState<MintDimensionsResponse | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const key = modelIds.join(',');

  useEffect(() => {
    if (!key) {
      setData(null);
      setLoading(false);
      return;
    }
    let cancelled = false;
    setLoading(true);
    async function load() {
      try {
        const res = await getAggregationClient().request<MintDimensionsResponse>(
          'terminus',
          `/mint/dimensions?models=${encodeURIComponent(key)}`,
        );
        if (!cancelled) {
          setData(res);
          setError(null);
        }
      } catch (e) {
        if (!cancelled) setError(String(e));
      } finally {
        if (!cancelled) setLoading(false);
      }
    }
    load();
    return () => { cancelled = true; };
  }, [key]);

  return { data, loading, error };
}

/** `models.compare`'s data need: full detail records for 2-4 models, fetched together (a
 *  variable-length array of `useModelDetail()` calls would violate rules-of-hooks, so this is
 *  one effect doing `Promise.all` over the generic `request<T>()` escape hatch instead). */
export function useModelsDetails(names: string[]): {
  data: Record<string, ModelDetailResponse>;
  loading: boolean;
  error: string | null;
} {
  const [data, setData] = useState<Record<string, ModelDetailResponse>>({});
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const key = names.join(',');

  useEffect(() => {
    if (names.length === 0) {
      setData({});
      setLoading(false);
      return;
    }
    let cancelled = false;
    setLoading(true);
    async function load() {
      try {
        const client = getAggregationClient();
        const entries = await Promise.all(
          names.map(async n => {
            const res = await client.request<ModelDetailResponse | null>('terminus', `/models/${encodeURIComponent(n)}`);
            return [n, res] as const;
          }),
        );
        if (!cancelled) {
          const out: Record<string, ModelDetailResponse> = {};
          for (const [n, res] of entries) if (res) out[n] = res;
          setData(out);
          setError(null);
        }
      } catch (e) {
        if (!cancelled) setError(String(e));
      } finally {
        if (!cancelled) setLoading(false);
      }
    }
    load();
    return () => { cancelled = true; };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [key]);

  return { data, loading, error };
}

/** The ≤4-model compare selection, persisted only in URL state (`?m=a&m=b…`, §6.1's
 *  "URL state only" rule — no `client.prefs` entry, no other browser storage). */
export function parseCompareModels(searchParams: URLSearchParams): string[] {
  return searchParams.getAll('m').filter(Boolean).slice(0, 4);
}

export function compareUrl(modelNames: string[]): string {
  const sp = new URLSearchParams();
  for (const m of modelNames.slice(0, 4)) sp.append('m', m);
  return `/models/compare?${sp.toString()}`;
}

/** Simple in-memory max-4 selection helper shared by BrowsePanel's row checkboxes — kept
 *  here (not React state) only for the pure-logic pieces so they're trivially testable;
 *  panels still own the actual `useState` for `selected`. */
export function toggleSelection(selected: string[], name: string, max = 4): { next: string[]; rejected: boolean } {
  if (selected.includes(name)) {
    return { next: selected.filter(n => n !== name), rejected: false };
  }
  if (selected.length >= max) {
    return { next: selected, rejected: true };
  }
  return { next: [...selected, name], rejected: false };
}
