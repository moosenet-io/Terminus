// LGUI-06 (§3.1): data hook for the Lumina Overview panel. Mirrors useHarmonyStatus's shape
// (poll + refetch, no props threaded — the panel is mounted standalone by the registry-driven
// router, panels/registerPanels.ts) but composes FOUR independent §7 reads (status, engram
// stats, analytics summary, analytics events) as separate section states, per this item's
// "per-section degradation boundaries" requirement: a slow/failing engram store must not blank
// the identity card or activity feed, and vice versa. Each section degrades honestly on its own.
import { useCallback, useEffect, useState } from 'react';
import { getAggregationClient } from '../lib/aggregationClient';
import type { HealthStatus } from '../lib/aggregationClient';
import type {
  LuminaAnalyticsSummary,
  LuminaAnalyticsEvent,
  LuminaEngramStats,
  LuminaStatus,
} from '../types/lumina';

const POLL_MS = 30_000;

/** One section's fetch state -- loading is true only until the FIRST successful/failed
 *  response; subsequent polls set `isRefetching` instead so ChartCard/section renderers can
 *  keep the previous frame at 0.6 opacity (§2.6) rather than re-skeletoning every 30s. */
interface SectionState<T> {
  data: T | null;
  loading: boolean;
  isRefetching: boolean;
  error: string | null;
}

function initialSection<T>(): SectionState<T> {
  return { data: null, loading: true, isRefetching: false, error: null };
}

function useLuminaSection<T>(path: string): SectionState<T> & { refetch: () => void } {
  const [state, setState] = useState<SectionState<T>>(() => initialSection<T>());

  const fetchOnce = useCallback((isRefetch: boolean) => {
    setState(s => ({ ...s, isRefetching: isRefetch }));
    getAggregationClient()
      .request<T>('lumina', path)
      .then(data => setState({ data, loading: false, isRefetching: false, error: null }))
      .catch(e =>
        setState(s => ({
          ...s,
          loading: false,
          isRefetching: false,
          error: e instanceof Error ? e.message : String(e),
        })),
      );
  }, [path]);

  useEffect(() => { fetchOnce(false); }, [fetchOnce]);

  useEffect(() => {
    const id = setInterval(() => fetchOnce(true), POLL_MS);
    return () => clearInterval(id);
  }, [fetchOnce]);

  return { ...state, refetch: () => fetchOnce(false) };
}

export interface UseLuminaResult {
  status: SectionState<LuminaStatus>;
  engram: SectionState<LuminaEngramStats>;
  analytics: SectionState<LuminaAnalyticsSummary>;
  events: SectionState<{ events: LuminaAnalyticsEvent[] }>;
  /** Whole-panel degraded state (§3.1: "whole-panel degraded card when the lumina health entry
   *  is down") -- reads the shared `/api/health` snapshot rather than opening its own poll,
   *  same convention useActivityFeed's caller (App.tsx) uses for the shell-level health. This
   *  hook fetches it once itself since panels are mounted with no props (registerPanels.ts). */
  health: SectionState<HealthStatus[]>;
  /** Convenience: is the lumina system reporting `available:false` right now. `false` (not
   *  degraded) while health hasn't loaded yet, matching App.tsx's healthLoaded convention. */
  degraded: boolean;
  refetchAll: () => void;
}

export function useLumina(): UseLuminaResult {
  const status = useLuminaSection<LuminaStatus>('/status');
  const engram = useLuminaSection<LuminaEngramStats>('/engram/stats');
  const analytics = useLuminaSection<LuminaAnalyticsSummary>('/analytics?view=summary&days=30');
  const events = useLuminaSection<{ events: LuminaAnalyticsEvent[] }>('/analytics?view=events&days=7');

  const [health, setHealth] = useState<SectionState<HealthStatus[]>>(() => initialSection<HealthStatus[]>());

  const fetchHealth = useCallback((isRefetch: boolean) => {
    setHealth(s => ({ ...s, isRefetching: isRefetch }));
    getAggregationClient()
      .health.list()
      .then(data => setHealth({ data, loading: false, isRefetching: false, error: null }))
      .catch(e =>
        setHealth(s => ({
          ...s,
          loading: false,
          isRefetching: false,
          error: e instanceof Error ? e.message : String(e),
        })),
      );
  }, []);

  useEffect(() => { fetchHealth(false); }, [fetchHealth]);
  useEffect(() => {
    const id = setInterval(() => fetchHealth(true), POLL_MS);
    return () => clearInterval(id);
  }, [fetchHealth]);

  const degraded = !health.loading && (health.data?.find(h => h.system === 'lumina')?.available === false);

  const refetchAll = useCallback(() => {
    status.refetch();
    engram.refetch();
    analytics.refetch();
    events.refetch();
    fetchHealth(false);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [status.refetch, engram.refetch, analytics.refetch, events.refetch, fetchHealth]);

  return { status, engram, analytics, events, health, degraded, refetchAll };
}
