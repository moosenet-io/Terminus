// CONST-20: Muse module client hooks. Every fetch a Muse panel section makes goes through
// `useMuseSection` below -- it is the one place that implements the "per-endpoint
// degradation is the CENTRAL requirement" rule from the spec item's own brief (the MUSEX-WIRE
// reality: most Muse features exist unwired in production). A single unwired/erroring
// endpoint degrades ONLY the section that called it (via the returned `degraded` value, fed
// straight into `ChartCard`'s `degraded` prop) -- it never throws, never blanks the whole
// panel, and never needs its own try/catch at the call site.
//
// Degradation is keyed on two equivalent "not wired" signals, one per adapter:
//   - httpAdapter: `client.request` throws `Error("HTTP {status} for {path}")` for a non-2xx
//     response (see aggregationClient.ts's `httpJson`). 404/501 are treated as "not yet
//     wired"; any other status/network error is a real (non-degraded) error state instead.
//   - mockAdapter: `mockGetFor` resolves `null` for any pathname with no `MOCK_GET` entry --
//     that IS the mock world's "this route isn't mocked" sentinel (see aggregationClient.ts's
//     own comment on `mockGetFor`). A `null`/`undefined` resolution is therefore treated the
//     same as a 404 by default. Killing an individual mock (delete/rename its `MOCK_GET` key,
//     or return `null` from a matcher) is exactly how CONST-20 was manually verified to prove
//     one dead endpoint collapses only its own section -- see the panel files' top comments.
import { useCallback, useEffect, useState } from 'react';
import { getAggregationClient } from '../lib/aggregationClient';

export interface MuseSection<T> {
  data: T | null;
  loading: boolean;
  /** false = healthy; otherwise the detail string to hand straight to `ChartCard`'s
   *  `degraded` prop (renders the module-standard degraded card, never a crash). */
  degraded: { detail: string } | false;
  refetch: () => void;
}

const NOT_WIRED_STATUS = new Set([404, 501]);

function classifyError(err: unknown): { detail: string } {
  if (err instanceof Error) {
    const m = /^HTTP (\d+) for/.exec(err.message);
    if (m && NOT_WIRED_STATUS.has(Number(m[1]))) {
      return { detail: 'not yet wired' };
    }
    return { detail: err.message };
  }
  return { detail: 'unknown error' };
}

/**
 * Generic per-endpoint Muse fetch. `path` may be `null` to skip fetching entirely (e.g. no
 * channel selected yet for a lineup call) -- returns an idle, non-degraded, non-loading state
 * in that case rather than firing a request.
 */
function useMuseSection<T>(path: string | null): MuseSection<T> {
  const [data, setData] = useState<T | null>(null);
  const [loading, setLoading] = useState(path !== null);
  const [degraded, setDegraded] = useState<{ detail: string } | false>(false);

  const fetchOnce = useCallback(() => {
    if (path === null) {
      setLoading(false);
      setData(null);
      setDegraded(false);
      return;
    }
    setLoading(true);
    getAggregationClient()
      .request<T | null>('muse', path)
      .then(d => {
        if (d === null || d === undefined) {
          // mockAdapter's "not mocked" sentinel -- treat exactly like a 404 from a real backend.
          setDegraded({ detail: 'not yet wired' });
          setData(null);
        } else {
          setDegraded(false);
          setData(d);
        }
        setLoading(false);
      })
      .catch(err => {
        setDegraded(classifyError(err));
        setData(null);
        setLoading(false);
      });
  }, [path]);

  useEffect(() => {
    fetchOnce();
  }, [fetchOnce]);

  return { data, loading, degraded, refetch: fetchOnce };
}

// ── Dashboard (muse.dashboard) ───────────────────────────────────────────────

export interface MuseStats {
  library_size: number;
  active_channels: number;
  pending_items: number;
  last_ingest_at: string | null;
}
export function useMuseStats(): MuseSection<MuseStats> {
  return useMuseSection<MuseStats>('/stats');
}

export interface MuseOnDeckItem {
  id: string;
  title: string;
  kind: string;
  progress_pct: number;
  poster_path?: string;
}
export interface MuseOnDeck {
  items: MuseOnDeckItem[];
}
export function useMuseOnDeck(): MuseSection<MuseOnDeck> {
  return useMuseSection<MuseOnDeck>('/on_deck');
}

export interface MusePremiereItem {
  id: string;
  title: string;
  release_date: string;
  rsvp_count: number;
}
export interface MusePremiere {
  items: MusePremiereItem[];
}
export function useMusePremiere(): MuseSection<MusePremiere> {
  return useMuseSection<MusePremiere>('/premiere');
}

export interface MuseGapItem {
  id: string;
  title: string;
  kind: string;
  detail: string;
}
export interface MuseGaps {
  gaps: MuseGapItem[];
  total: number;
}
export function useMuseGaps(): MuseSection<MuseGaps> {
  return useMuseSection<MuseGaps>('/gaps');
}

// ── Taste (muse.taste) ───────────────────────────────────────────────────────

export interface MuseTastePoint {
  x: number;
  y: number;
  model: string;
}
export interface MuseTasteCluster {
  cluster_id: number;
  label: string;
  points: MuseTastePoint[];
}
export interface MuseTasteClusters {
  clusters: MuseTasteCluster[];
}
export function useMuseTasteClusters(): MuseSection<MuseTasteClusters> {
  return useMuseSection<MuseTasteClusters>('/api/graph/taste-clusters');
}

export interface MuseWatchHistoryPoint {
  date: string;
  [seriesKey: string]: number | string;
}
export interface MuseWatchHistory {
  series: MuseWatchHistoryPoint[];
}
export function useMuseWatchHistory(): MuseSection<MuseWatchHistory> {
  return useMuseSection<MuseWatchHistory>('/api/graph/watch-history');
}

export interface MuseGroupDynamicsRow {
  participant: string;
  watched_together_pct: number;
  favorite_genre: string;
  sessions: number;
}
export interface MuseGroupDynamics {
  rows: MuseGroupDynamicsRow[];
}
export function useMuseGroupDynamics(): MuseSection<MuseGroupDynamics> {
  return useMuseSection<MuseGroupDynamics>('/api/graph/group-dynamics');
}

// ── Channels (muse.channels) ─────────────────────────────────────────────────

export interface MuseChannel {
  id: string;
  name: string;
  item_count: number;
}
export interface MuseChannels {
  channels: MuseChannel[];
}
export function useMuseChannels(): MuseSection<MuseChannels> {
  return useMuseSection<MuseChannels>('/api/channels');
}

export interface MuseLineupItem {
  id: string;
  title: string;
  position: number;
}
export interface MuseLineup {
  channel_id: string;
  lineup: MuseLineupItem[];
}
/** `channelId === null` renders an idle (not degraded, not loading) section -- use this while
 *  no channel is selected yet, so the lineup ChartCard shows its own empty state, not a spurious
 *  "not yet wired" degrade. */
export function useMuseLineup(channelId: string | null): MuseSection<MuseLineup> {
  return useMuseSection<MuseLineup>(channelId ? `/api/channels/${encodeURIComponent(channelId)}/lineup` : null);
}

export interface MuseGuideEntry {
  channel_id: string;
  title: string;
  start: string;
  end: string;
}
export interface MuseGuide {
  entries: MuseGuideEntry[];
}
export function useMuseGuide(): MuseSection<MuseGuide> {
  return useMuseSection<MuseGuide>('/guide');
}

/** Compose/maintenance mutations -- both operator-RoleGated + ConfirmDialog-confirmed at the
 *  call site (ChannelsPanel), never fired directly from a click handler. See the aggregation
 *  client's mockWriteFor comment for why these paths aren't in the original §5.4 route list. */
export function useMuseChannelActions() {
  const composeChannel = useCallback(async (channelId: string) => {
    return getAggregationClient().request('muse', `/api/channels/${encodeURIComponent(channelId)}/compose`, {
      method: 'POST',
    });
  }, []);
  const runMaintenance = useCallback(async (channelId: string) => {
    return getAggregationClient().request('muse', `/api/channels/${encodeURIComponent(channelId)}/maintenance`, {
      method: 'POST',
    });
  }, []);
  return { composeChannel, runMaintenance };
}

/** Same-origin, relative art URL for `<img src>` -- deliberately NOT routed through
 *  `client.request` (that path is JSON-typed; the proxy's `art/` sub-path is raw binary
 *  passthrough, see `proxy.rs`'s module doc). A relative path resolves against the document
 *  origin on its own, so this needs neither `window.location` nor a fetch call. */
export function museArtUrl(kind: string, id: string): string {
  return `/api/muse/art/${encodeURIComponent(kind)}/${encodeURIComponent(id)}`;
}
