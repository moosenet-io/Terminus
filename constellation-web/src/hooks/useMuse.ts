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

// ── Library (muse.library) — MGUI-01 ─────────────────────────────────────────
//
// `GET /api/library` is on Muse's PUBLIC router, so unlike the per-account sections above
// it needs no upstream bearer and populates today. Every field name below was copied from a
// live capture through the proxy, not inferred:
//   {"counts":{"on_disk":1629,"owned":1892,"wanted":0},
//    "owned":[{"availability":"on_disk","backdrop_url":"...","imdb_id":"tt...","kind":"movie",
//              "media_item_id":6655,"media_metadata_id":1225,"monitored":false,
//              "poster_url":"/art/media_metadata/1225","title":"The Martian",
//              "tmdb_id":"286217","tvdb_id":null,"year":2015}],
//    "wanted":[]}

export interface MuseLibraryCounts {
  owned: number;
  on_disk: number;
  wanted: number;
}

export interface MuseLibraryItem {
  media_item_id: number;
  media_metadata_id: number;
  kind: string;
  title: string;
  year: number | null;
  /** `"on_disk"` when a file exists, else `"monitored"`. The badge derives from THIS, never
   *  from `monitored` — a title can be owned-and-unmonitored or monitored-with-no-file, and
   *  re-deriving would mislabel both. */
  availability: string;
  monitored: boolean;
  /** Muse-relative art path. NOT used for `<img src>` — see `museArtUrl`, which adds the
   *  same-origin proxy prefix the browser needs. */
  poster_url: string;
  backdrop_url: string;
  tmdb_id: string | null;
  tvdb_id: string | null;
  imdb_id: string | null;
}

export interface MuseWantedItem {
  monitored_item_id: number;
  media_metadata_id: number;
  library_id: number;
  kind: string;
  title: string;
  year: number | null;
  availability: string;
  poster_url: string;
}

export interface MuseLibrary {
  counts: MuseLibraryCounts;
  owned: MuseLibraryItem[];
  wanted: MuseWantedItem[];
}

/** The poster wall's data. `limit` bounds the page Muse returns — the panel reports the
 *  untruncated `counts.owned` separately so a capped page never reads as the whole library. */
export function useMuseLibrary(limit = 120): MuseSection<MuseLibrary> {
  return useMuseSection<MuseLibrary>(`/api/library?limit=${encodeURIComponent(String(limit))}`);
}

/** One row of the management table (guide screen 03). `GET /api/library/table` returns a BARE
 *  ARRAY, not an envelope — field names copied from a live capture:
 *    [{"cutoff_met":null,"file_count":2,"kind":"movie","media_item_id":2,
 *      "media_metadata_id":2,"monitored":true,"on_disk":true,
 *      "poster_url":"/art/media_metadata/2","quality_profile_id":null,
 *      "quality_profile_name":null,"size_bytes":16893019180,
 *      "title":"10 Things I Hate About You","year":1999}] */
export interface MuseLibraryTableRow {
  media_item_id: number;
  media_metadata_id: number;
  title: string;
  year: number | null;
  kind: string;
  monitored: boolean;
  on_disk: boolean;
  file_count: number;
  size_bytes: number | null;
  quality_profile_id: number | null;
  quality_profile_name: string | null;
  /** `null` when no quality profile / cutoff is configured — which is the norm on this
   *  deployment. A null must NOT be read as "meets cutoff"; see `LibraryTablePanel`. */
  cutoff_met: boolean | null;
}

/** `enabled = false` passes a null path, which `useMuseSection` treats as idle (no request) — so
 *  the grid view does not pay for the table's separate fetch until the toggle asks for it. React
 *  hooks cannot be called conditionally, so this flag is how laziness is expressed. */
export function useMuseLibraryTable(limit = 500, enabled = true): MuseSection<MuseLibraryTableRow[]> {
  return useMuseSection<MuseLibraryTableRow[]>(
    enabled ? `/api/library/table?limit=${encodeURIComponent(String(limit))}` : null,
  );
}

// ── Media detail (MGUI-03) ───────────────────────────────────────────────────
// Shapes copied from a live `GET /api/library/{id}` capture, not inferred.

export interface MuseMediaFile {
  id: number;
  relative_path: string;
  media_info: Record<string, unknown> | null;
  quality_tier_id: number | null;
  release_group: string | null;
  edition: string | null;
  date_added: string | null;
}

export interface MuseMediaDetail {
  found: boolean;
  poster_url: string;
  backdrop_url: string;
  media_item: Record<string, unknown> | null;
  metadata: Record<string, unknown> | null;
  /** Absent (not merely empty) on a `found: false` response — always default before
   *  dereferencing. */
  files: MuseMediaFile[];
  enrichment: unknown[];
  /** `null` for every title sampled on this deployment. A null means NO VERDICT WAS
   *  RECORDED — it does not establish which pass did or did not run, and it MUST
   *  render as "no verdict", never as CONSISTENT. */
  match_verdict: unknown | null;
}

export function useMuseMediaDetail(id: string | null): MuseSection<MuseMediaDetail> {
  return useMuseSection<MuseMediaDetail>(id ? `/api/library/${encodeURIComponent(id)}` : null);
}

// ── Discover (MGUI-04) ───────────────────────────────────────────────────────

export interface MuseDiscover {
  /** Whether a trending provider is configured at all. Distinguishes "TMDb not
   *  set up" from "set up but no snapshot ingested yet" — two different fixes. */
  configured: boolean;
  region: string;
  items: {
    media_metadata_id?: number;
    tmdb_id?: string;
    title: string;
    year?: number | null;
    kind?: string;
  }[];
}

export function useMuseDiscover(): MuseSection<MuseDiscover> {
  return useMuseSection<MuseDiscover>('/api/discover');
}

// ── Subsystems (MGUI-06) ─────────────────────────────────────────────────────

export interface MuseSubsystem {
  key: string;
  label: string;
  concern: string;
  /** The guide's wiring vocabulary: live | worker | seam | unmounted. An
   *  unrecognized value is shown as-is, never coerced to `live`. */
  state: string;
}

export function useMuseSubsystems(): MuseSection<{ subsystems: MuseSubsystem[] }> {
  return useMuseSection<{ subsystems: MuseSubsystem[] }>('/api/subsystems');
}

// ── Taste profile (MGUI-07) ──────────────────────────────────────────────────

export interface MuseTasteProfile {
  account_id: number;
  has_data: boolean;
  /** Empty on this deployment — the genres tables are unpopulated (MUSE #90). */
  genre_lean: { genre: string; weight: number }[];
  decade_lean: { decade: number; weight: number }[];
  centroids: unknown[];
  divergence: {
    adventurousness?: number;
    contrarian_index?: number;
    mainstream_score?: number;
    computed_at?: string;
    blind_spots?: unknown[];
    guilty_pleasures?: { media_metadata_id: number; title: string; rewatch_count: number }[];
  } | null;
  profile: Record<string, unknown> | null;
}

export function useMuseTasteProfile(): MuseSection<MuseTasteProfile> {
  return useMuseSection<MuseTasteProfile>('/api/taste');
}

// ── Curation (MGUI-09) ───────────────────────────────────────────────────────

export interface MuseCurationItem {
  media_item_id?: number;
  media_metadata_id?: number;
  title: string;
  kind?: string;
  /** Server-composed, grounded narration. Rendered VERBATIM — see CurationPanel. */
  reason?: string;
  rationale?: string;
  tag?: string;
  source?: string;
  fit?: number;
  taste_fit?: number;
  score?: number;
}

export function useMuseCuration(): MuseSection<{ account_id: number; recommendations: MuseCurationItem[] }> {
  return useMuseSection<{ account_id: number; recommendations: MuseCurationItem[] }>('/api/curation');
}

// ── Wanted + download queue (MGUI-14) ────────────────────────────────────────

export interface MuseWantedRow {
  monitored_item_id?: number;
  media_metadata_id?: number;
  title: string;
  kind?: string;
  year?: number | null;
  quality_profile_name?: string | null;
  status?: string;
  note?: string;
}

export interface MuseQueueRow {
  id?: number;
  title: string;
  client?: string;
  status?: string;
  progress?: number;
  size_bytes?: number | null;
  eta_seconds?: number | null;
  download_speed?: number | null;
}

export function useMuseRequestsQueue(): MuseSection<{ wanted: MuseWantedRow[]; queue: MuseQueueRow[] }> {
  return useMuseSection<{ wanted: MuseWantedRow[]; queue: MuseQueueRow[] }>('/api/requests/queue');
}

// ── Settings (MGUI-11 / 12 / 13) ─────────────────────────────────────────────
// Shapes copied from a live `GET /api/settings` capture.

export interface MuseSettings {
  master_enabled: boolean;
  acquisition: { enabled: boolean } & Record<string, unknown>;
  adaptation_loop: { enabled: boolean; aggressiveness: number };
  channel_director: { enabled: boolean; serendipity_percent: number };
  discord_bot: { enabled: boolean; promotion_cadence_secs: number; promotion_match_threshold: number; trusted_friends: unknown[] };
  /** ALREADY masked server-side. The panel never renders it regardless — see
   *  IntegrationsSettings: a mask still leaks shape (length, prefix). */
  discord_bot_token_masked: string | null;
  kg_viz: { enabled: boolean; taste_neighbor_threshold: number; watch_history_limit: number };
  question_frequency: { frequency: string; silent_mode: boolean };
  sharing: { granularity: string };
  watch_together: { enabled: boolean };
  whats_hot: { enabled: boolean; source_weights: Record<string, unknown> };
  personas: unknown[];
}

export function useMuseSettings(): MuseSection<MuseSettings> {
  return useMuseSection<MuseSettings>('/api/settings');
}

export interface MuseIndexer {
  id: number;
  name: string;
  enabled: boolean;
  protocol: string;
  privacy: string;
  categories: unknown[];
}

export function useMuseIndexers(): MuseSection<{ configured: boolean; reachable: boolean; indexers: MuseIndexer[] }> {
  return useMuseSection<{ configured: boolean; reachable: boolean; indexers: MuseIndexer[] }>('/api/indexers');
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

/** Muse's real `ChannelSummary` — Muse `src/web/guide.rs:35`, cross-checked against a live
 *  `GET /api/channels` on <host> (`.67:8098`, probed directly, bypassing the proxy).
 *
 *  THERE IS NO `item_count`. An earlier version of this interface declared `{id: string,
 *  name: string, item_count: number}` — a shape taken from the MOCK adapter rather than from
 *  the API, which Muse has never returned. Because the live list is empty today, every read
 *  of it was `undefined` and nothing rendered, so the divergence stayed invisible; the first
 *  real channel would have rendered "undefined items" and a numeric id compared against a
 *  string. Typed from the server struct now, and the mock was corrected to match it. */
export interface MuseChannel {
  id: number;
  name: string;
  kind: string;
  mode: string;
  channel_number: number | null;
  enabled: boolean;
}
export interface MuseChannels {
  channels: MuseChannel[];
}

/** MGUI-10: the LIVE `GET /api/channels` (captured through the proxy on this deployment)
 *  answers a BARE ARRAY — `[]` — while the mock adapter answers the `{channels:[…]}`
 *  envelope this module was originally typed against. Both shapes are therefore accepted
 *  and normalized by `museChannelList` below.
 *
 *  This is not defensive padding: it is a real contract divergence. It happens to be
 *  invisible today only because the array is empty (`data?.channels` is `undefined`, which
 *  `?? []` swallows) — the moment a channel exists, the un-normalized read would render an
 *  empty channel list against a non-empty backend. */
export type MuseChannelsResponse = MuseChannels | MuseChannel[];

/** Normalize either observed `/api/channels` shape to a plain list.
 *
 *  Returns `null` — NOT `[]` — for a shape we do not recognize, and for no data at all. An
 *  earlier version collapsed both into `[]`, which handed callers a value indistinguishable
 *  from a genuinely empty list and let the grid state "GET /api/channels returned an empty
 *  list" about a payload it had never successfully parsed (gpt56). `[]` now means exactly one
 *  thing: the server returned a list and it had no elements. */
export function museChannelList(data: MuseChannelsResponse | null): MuseChannel[] | null {
  if (data === null) return null;
  if (Array.isArray(data)) return data;
  return Array.isArray(data.channels) ? data.channels : null;
}

export function useMuseChannels(): MuseSection<MuseChannelsResponse> {
  return useMuseSection<MuseChannelsResponse>('/api/channels');
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
export function useMuseLineup(channelId: number | null): MuseSection<MuseLineup> {
  // `channelId !== null`, never a truthiness test: channel ids are i64 and 0 is a legal id,
  // which a truthy check would silently treat as "no channel selected" (codex).
  return useMuseSection<MuseLineup>(
    channelId !== null ? `/api/channels/${encodeURIComponent(String(channelId))}/lineup` : null,
  );
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

/** MGUI-10: what `GET /guide` ACTUALLY returns on this deployment, captured through the
 *  proxy: `{"raw":"<!doctype html>\n<html lang=\"en\">…<title>Muse — Channel Guide</title>…"}`.
 *
 *  `/guide` is a rendered HTML PAGE — Muse's own human-facing channel guide — not a
 *  structured programme feed. The proxy wraps a non-JSON upstream body in `{raw}`, which is
 *  why it arrives as JSON at all. The mock adapter answers the `{entries:[…]}` envelope
 *  this module was typed against, so both shapes are declared here.
 *
 *  There is no JSON programme feed behind it today: `/api/guide`, `/guide.json`,
 *  `/api/channels/guide`, `/xmltv` and `/api/epg` all answer 404 (probed directly).
 *
 *  **The grid deliberately does NOT parse `raw`.** Scraping programme blocks out of an HTML
 *  string would manufacture data whose provenance the panel cannot vouch for, and would
 *  silently break on any markup change. When `raw` is present the grid says so and renders
 *  from `/api/channels` alone. */
export type MuseGuideResponse = MuseGuide | { raw: string };

/** Structured programme entries, or `[]` when the response carried none. The second value
 *  reports the HTML-page case specifically, so the UI can explain WHY there are no blocks
 *  instead of implying the schedule is empty. */
export function museGuideEntries(data: MuseGuideResponse | null): {
  entries: MuseGuideEntry[];
  htmlOnly: boolean;
} {
  if (data === null) return { entries: [], htmlOnly: false };
  if ('entries' in data && Array.isArray(data.entries)) return { entries: data.entries, htmlOnly: false };
  if ('raw' in data && typeof data.raw === 'string') return { entries: [], htmlOnly: true };
  return { entries: [], htmlOnly: false };
}

export function useMuseGuide(): MuseSection<MuseGuideResponse> {
  return useMuseSection<MuseGuideResponse>('/guide');
}

// ── Tuner telemetry (MGUI-10) ────────────────────────────────────────────────
//
// The guide's programming-grid footer reads "now · 21:14 · MUSE0001 tuner advertising
// /discover.json". That is REAL and reachable: Muse serves an HDHomeRun-compatible
// discovery document. Live capture through the proxy:
//   {"BaseURL":"http://…:8098","DeviceAuth":"muse","DeviceID":"MUSE0001",
//    "FirmwareName":"muse-tuner","FirmwareVersion":"0.1.0","FriendlyName":"Muse TV",
//    "LineupURL":"http://…/lineup.json","Manufacturer":"Muse",
//    "ManufacturerURL":"http://…/","ModelNumber":"MUSE-TUNER-1","TunerCount":4}
//
// Field names are PascalCase because that is the HDHomeRun wire format, not a style slip.
// Every field below was in that capture; nothing is inferred.

export interface MuseTunerDiscovery {
  DeviceID: string;
  FriendlyName: string;
  ModelNumber: string;
  FirmwareName: string;
  FirmwareVersion: string;
  /** How many concurrent tuners the device advertises. This is a DECLARED capacity, not a
   *  live in-use count — there is no per-tuner occupancy field in the document, so the UI
   *  must never render it as "3 of 4 tuners busy". */
  TunerCount: number;
  /** Absolute upstream URLs (Muse's own origin). Shown as text only — they are not
   *  same-origin, so they are never fetched from the browser. */
  BaseURL: string;
  LineupURL: string;
}

export function useMuseTuner(): MuseSection<MuseTunerDiscovery> {
  return useMuseSection<MuseTunerDiscovery>('/discover.json');
}

/** `GET /lineup.json` — the HDHomeRun lineup the tuner advertises to clients (Plex, etc.).
 *  A BARE ARRAY; it is `[]` on this deployment, so the ELEMENT SHAPE IS UNVERIFIED. It is
 *  typed `unknown[]` and only its LENGTH is used, because typing fields nobody has observed
 *  would be a guess dressed as a contract. */
export function useMuseTunerLineup(): MuseSection<unknown[]> {
  return useMuseSection<unknown[]>('/lineup.json');
}

/** Compose/maintenance mutations -- both operator-RoleGated + ConfirmDialog-confirmed at the
 *  call site (ChannelsPanel), never fired directly from a click handler. See the aggregation
 *  client's mockWriteFor comment for why these paths aren't in the original §5.4 route list. */
/** Body of `POST /channels/{id}/compose` — Muse `src/channels/routes.rs:50`. `show_media_item_ids`
 *  is REQUIRED and must be non-empty; the handler rejects an empty list with 400. Compose is
 *  therefore not a zero-argument "rebuild this channel" trigger: it schedules a session from an
 *  explicit set of shows the caller chooses. */
export interface MuseComposeRequest {
  show_media_item_ids: number[];
  target_session_ms?: number;
}

export function useMuseChannelActions() {
  // Path verified live: `POST /channels/{id}/compose` -> 415 (route present, reached the JSON
  // body extractor), while `POST /api/channels/{id}/compose` -> 404. The previous `/api/`-
  // prefixed path did not exist; it is mounted on Muse's OPEN router at the root, not under
  // the `/api` prefix that carries the channel READ routes (Muse `src/http/mod.rs:212`).
  const composeChannel = useCallback(async (channelId: number, body: MuseComposeRequest) => {
    return getAggregationClient().request('muse', `/channels/${encodeURIComponent(String(channelId))}/compose`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(body),
    });
  }, []);
  return { composeChannel };
}

/** Same-origin, relative art URL for `<img src>` -- deliberately NOT routed through
 *  `client.request` (that path is JSON-typed; the proxy's `art/` sub-path is raw binary
 *  passthrough, see `proxy.rs`'s module doc). A relative path resolves against the document
 *  origin on its own, so this needs neither `window.location` nor a fetch call. */
export function museArtUrl(kind: string, id: string): string {
  return `/api/muse/art/${encodeURIComponent(kind)}/${encodeURIComponent(id)}`;
}

/** MGUI-15: the same-origin art URL at a RENDITION width (MUSE #100).
 *
 *  `width` must be on Muse's rendition ladder — the server answers `400` for
 *  anything else BY DESIGN (an off-ladder width is an amplification vector, so it
 *  is rejected rather than clamped). The ladder is mirrored here as a union type
 *  so an off-ladder value cannot be written in the first place; if Muse's ladder
 *  changes, this type is the thing that must change with it.
 *
 *  Without a width the endpoint serves the FULL-SIZE master — 1.9 MB for one
 *  poster — which is what made the first cut of the poster wall slow. */
export type MuseArtWidth = 160 | 320 | 640;

export function museArtUrlAt(kind: string, id: string, width: MuseArtWidth): string {
  return `${museArtUrl(kind, id)}?w=${width}`;
}

// ── Request lifecycle (MGUI-08) ──────────────────────────────────────────────
//
// Shapes below are transcribed from Muse's own handlers
// (`src/web/dashboard.rs::get_request_detail` / `get_requests_queue`), NOT from a
// live capture: `GET /api/requests` returns `{requests: [], tiers: {}, total: 0}` on
// this deployment, so there is no request id to sample a detail response from. That
// is a statement about what the list endpoint returned, and nothing more — it does
// not establish anything about whether any worker or request path has run.

/** One stop on the lifecycle stepper. Muse emits the fixed happy-path order
 *  `requested → approved → searching → grabbed → available`, each marked
 *  `reached | current | pending` from the row's REAL `status`. An unrecognized
 *  state string is rendered as-is, never coerced. */
export interface MuseRequestStep {
  label: string;
  state: string;
}

/** The `media_requests` row, serialized wholesale by the handler (`"request": request`).
 *  Every field here exists on the Rust `MediaRequest` struct — there is deliberately no
 *  release/score/seeder field, because none is persisted on the row (see the panel). */
export interface MuseRequestRow {
  id: number;
  provider_ids: Record<string, unknown> | null;
  media_kind: string;
  title: string;
  requested_by: string | null;
  status: string;
  /** `NULL` on a row that was never classified. Renders as an absence, never as a tier. */
  tier: string | null;
  quality_profile_id: number | null;
  note: string | null;
  monitored_item_id: number | null;
  created_at: string;
  updated_at: string;
}

/** `GET /api/requests/:id`. A miss returns `{found: false, request_id}` with NO
 *  `request`/`steps` — every consumer must therefore treat those as optional rather
 *  than dereferencing them. */
export interface MuseRequestDetail {
  found: boolean;
  request_id?: number;
  request?: MuseRequestRow;
  status?: string;
  steps?: MuseRequestStep[];
  /** `"denied" | "failed"` when the request ended off the happy path, else `null`. */
  terminal?: string | null;
}

export function useMuseRequestDetail(id: string | null): MuseSection<MuseRequestDetail> {
  return useMuseSection<MuseRequestDetail>(id ? `/api/requests/${encodeURIComponent(id)}` : null);
}

/** A `download_queue` row as `GET /api/requests/queue` serializes it. `request_id` is
 *  what lets a per-request view show the release that was ACTUALLY grabbed for it.
 *
 *  `progress` is hard-coded `null` by the handler — Muse documents it as a SEAM
 *  (qBittorrent per-torrent progress is not persisted). It is typed nullable here so
 *  no caller can default it into a 0% bar. */
export interface MuseDownloadQueueRow {
  id: number;
  request_id: number | null;
  monitored_item_id: number | null;
  release_title: string;
  indexer: string | null;
  protocol: string | null;
  status: string;
  size_bytes: number | null;
  added_at: string;
  progress: number | null;
}

/** The wanted set + download queue. This panel only reads `queue` (filtered to one
 *  `request_id`), so `wanted` is typed loosely on purpose — MGUI-09/14's own queue
 *  surface is the place that renders it. */
export function useMuseDownloadQueue(): MuseSection<{ wanted: unknown[]; queue: MuseDownloadQueueRow[] }> {
  return useMuseSection<{ wanted: unknown[]; queue: MuseDownloadQueueRow[] }>('/api/requests/queue');
}

/** The acquisition-gate slice of `/api/settings`, read narrowly.
 *
 *  Deliberately NOT a general settings hook: this reads exactly the two booleans the
 *  lifecycle panel is allowed to reason about, so it cannot accidentally grow into a
 *  second, competing notion of "what the settings say". `master_enabled` is included
 *  because Muse's own gate is `master_enabled && acquisition.enabled`
 *  (`ExperienceSettings::is_acquisition_enabled`) — see the panel for how that is used
 *  only to make the SAFE verdict stronger, never to manufacture an armed one. */
export interface MuseAcquisitionGate {
  master_enabled: boolean;
  acquisition: { enabled: boolean };
}

export function useMuseAcquisitionGate(): MuseSection<MuseAcquisitionGate> {
  return useMuseSection<MuseAcquisitionGate>('/api/settings');
}
