// LGUI-06: shapes for the Lumina Overview panel, bound exactly to LUMINA-GUI-SPEC.md §7
// ("Data contracts — the new Lumina JSON API"). These are the REAL response sketches from
// LGUI-01/02 (merged in the lumina repo) — the mock adapter below (aggregationClient.ts)
// returns objects satisfying these shapes so the panel builds/runs identically against mock
// and http modes; the http adapter's generic `request<T>()` passthrough needs no per-endpoint
// code, just these types at the call site.

/** `GET /api/status` (§7). */
export interface LuminaChannelStatus {
  name: string;
  /** 'connected' | 'configured-off' | 'misconfigured' — free-form per source, mapped to a
   *  Badge tone by `IdentityCard` (green/neutral/amber respectively). */
  state: string;
  configured: boolean;
}

export interface LuminaStatus {
  version: string;
  uptime_secs: number;
  state: 'online' | 'idle' | 'error';
  channels: LuminaChannelStatus[];
  onboarding_complete: boolean;
  dynamic_prompt: boolean;
  chord_configured: boolean;
  /** Not in the §7 sketch verbatim but needed for the Identity Card's display name — the
   *  admin's chosen assistant name from the naming ceremony. Optional: an instance that hasn't
   *  completed onboarding has no name yet, so the card falls back to "Lumina". */
  display_name?: string;
}

/** `GET /api/analytics?view=summary&days=` (§7). */
export interface LuminaTopTool {
  name: string;
  count: number;
}

export interface LuminaDailyPoint {
  date: string;
  turns: number;
  deep: number;
  tool_calls: number;
}

/** `GET /api/analytics?view=events&days=` (§7) — one entry, log-line voice per CONST-GUI-SPEC
 *  §2.2 ("[ok] tool searxng_search 412ms"). */
export interface LuminaAnalyticsEvent {
  ts: string;
  level: 'ok' | 'warn' | 'error';
  /** Pre-formatted body text (sans the "[level]" prefix, which the feed renders itself to
   *  match ActivityFeedCard's convention) — e.g. "tool searxng_search 412ms". */
  text: string;
}

export interface LuminaAnalyticsSummary {
  top_tools: LuminaTopTool[];
  failure_rate: number;
  escalation_rate: number;
  avg_duration_ms: number;
  daily: LuminaDailyPoint[];
  events?: LuminaAnalyticsEvent[];
}

/** `GET /api/engram/stats` (§7). */
export interface LuminaEngramStats {
  total: number;
  by_type: Record<string, number>;
  by_sensitivity: Record<string, number>;
  db_bytes: number;
  embedded_pct: number;
  store_ok: boolean;
  /** Not in the §7 sketch verbatim — a 30-day daily total-count series for the memory-growth
   *  area chart (§3.1/§8). Degrades honestly (empty array) when the store can't derive history. */
  growth_30d?: { date: string; total: number }[];
}
