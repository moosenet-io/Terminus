// LGUI-06 (§3.1): data hook for the Lumina Overview panel. Mirrors useHarmonyStatus's shape
// (poll + refetch, no props threaded — the panel is mounted standalone by the registry-driven
// router, panels/registerPanels.ts) but composes FIVE independent §7 reads (status, engram
// stats, analytics-for-routing-mix, analytics-for-top-tools, analytics events) as separate
// section states, per this item's "per-section degradation boundaries" requirement: a slow/
// failing engram store must not blank the identity card or activity feed, and vice versa.
// Each section degrades honestly on its own.
//
// Review fix (chart-window finding): §3.1/§8 gives each chart its OWN window — memory growth
// is 30d (engram's own `growth_30d`, unrelated to this endpoint), routing mix is 14d, top
// tools is 7d. The `/analytics?view=summary&days=` endpoint's `days` param scopes BOTH `daily`
// and `top_tools` server-side, so one fetch can't correctly serve two different windows —
// this hook issues two separate requests (`days=14` for routing mix, `days=7` for top tools)
// rather than over-fetching once and rendering an unsliced 30d/14d series into a 14d/7d chart.
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
  /** `?days=14` — backs the Routing mix chart (§3.1: "14-day stacked bars fast vs deep") and
   *  the tile row's "turns today"/"deep-turn share" (today = last entry of this window). Do
   *  NOT source the routing-mix chart from `analyticsTools` below — that's a different window. */
  analyticsRouting: SectionState<LuminaAnalyticsSummary>;
  /** `?days=7` — backs the Top tools chart (§3.1: "Top tools (7d)"). Kept as its own request
   *  (not a client-side slice of the 14-day fetch) because `top_tools` is server-ranked over
   *  whatever `days` window was requested — a 14d-ranked top_tools list is not the same data
   *  as a genuine 7d one. */
  analyticsTools: SectionState<LuminaAnalyticsSummary>;
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
  // Two separate windowed requests -- see analyticsRouting/analyticsTools doc comments above
  // (chart-window review finding: a single over-fetched request can't correctly back two
  // charts with two different windows).
  const analyticsRouting = useLuminaSection<LuminaAnalyticsSummary>('/analytics?view=summary&days=14');
  const analyticsTools = useLuminaSection<LuminaAnalyticsSummary>('/analytics?view=summary&days=7');
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
    analyticsRouting.refetch();
    analyticsTools.refetch();
    events.refetch();
    fetchHealth(false);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [status.refetch, engram.refetch, analyticsRouting.refetch, analyticsTools.refetch, events.refetch, fetchHealth]);

  return { status, engram, analyticsRouting, analyticsTools, events, health, degraded, refetchAll };
}
