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
import type { LiveSessionsResult, HistorySessionsResult, MuseTerminateResult } from '../lib/aggregationClient';

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
/** MUSE #112: `kind` scopes the fetch SERVER-SIDE.
 *
 *  Filtering in the browser would still ship every row over the wire and still be bounded by
 *  the same limit — so on a library larger than the cap, "all your movies" would mean "the
 *  movies among the first N of everything", which is a confident label over the wrong set.
 *  `undefined` keeps the mixed view. */
export function useMuseLibrary(limit = 120, kind?: 'movie' | 'show'): MuseSection<MuseLibrary> {
  const k = kind ? `&kind=${encodeURIComponent(kind)}` : '';
  return useMuseSection<MuseLibrary>(`/api/library?limit=${encodeURIComponent(String(limit))}${k}`);
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
  }[];  /** MUSE #111: true when a TMDb client exists but is KEY-LESS — it serves movie metadata and
   *  has no trending endpoint, so Discover can never populate. Distinct from having no client
   *  at all, because the operator action differs: add an API key, versus configure TMDb. */
  metadata_provider_only?: boolean;
  /** The server's own explanation of why trending is unavailable. Preferred over any sentence
   *  written in the panel — the code that knows the fact should be the code that states it. */
  reason?: string | null;
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

// ── Liveness / DB probe (MACT-06, MUSE-126) ──────────────────────────────────
//
// `GET /health` is Muse's PUBLIC liveness+DB probe (Muse `src/http/mod.rs::health`) — reused
// as-is, no new endpoint. Field shape lives in `tileFormat.ts`'s `MuseHealthPayload` doc
// comment (transcribed from the handler, not a live capture).

export interface MuseHealth {
  status: string;
  version?: string;
  db: string;
}

export function useMuseHealth(): MuseSection<MuseHealth> {
  return useMuseSection<MuseHealth>('/health');
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
  const list = data === null ? null : Array.isArray(data) ? data : Array.isArray(data.channels) ? data.channels : null;
  if (list === null) return null;
  // Element-level validation, not just container-shape. `[null]` previously reached
  // `buildRows`, where reading `c.id` THREW and took the panel down; `[{}]` produced a row
  // labelled `undefined`. Neither is an empty list and neither is a channel, so an array
  // whose elements are not channels is `null` — unreadable — like any other unknown shape
  // (gpt56). Muse is typed Rust serializing Vec<ChannelSummary> and cannot itself emit
  // these, but the proxy sits in between and `useMuseSection` renders any 2xx body as data.
  return list.every(isMuseChannel) ? list : null;
}

function isMuseChannel(v: unknown): v is MuseChannel {
  if (typeof v !== 'object' || v === null) return false;
  const c = v as Record<string, unknown>;
  // id + name are what every render path dereferences; the rest degrade to a dash on their
  // own, so requiring them would reject a channel over a cosmetic field.
  return typeof c.id === 'number' && typeof c.name === 'string';
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
  /** False whenever the body was not a readable schedule: not an `entries` list of valid
   *  entries, and not an HTML `raw` page.
   *
   *  `null` counts as UNRECOGNIZED, not as an idle sentinel. In practice a null/absent body
   *  is already marked degraded by `useMuseSection` (see its `.then`), and `gridState` ranks
   *  loading and degraded above this flag, so the value cannot change what renders today.
   *  It is reported this way regardless because "recognized" must be a property of the BODY
   *  alone — a parser that returns `true` for "no body" is only safe as long as every caller
   *  happens to check loading and degraded first, and that is not a guarantee a pure function
   *  can make about its callers (codex, gpt56). */
  recognized: boolean;
} {
  // A scalar 2xx body (`true`, `42`, `"x"`) is not an object, and `'entries' in data` THROWS
  // on a primitive — it crashed the panel instead of reaching the unrecognized state (codex).
  if (typeof data !== 'object' || data === null) return { entries: [], htmlOnly: false, recognized: false };
  if ('entries' in data && Array.isArray(data.entries)) {
    return data.entries.every(isMuseGuideEntry)
      ? { entries: data.entries, htmlOnly: false, recognized: true }
      : { entries: [], htmlOnly: false, recognized: false };
  }
  if ('raw' in data && typeof data.raw === 'string') return { entries: [], htmlOnly: true, recognized: true };
  return { entries: [], htmlOnly: false, recognized: false };
}

function isMuseGuideEntry(v: unknown): v is MuseGuideEntry {
  if (typeof v !== 'object' || v === null) return false;
  const e = v as Record<string, unknown>;
  if (
    typeof e.channel_id !== 'string' ||
    typeof e.title !== 'string' ||
    typeof e.start !== 'string' ||
    typeof e.end !== 'string'
  ) {
    return false;
  }
  const start = parseGuideInstant(e.start);
  const end = parseGuideInstant(e.end);
  // `end === start` is allowed: a zero-length programme is drawn as a hairline by
  // blockGeometry, which is a deliberate existing behaviour. Only end BEFORE start is invalid.
  return start !== null && end !== null && end >= start;
}

/** Parse an ISO-8601 instant, rejecting dates that do not exist on the calendar.
 *
 *  `Date.parse` is not enough on its own: it silently ROLLS OVER an out-of-range day, so
 *  "2026-02-30T00:00:00Z" parses happily as 2026-03-02 and the grid would place a programme
 *  on a date the response never carried (codex — verified: Date.parse of that string returns
 *  the March 2 instant, while a month above 12 does return NaN). The calendar fields are
 *  therefore checked explicitly, against the string, before the instant is trusted.
 *
 *  Only the Y-M-D triple is validated this way; it is the part `Date.parse` rolls over. An
 *  explicit UTC offset is left to `Date.parse`, since a legitimate offset can shift the UTC
 *  date and a naive comparison against the parsed value would reject valid timestamps. */
/** Days in a month under the PROLEPTIC GREGORIAN calendar, computed arithmetically.
 *
 *  Deliberately not `new Date(Date.UTC(year, month, 0)).getUTCDate()`, the obvious trick: JS
 *  maps years 0–99 onto 1900–1999, so year 0000 was measured as 1900 and its real February 29
 *  (year zero IS a leap year) was rejected as nonexistent (codex, gpt56). The failure is in
 *  the safe direction — a valid body marked unreadable rather than an invalid one accepted —
 *  but it is still wrong, and the arithmetic form has no such edge. */
function daysInMonth(year: number, month: number): number {
  if (month === 2) {
    const leap = (year % 4 === 0 && year % 100 !== 0) || year % 400 === 0;
    return leap ? 29 : 28;
  }
  return [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31][month - 1];
}

export function parseGuideInstant(value: string): number | null {
  const m = /^(\d{4})-(\d{2})-(\d{2})T/.exec(value);
  if (m === null) return null;
  const year = Number(m[1]);
  const month = Number(m[2]);
  const day = Number(m[3]);
  if (month < 1 || month > 12 || day < 1) return null;
  if (day > daysInMonth(year, month)) return null;
  const parsed = Date.parse(value);
  return Number.isFinite(parsed) ? parsed : null;
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

// ── Import / acquisition activity (MACT-05, MUSE-125) ────────────────────────
// Extends the SAME `GET /api/requests/queue` endpoint `useMuseDownloadQueue` already binds
// (MGUI-09/14) — no new endpoint, no new tracking. That hook left `wanted` typed `unknown[]`
// because MGUI-09/14's queue view never reads it; the Activity panel's Import section needs a
// count and a few display fields, so this adds the authoritative typing for that row instead of
// widening the existing (narrower-purpose) hook's return shape.
//
// Typed from the Rust source, not a live capture — `CONSTELLATION_MUSE_TOKEN` is unprovisioned
// (TERM-549) so this protected route 401s on a fresh session; a live capture isn't possible
// right now (see this item's own note). Mirrors:
//   - `WantedTitleRow` (Muse `src/repo/dashboard.rs`) as JSON-shaped by `get_requests_queue`
//     (`src/web/dashboard.rs`) — a manually-built `json!({...})`, not a struct `Serialize`, so
//     every key below is unconditionally present (no `#[serde(skip_serializing_if)]` involved).
//   - `DownloadQueueEntry` (Muse `src/models/acquisition.rs`), same manual `json!({...})` — see
//     `MuseDownloadQueueRow`'s doc above for the `progress: null` seam this type shares.
export interface MuseWantedTitleRow {
  monitored_item_id: number;
  media_metadata_id: number;
  library_id: number;
  kind: string;
  title: string;
  year: number | null;
  poster_url: string;
}

export function useMuseImportActivity(): MuseSection<{ wanted: MuseWantedTitleRow[]; queue: MuseDownloadQueueRow[] }> {
  return useMuseSection<{ wanted: MuseWantedTitleRow[]; queue: MuseDownloadQueueRow[] }>('/api/requests/queue');
}

// ── Provider search + request (MGUI-16) ──────────────────────────────────────
//
// `GET /api/muse/api/search?q=&kind=movie|series|all` — the metadata-provider fan-out that
// backs the Request page. Every shape below is transcribed from that endpoint's CONTRACT,
// which is final but **NOT DEPLOYED YET** (it is in review at the time this was written).
// Nothing here comes from a live capture, and this comment is the only place that fact can
// be recorded — so no reader mistakes these types for observed behaviour the way the old
// `item_count` channel type was mistaken for one.
//
// The response is deliberately NOT just a result list: it also reports, per provider, what
// was searched and how it went. That is what makes an honest short list possible. On this
// deployment keyless TMDb is movies-only and keyless TVDB is series-only, so losing ONE
// provider loses an entire media kind — a five-result list is then not a short answer, it is
// half an answer, and `complete`/`uncovered_kinds` are the only things that say so.

/** Per-kind outcome inside one provider's entry. `result_count` is what the provider
 *  contributed AFTER `limit` was applied; `provider_returned` is what it handed over before
 *  that. `truncated` is the provider's own verdict — it is never re-derived here by comparing
 *  the two, because only the server knows whether the upstream itself had more to give. */
export interface MuseSearchProviderKind {
  kind: string;
  /** `"ok" | "partial" | "error" | "not_consulted"`. An unrecognized value renders verbatim. */
  status: string;
  /** The provider's own message for a `partial`/`error` kind. `null` when it said nothing. */
  error: string | null;
  result_count: number;
  truncated: boolean;
  provider_returned: number;
  limit: number;
}

/** One metadata provider as the RUNNING deployment reports it. This is the whole point of
 *  the catalog section: the list of providers is an observation about this server, never a
 *  hardcoded roster of metadata APIs that exist in the world. */
export interface MuseSearchProvider {
  name: string;
  /** How the provider is reached (e.g. `radarr_proxy`) — rendered as text, not interpreted. */
  mode: string;
  configured: boolean;
  /** Kinds this provider CAN search at all (keyless TMDb: movies only). */
  searchable_kinds: string[];
  /** Kinds it was actually asked for on THIS query — the intersection with the kind filter. */
  searched_kinds: string[];
  status: string;
  kinds: MuseSearchProviderKind[];
  result_count: number;
}

export interface MuseSearchResult {
  provider: string;
  kind: string;
  title: string;
  year: number | null;
  overview: string | null;
  first_aired: string | null;
  rating: number | null;
  /** Provider-native ids (`{"tmdb":"286217"}`), echoed VERBATIM into a request body. Never
   *  parsed or re-keyed here — Muse owns their meaning. */
  provider_ids: Record<string, unknown> | null;
  /** The PROVIDER's poster, an absolute upstream URL — `null` for a hit with no artwork.
   *  Distinct from Muse's own art: a title Muse does not hold has no `media_metadata` row and
   *  therefore no `museArtUrl` to serve. */
  poster_url: string | null;
  /** TRI-STATE, and the `null` is the whole point:
   *
   *    true   — the hit pins exactly one catalog row and that row is HELD (a media_items row
   *             exists).
   *    false  — definitively not held: either no catalog row at all, or exactly one row and
   *             it holds no file.
   *    null   — UNKNOWN. The hit's identifiers did not agree on a single catalog row, so the
   *             question was not answered.
   *
   *  `null` exists because `media_metadata.imdb_id` has no uniqueness constraint (a plain
   *  index only), so several catalog rows can share one IMDb id while a provider hit is ONE
   *  title. "Some row sharing this id is held" is a different statement from "you hold this
   *  title", and the endpoint refuses to make either.
   *
   *  Typed `boolean | null` deliberately: TypeScript cannot catch a JSON `null` at runtime, so
   *  a `boolean` here would let `!in_library` read a null as "not held" and offer a Request
   *  button for something the operator may already own. Every consumer must branch on all
   *  three — see `ownershipState` in RequestPanel.tsx. */
  in_library: boolean | null;
  /** Muse knows the title (has metadata) but does not necessarily hold it. `in_catalog`
   *  WITHOUT `in_library` is the requestable case — the two must never be conflated.
   *
   *  TRI-STATE for the same reason `in_library` is: when the hit cannot be resolved to a
   *  catalog row, whether Muse knows the title was not answered either. `null` must never be
   *  rendered as "not in the catalog" — see `catalogState` in RequestPanel.tsx. */
  in_catalog: boolean | null;
  /** True when the hit's identifiers matched more than one catalog row — the reason
   *  `in_library` is `null` and `media_metadata_id` could not be pinned.
   *
   *  Nullable rather than required: it is an EXPLANATION for a state `in_library` already
   *  reports on its own, so a body missing it should degrade to "no reason given" rather than
   *  make the whole search unreadable. `in_library` itself is required, because that one
   *  decides whether a write control is offered. */
  ambiguous_match: boolean | null;
  /**
   * WHY the two ownership flags say what they say. Always present.
   *
   *   settled                — exactly one catalog row, nothing contradicting it.
   *                            `in_library` is definite.
   *   absent                 — identifiers were CHECKED and matched nothing. `in_library` is a
   *                            definite `false`. A real negative.
   *   no_indexed_identifier  — NOTHING WAS CHECKED. The hit carried no id the endpoint can
   *                            look up (only tmdb/tvdb/imdb are indexed; tvrage/tvmaze/anilist
   *                            live in a jsonb column and are not queried). Both flags `null`.
   *   ambiguous_rows         — several catalog rows reachable; cannot tell which is this one.
   *   contradicted           — one row, but an identifier it stores disagrees with the hit.
   *
   * This field exists because `in_library: false` used to be returned for a hit that was never
   * looked up at all. "We checked and it isn't there" and "we couldn't check" are different
   * facts, and the UI must not have to infer which one it got.
   *
   * Typed `string`, not a union: the parser accepts any non-empty string and the panel renders
   * an unrecognized value as unknown-with-the-word-shown. New resolutions may be added
   * server-side, and rejecting the whole response over one — or silently mapping it onto a
   * neighbouring case — would be worse than saying "this page does not know that word".
   */
  resolution: string;
  /** `null` whenever the catalog row could not be pinned — including every ambiguous hit. */
  media_metadata_id: number | null;
}

export interface MuseSearchResponse {
  query: string;
  requested_kinds: string[];
  providers: MuseSearchProvider[];
  /** False when the fan-out could not cover everything that was asked for. */
  complete: boolean;
  /** Kinds no provider successfully covered. Non-empty means an ENTIRE kind is missing from
   *  `results` — the results are a subset of the question, not an answer to it. */
  uncovered_kinds: string[];
  results: MuseSearchResult[];
}

function isStringArray(v: unknown): v is string[] {
  return Array.isArray(v) && v.every(x => typeof x === 'string');
}

/** `null` and `undefined` both mean "absent"; anything else must be the named primitive.
 *  Absence is tolerated because a field the panel renders as an absence cannot mislead;
 *  a WRONG TYPE can (a `{}` where a number belongs renders as "[object Object]"). */
function isNullableNumber(v: unknown): boolean {
  return v === null || v === undefined || typeof v === 'number';
}
function isNullableString(v: unknown): boolean {
  return v === null || v === undefined || typeof v === 'string';
}

function isSearchProviderKind(v: unknown): v is MuseSearchProviderKind {
  if (typeof v !== 'object' || v === null || Array.isArray(v)) return false;
  const k = v as Record<string, unknown>;
  return (
    typeof k.kind === 'string' &&
    typeof k.status === 'string' &&
    isNullableString(k.error) &&
    typeof k.result_count === 'number' &&
    typeof k.truncated === 'boolean' &&
    typeof k.provider_returned === 'number' &&
    typeof k.limit === 'number'
  );
}

function isSearchProvider(v: unknown): v is MuseSearchProvider {
  if (typeof v !== 'object' || v === null || Array.isArray(v)) return false;
  const p = v as Record<string, unknown>;
  return (
    typeof p.name === 'string' &&
    typeof p.mode === 'string' &&
    typeof p.configured === 'boolean' &&
    isStringArray(p.searchable_kinds) &&
    isStringArray(p.searched_kinds) &&
    typeof p.status === 'string' &&
    Array.isArray(p.kinds) &&
    p.kinds.every(isSearchProviderKind) &&
    typeof p.result_count === 'number'
  );
}

function isSearchResult(v: unknown): v is MuseSearchResult {
  if (typeof v !== 'object' || v === null || Array.isArray(v)) return false;
  const r = v as Record<string, unknown>;
  if (typeof r.provider !== 'string' || typeof r.kind !== 'string' || typeof r.title !== 'string') return false;
  // Ownership decides whether a WRITE control is offered, so a wrong or missing value here is
  // not a cosmetic defect: a truthy string would offer a request for a title already on disk,
  // and an absent field would offer one for a title whose ownership was never reported.
  //
  // `in_library` is tri-state — `null` is a legal, meaningful value (ambiguous match), and is
  // NOT the same as absent. `undefined` is rejected; `null` is kept and rendered as its own
  // third state.
  if (r.in_library !== null && typeof r.in_library !== 'boolean') return false;
  if (r.in_catalog !== null && typeof r.in_catalog !== 'boolean') return false;
  // `resolution` must be PRESENT and non-empty — it is the only field that distinguishes a
  // checked negative from an unchecked one, so a hit without it cannot be rendered honestly.
  // Its VALUE is not whitelisted: an unrecognized resolution is a state this page has no
  // wording for, which it says out loud (see `ownershipReason`). Whitelisting would reject an
  // otherwise perfectly good response the first time the server grows a sixth case.
  if (typeof r.resolution !== 'string' || r.resolution.trim() === '') return false;
  // Explanatory only — see the field's comment for why an absent one is tolerated.
  if (r.ambiguous_match !== null && r.ambiguous_match !== undefined && typeof r.ambiguous_match !== 'boolean') return false;
  if (!isNullableNumber(r.year) || !isNullableNumber(r.rating) || !isNullableNumber(r.media_metadata_id)) return false;
  if (!isNullableString(r.overview) || !isNullableString(r.first_aired) || !isNullableString(r.poster_url)) return false;
  // `provider_ids` is forwarded verbatim to `POST /requests` and never rendered, so its VALUES
  // are not type-checked here — Muse validates the ids it owns. Its container shape is checked
  // because the panel spreads it into a JSON body, and an array or a scalar there would send
  // Muse something it cannot read.
  if (r.provider_ids !== null && r.provider_ids !== undefined) {
    if (typeof r.provider_ids !== 'object' || Array.isArray(r.provider_ids)) return false;
  }
  return true;
}

/**
 * Parse a `/api/search` body, or `null` if it cannot be read.
 *
 * Same contract as `museChannelList`/`museGuideEntries`: `null` means UNREADABLE, and an
 * empty `results` array means exactly one thing — the server searched and found nothing.
 * Collapsing the two would let the panel print "no matches for X" about a payload it never
 * understood, which is the single failure this page exists to avoid.
 *
 * Note the primitive guard on the first line: `'query' in data` THROWS on a scalar 2xx body
 * (`true`, `42`, `"x"`), and `useMuseSection` hands any 2xx body straight through as data.
 */
export function museSearchResponse(data: unknown): MuseSearchResponse | null {
  if (typeof data !== 'object' || data === null || Array.isArray(data)) return null;
  const d = data as Record<string, unknown>;
  if (typeof d.query !== 'string' || typeof d.complete !== 'boolean') return null;
  if (!isStringArray(d.requested_kinds) || !isStringArray(d.uncovered_kinds)) return null;
  // Element-level validation on BOTH lists. A `[null]` provider reached the catalog render
  // path and threw on `p.name`; a `[null]` result threw on `r.title`. Neither array is empty
  // and neither carries a provider/result, so the body is unreadable — not an empty search.
  if (!Array.isArray(d.providers) || !d.providers.every(isSearchProvider)) return null;
  if (!Array.isArray(d.results) || !d.results.every(isSearchResult)) return null;
  return {
    query: d.query,
    requested_kinds: d.requested_kinds,
    providers: d.providers,
    complete: d.complete,
    uncovered_kinds: d.uncovered_kinds,
    // The ONE normalization in this parser: an absent `ambiguous_match` becomes an explicit
    // `null` so the declared type is the truth rather than a near-truth. Nothing else is
    // defaulted — a defaulted value is an invented one.
    results: d.results.map(r => ({ ...r, ambiguous_match: r.ambiguous_match ?? null })),
  };
}

/** The kind filter's vocabulary. This is the SEARCH endpoint's vocabulary (`series`), not the
 *  library's (`show`) — an unknown `kind` is a 400, so it is a union type rather than a
 *  string, and the two vocabularies are deliberately not unified behind this panel's back. */
export type MuseSearchKind = 'all' | 'movie' | 'series';

/**
 * Run a provider search. `query` is the SUBMITTED term, never the live input value: an empty
 * `q` is a 400, and firing per keystroke would hammer every upstream provider. A `null`/blank
 * query passes a null path, which `useMuseSection` treats as idle — no request, no data, and
 * therefore nothing the panel may say about results.
 */
export function useMuseSearch(query: string | null, kind: MuseSearchKind): MuseSection<unknown> {
  const q = query === null ? '' : query.trim();
  return useMuseSection<unknown>(
    q === '' ? null : `/api/search?q=${encodeURIComponent(q)}&kind=${encodeURIComponent(kind)}`,
  );
}

/** Body of `POST /api/muse/requests`.
 *
 *  `quality_profile_id` is REQUIRED even though it is nullable on the persisted row: Muse's
 *  `has_matching_capability` check needs Prowlarr configured AND a profile id, and rejects the
 *  request with 400 without one. Typed non-optional so a caller cannot omit it. */
export interface MuseCreateRequestBody {
  provider_ids: Record<string, unknown>;
  kind: string;
  title: string;
  quality_profile_id: number;
}

/** `POST /requests` — Muse mounts request creation at the ROOT of its open router
 *  (`http/requests.rs`), not under the `/api` prefix that carries the request READ routes.
 *  That asymmetry is Muse's, not a typo: `/api/requests` is the list, `/requests` is the
 *  create. The aggregation client adds the `/api/muse` proxy prefix, so this is
 *  `POST /api/muse/requests` at the browser. */
export function useMuseCreateRequest() {
  const submit = useCallback(async (body: MuseCreateRequestBody) => {
    return getAggregationClient().request<unknown>('muse', '/requests', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(body),
    });
  }, []);
  return { submit };
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

// ── Maestro Activity sessions (MACT-03, MUSE #123) ───────────────────────────
//
// `client.muse.sessions.live()`/`.history()` are typed methods on the aggregation client (see
// their doc comments in `aggregationClient.ts`), not paths fed through the generic
// `useMuseSection(path)` above. That mirrors the CONST-21/CGUI-08 precedent already in this
// file's sibling module (`client.mint.*` / `client.models.*`, see the NOTE above `MOCK_GET` in
// `aggregationClient.ts`): once a typed client method exists, panels/hooks call IT directly
// rather than re-deriving the same fetch through the untyped path-dispatch table — a second
// implementation of the same request is exactly the kind of drift this spec's epic forbids.
// `useMuseTypedSection` below reproduces `useMuseSection`'s exact `{data, loading, degraded,
// refetch}` contract and polling shape (same three-state UX every Muse panel already expects)
// so MACT-04 gets identical behaviour regardless of which path a hook takes underneath —
// "inheriting per-endpoint degradation unchanged" holds either way, because both typed methods
// already degrade to `{available:false, detail}` themselves (never throw), same as
// `terminus.activity()`.

/** Generic per-endpoint Muse fetch, sourced from an already-degrading typed client method
 *  (`{available, detail?, ...data}`) instead of a raw path. Same shape/contract as
 *  `useMuseSection` above — see that function's doc and the module note just above this one for
 *  why sessions hooks use this instead of `useMuseSection(path)` directly. */
function useMuseTypedSection<T extends { available: boolean; detail?: string }>(
  fetcher: () => Promise<T>,
  deps: readonly unknown[],
): MuseSection<T> {
  const [data, setData] = useState<T | null>(null);
  const [loading, setLoading] = useState(true);
  const [degraded, setDegraded] = useState<{ detail: string } | false>(false);

  // eslint-disable-next-line react-hooks/exhaustive-deps -- `fetcher` is rebuilt by callers
  // every render from `deps`; keying the effect on `deps` (not `fetcher`) avoids a fetch loop.
  const fetchOnce = useCallback(() => {
    setLoading(true);
    fetcher()
      .then(res => {
        if (!res.available) {
          setDegraded({ detail: res.detail ?? 'unavailable' });
          setData(null);
        } else {
          setDegraded(false);
          setData(res);
        }
        setLoading(false);
      })
      .catch(err => {
        // Defensive only -- the typed client methods this feeds never throw (they resolve
        // `available:false` on every failure mode instead), but a hook that could still throw
        // past its own `.catch` would break the "never crashes the panel" contract silently.
        setDegraded({ detail: err instanceof Error ? err.message : 'unknown error' });
        setData(null);
        setLoading(false);
      });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, deps);

  useEffect(() => {
    fetchOnce();
  }, [fetchOnce]);

  return { data, loading, degraded, refetch: fetchOnce };
}

/** LIVE pane data (`GET /api/sessions/live` via `proxy_muse`). `source` on the resolved envelope
 *  is always `"muse-derived"` in H1 -- see `LiveSessionsResult`'s doc comment for the H2 flip
 *  this is designed to make visible rather than silent. */
export function useMuseLiveSessions(): MuseSection<LiveSessionsResult> {
  return useMuseTypedSection<LiveSessionsResult>(() => getAggregationClient().muse.sessions.live(), []);
}

/** HISTORY pane data (`GET /api/sessions/history?limit=`). `source` is always `"muse-history"` --
 *  Muse's permanent role, unaffected by the H2 live-source flip. */
export function useMuseSessionHistory(limit?: number): MuseSection<HistorySessionsResult> {
  return useMuseTypedSection<HistorySessionsResult>(
    () => getAggregationClient().muse.sessions.history(limit),
    [limit],
  );
}

/** The terminate mutation. Returns a typed [`MuseTerminateResult`] (never throws) plus an
 *  `inFlight` flag for the confirm-dialog button — MACT-04's concern, not this hook's, is
 *  turning `kind` into copy/toasts. */
export function useMuseTerminateSession() {
  const [inFlight, setInFlight] = useState(false);
  const terminate = useCallback(async (sessionKey: string, reason?: string): Promise<MuseTerminateResult> => {
    setInFlight(true);
    try {
      return await getAggregationClient().muse.sessions.terminate(sessionKey, reason);
    } finally {
      setInFlight(false);
    }
  }, []);
  return { terminate, inFlight };
}
