# Personal & Life — domain index

[← tools index](../README.md)

The `personal-life` domain is the largest single grouping in `terminus_personal` — finance,
health, vehicle, habits, travel, home, media, and general life-admin integrations. It spans
**17 tools**, listed below with a one-line description and an exact action count read off each
module's own `register()`/`register_all()` call site in source (not an estimate — every count
below was verified by reading the actual `Box::new(...)` registration calls, since some modules
register a different tool set depending on runtime config — e.g. `<media-service>`, `commute`, and
`weather` swap in `NotConfiguredStub` placeholders when their API keys are absent, and `google`
spans three files (`caldav.rs`, `imap.rs`, `smtp.rs`) summed together).

Six of these modules (marked **[deep-dive]** below) have a full per-tool reference page written
for this documentation pass — exact input schemas, output shapes, every validated branch, error
paths, env vars, and worked examples, all sourced directly from the Rust implementation. The
remaining ten are listed here with their verified tool count and a one-liner from the module's own
top-of-file doc comment; their deep-dive pages are a separate documentation pass.

| Tool | Actions | What it does | Page |
| --- | --- | --- | --- |
| `ledger` | 8 | Finance tracking via the Actual Budget HTTP API — accounts, transactions, budget summaries, category spend, balances, recent activity. | [`ledger.md`](ledger.md) **[deep-dive]** |
| `vitals` | 11 | Health tracking — weight, exercise, sleep, a generic multi-metric daily log, trends, CSV import, and an LLM-generated fitness/health program. | [`vitals.md`](vitals.md) **[deep-dive]** |
| `meridian` | 5 | SIMULATED paper-trading crypto portfolio sandbox (portfolio, live market data, AI/rule-based analysis, HTML report, reset) — no real money, no real orders, ever. | [`meridian.md`](meridian.md) **[deep-dive]** |
| `relay` | 8 | Vehicle/maintenance tracking via the LubeLogger REST API — vehicles, fuel log, service records, odometer, cost summary, maintenance history. | [`relay.md`](relay.md) **[deep-dive]** |
| `myelin` | 9 | LLM/AI cost tracking straight from Postgres via parameterized `sqlx` — today/weekly/monthly spend, runaway-request detection, burn-rate projection, by-model/by-user breakdowns, daily-cap check. | [`myelin.md`](myelin.md) **[deep-dive]** |
| `crucible` | 10 | Learning-tracker system (books, courses, certs, hobbies, skills) — SSH-exec to a remote fleet-host script (not HTTP), with an explicit "assumed but unverified" backend contract documented in source. | [`crucible.md`](crucible.md) **[deep-dive]** |
| `odyssey` | 8 | Trip planning — bucket list, loyalty-card points, trip log, deals, destination research, itinerary optimization. | [`odyssey.md`](odyssey.md) |
| `hearth` | 7 | Pantry/meal-planning via Grocy — pantry list/add, meal plan, shopping list, "what can I make", recipe search, stock check. Replaced a legacy Python tool that used `shell=True`. | [`hearth.md`](hearth.md) |
| `<media-service>` | 4 | Read-only media request queries against <media-service> (Plex/Jellyfin request management) — status, requests, request count, search. | [`<media-service>.md`](<media-service>.md) |
| `media` | 3 | **S94.** Sovereign media-stack orchestration domain (Radarr/Sonarr/Prowlarr/qtor/Plex/<media-service>/TMDb) — one thin, env-config-backed client per service, `media_domain_status`, and (MEDIA-02) `media_search` (fuzzy TMDb title resolution) + `media_status` (cross-service presence/quality). Request/organize/recommend tools land in MEDIA-03..07. | [`media.md`](media.md) |
| `commute` | 4 | Traffic-aware routing and incident data (TomTom) plus a Bay Area public-transit planner (511.org). | [`commute.md`](commute.md) |
| `weather` | 1 | Current conditions and forecasts via OpenWeatherMap. | [`weather.md`](weather.md) |
| `news` | 3 | Headlines, search, and topic feeds (NewsAPI / GNews). | [`news.md`](news.md) |
| `council` | 4 | The "Obsidian Circle" deep-reasoning council — convene, presets, status, history — routed through the Chord proxy. | [`council.md`](council.md) |
| `lumina_ext` | 6 | The remaining `lumina_*` tools not yet moved to a dedicated module — ClawHub search/skill-detail, AICPB rankings, ClawMart browse, the Claw awesome-list, and a generic web-fetch tool. | [`lumina_ext.md`](lumina_ext.md) |
| `seer` | 3 | Research-backend integration via typed HTTP — query, status, recent. | [`seer.md`](seer.md) |
| `google` | 8 | Calendar (CalDAV: today, week, add, conflicts) and email (IMAP read: inbox/read/summarize; SMTP send) — spans `caldav.rs` + `imap.rs` + `smtp.rs`, all behind one `GoogleConfig`. | [`google.md`](google.md) |

## Common patterns across the domain

A few conventions recur across most of these 17 modules, worth knowing before reading any single
page:

- **Config-gated stub registration.** Several modules (`<media-service>`, `commute`, `weather`,
  `google`) register a `NotConfiguredStub` placeholder tool per real tool name when their
  required env vars are absent, rather than omitting the tool from the registry entirely — a
  caller sees the tool exists but gets a clear "not configured" response, instead of a
  tool-not-found error. Other modules (`ledger`, `vitals`, `relay`, `myelin`) instead register
  the real tool unconditionally and return `ToolError::NotConfigured` at call time. `crucible`
  does the same at the `run_subcommand`/`ssh_exec` layer.
- **Two distinct non-HTTP transports.** Most modules are typed `reqwest` HTTP clients against a
  self-hosted service. Two exceptions on this page: `myelin` talks to Postgres directly via
  `sqlx`, and `crucible` SSHes into a remote fleet host and execs a script — see
  [`crucible.md`](crucible.md) for why that's a deliberate, explicitly-flagged architectural
  choice rather than an oversight.
- **PII remediation (2026-07).** Several modules had compiled-in fleet-host paths, directories,
  or scripts removed in favor of required-at-runtime env vars with no fallback (`vitals`'s
  `VITALS_IMPORT_DIR`, `crucible`'s `CRUCIBLE_SCRIPT`) — these now fail clean with
  `ToolError::NotConfigured` rather than silently defaulting to an internal path.
- **SIMULATED-only boundaries.** `meridian` is the one module on this page with a hard,
  structural safety boundary: it is paper-trading only, with no toggle anywhere in its code path
  that could route to a real exchange.

[← tools index](../README.md)
