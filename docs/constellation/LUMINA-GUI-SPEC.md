# Lumina Module & Agent Onboarding Wizard — Build Spec
plane_project: LUM
module: Lumina
prefix: LGUI
spec_id: S119-lumina-gui

> RECONSTRUCTION NOTE (2026-07-19, S121 takeover orchestrator): this file was authored
> 2026-07-18 but only ever existed UNTRACKED in the dev-box main checkout; a concurrent
> session's cleanup deleted it before it was committed. This is a faithful reconstruction
> from the takeover orchestrator's full read of the original, same day. Content matches the
> original v1.0; minor whitespace drift possible.

## Metadata
- **Author:** Fable (spec agent), for <operator> (Moose)
- **Session:** S119 · **Date:** 2026-07-18 · **Spec version:** v1.0 (reconstructed 2026-07-19)
- **Estimated total:** ~71h autonomous agent work across 13 items (LGUI-01..13)
- **Context:** The Lumina module of the Constellation portal — deepening CONST-GUI-SPEC.md §5.3
  from a health-gated stub into a full product surface: assistant dashboard, conversations,
  memory browser, persona controls, routing/tools/users/vault panels, and the centerpiece: a
  first-run **agent onboarding wizard** that stands up a working assistant from zero and ends
  with a live, health-checked instance. UI code lands in the **Terminus** repo
  (`constellation-web/src/panels/lumina/**` + small proxy changes); the new assistant-facing
  API lands in the **lumina-constellation** repo (`crates/lumina-core`) — a multi-repo spec
  per the skill's multi-repo rules (one PR per repo, dependency repo merges first). Tracking
  is **Plane LUM** throughout.
- **Prefix note:** `LGUI` presumed free; verify at ingest (`plane_prefix_check LGUI` →
  register → promote, project LUM). If taken, fall back to `LUMUI`.
- **Relationship to CONST (do not duplicate):** the shell, module registry, brand token sheet,
  viz kit, aggregation client, and role model are TERM-tracked work this spec CONSUMES (§10.2).
  CONST-GUI-SPEC.md §5.3 is superseded by this spec's §2.
- **This spec supersedes S97 CONST-07** (Lumina config surface).

## Pre-flight
- Repositories: `moosenet/Terminus` (panels, proxy) AND `moosenet/lumina-constellation`
  (lumina-core API items) — worktrees per repo, PRs per repo
- Build/test: UI via `npm ci && npm run build` in `constellation-web/`; Rust via the compiler
  tool (`compiler_build(module=terminus, …)` / `compiler_build(module=lumina, …)`,
  `mode=test`), never ad-hoc cargo on shared hosts (skill v4.2)
- Secrets (NAMES only; <secret-manager>-provisioned by the operator, S7): existing
  `LUMINA_HTTP_TOKEN` (lumina-side bearer); NEW `CONSTELLATION_LUMINA_TOKEN` (same value,
  materialized into the terminus-primary env so the proxy can authenticate — §6.1)
- Non-secret env: `CONSTELLATION_LUMINA_URL` (still unset in production — operator provides a
  reachable base URL; until then the module stays health-gated and hidden, by design)
- Baselines: both repos' test suites green modulo known environmental failures;
  `constellation-web` typecheck/build green

---

# §0 Ground truth (audited — file citations; do not re-derive, extend)

All paths repo-relative. Lumina files are in `moosenet/lumina-constellation`.

## 0.1 What "an assistant" IS (the thing onboarding must provision)

1. **A layered persona on disk** — root `LUMINA_PROMPT_DIR` (default `~/.lumina/prompt-layers`):
   shared `core-identity.txt`, `behavioral-rules.txt`, `capabilities.txt`, `base-traits.json`;
   per-user `{root}/{user}/`: `trait-vector.json` (`TraitVector {flair, spontaneity, humor,
   focus}`, soft bounds 0.15–0.85, defaults 0.70/0.55/0.65/0.75 — `crates/lumina-core/src/
   prompt/traits.rs`), `trait-modifier.json` (per-user offsets over the shared base;
   `effective = clamp(base+modifier)` — `prompt/multi_personality.rs`), `opinions.txt`,
   `knowledge-digest.txt`, `active-context.txt`. Assembled per-turn by `PromptAssembler`
   (`prompt/mod.rs`, layer order `[identity][rules][capabilities][style][personality]
   [opinions][knowledge][context][memory][proactive][now]`; `LUMINA_DYNAMIC_PROMPT=false`
   falls back to the legacy `Config::system_prompt`).
2. **A first-run state machine that seeds it** — the **Naming Ceremony** (`onboarding/mod.rs`,
   `onboarding/questions.rs`, DPROMPT-15): resumable serde `CeremonyState {is_admin,
   questions, cursor, answers}`; admin flow = 5 `QuestionKind`s (`Name, DetailLevel,
   Personality, Location, UseCase`), non-admin = 2 (`Name, Location`); free-text answers
   interpreted by keyword (`wants_headlines`/`wants_quirky`, ambiguous → defaults);
   `complete()` writes trait-vector + knowledge-digest + active-context + shared
   core-identity + the `onboarding-complete` marker; `detect_first_run()` = marker absent;
   `/setup` re-runs. **The GUI wizard drives THIS machine — it does not reimplement it.**
3. **Encrypted stores** (SQLCipher, fail-closed on wrong key): engram memory
   (`ENGRAM_DB_PATH`, default `~/.lumina/engram.db`; per-user `~/.lumina/users/{id}/engram.db`;
   key `ENGRAM_DB_KEY` — missing = hard error), users (`~/.lumina/users.db`, key
   `LUMINA_USERS_DB_KEY`), settings (`settings.db`, key `LUMINA_SETTINGS_DB_KEY`),
   skills/training (`LUMINA_TRAINING_DB_KEY`), optional `LUMINA_EMBEDDING_KEY`.
4. **Chord-routed inference** — `CHORD_PROXY_URL` is the ONLY hard-required boot var
   (`config.rs`; <secret-manager>→vault→env via `secret_or`). Routing: `LUMINA_FAST_MODEL` /
   `LUMINA_DEEP_MODEL` / `LUMINA_ESCALATION_THRESHOLD` + 3-layer rules (`router.rs`,
   `router_rules.rs`); optional VRAM-swap lifecycle via `CHORD_CONTROL_URL`+`CHORD_API_KEY`.
5. **Tools** — deny-all `ToolGate` + TOML allowlist + optional WASM sandbox + egress
   allowlist (`tool_gate.rs`); catalog discovered from Chord (`tool_discovery.rs`,
   `TOOL_DISCOVERY_ALWAYS_ON` default `discover_tools,searxng_search,engram_query,utc_now,
   health`); per-user ADDITIVE `UserToolConfig` (deny wins → global allow → user extra) in
   `user_config.rs`; learned `Skill` store (`skills/skill_store.rs`); read-only ClawHub
   discovery (`skill_hub/mod.rs`, `LUMINA_CLAWHUB_URL`).
6. **≥1 channel** (`channels/mod.rs` — registry errors with zero): matrix (4 required
   `MATRIX_*` vars), telegram (feature-gated), http (`LUMINA_HTTP_BIND` default
   loopback:3300), cli.
7. **Users** — `UserIdentity {user_id, display_name, matrix_user_id, role: Admin|Member|
   Guest, created_at, last_seen, enabled}` + channel identities + per-role `PermissionSet` +
   cost caps (`UserCostLimit {daily_turn_limit, daily_deep_limit}`, `DailyUsage`; Member
   200/50, Guest 20/5, Admin unlimited; deep exhaustion downgrades to fast) + per-user vault
   credentials (`CredentialType = GoogleAppPassword|CalDavUrl|ImapHost`, keys
   `GOOGLE_APP_PASSWORD_{user_id}` etc., `is_configured` returns bool only) + settings store
   well-known keys (`timezone, location, calendar_url, email, briefing_time, detail_level,
   language`; secret-shaped keys REJECTED from settings — vault only).
8. **Vault** — `~/.lumina/vault.enc`, `LUMINA_KEY_PROVIDER = file|env|interactive`
   (`vault/key_provider.rs`; CLI `VaultWizard` exists); vault init soft at boot, but the DB
   keys above fail closed at store-open.

## 0.2 The HTTP surface that EXISTS today (the gap the spec closes)

`http_server.rs::serve()` on `LUMINA_HTTP_BIND`, bearer `LUMINA_HTTP_TOKEN` (constant-time;
unset token = every authed route 401): `POST /v1/chat/completions` (OpenAI-shaped,
NON-streaming), `GET /dashboard` (server-rendered HTML with DEFAULT params), `GET /mobile`,
public `/manifest.json` + `/sw.js`. **No health endpoint, no JSON APIs for engram/persona/
analytics/users, no WebSocket/SSE.** The Soma pages (`soma/routes/{analytics,credentials,
prompt}.rs`) are render/parse functions wired to NO router — including a complete credentials
form contract (CSRF constant-time, `SecretString`, `CredentialsApplyResult {stored, deleted,
unchanged}`). Terminus already proxies `* /api/lumina/*path` (`src/constellation/proxy.rs`)
with masking/audit/degradation but forwards NO auth to lumina.

**The two structural decisions:**
- **D1 — build the Lumina JSON API in lumina-core** (LGUI-01..04): new authed routes on the
  EXISTING `http_server.rs` router, reusing the existing subsystem code. No second server, no
  new port.
- **D2 — the Terminus proxy authenticates server-side** (LGUI-05): `proxy_lumina` attaches
  `Authorization: Bearer <CONSTELLATION_LUMINA_TOKEN>` (runtime-materialized in the
  terminus-primary env; same value as lumina's `LUMINA_HTTP_TOKEN`). The browser NEVER holds
  a lumina credential. Mirrors S9's single-door.

---

# §1 Product vision

The Lumina module is **the assistant's home** in the portal: where the operator meets, tunes,
and supervises their assistant — and where a brand-new install becomes a living assistant
through a guided onboarding. Register: warm but operator-grade — a *person-shaped* system;
the UI should feel like tending a companion, not administering a daemon. Everything renders
under the Constellation shell in the deep-space brand (CONST §2) with the shared viz kit
(CONST §4). Real-time is honest polling; refetch keeps the frame.

# §2 Module IA — panels (module id `lumina`, healthSystem `lumina`, order after Muse)

| Panel id | Route | Purpose | Min role |
|---|---|---|---|
| `lumina.overview` | `/lumina` | Assistant dashboard: identity card, health, activity, memory/usage tiles | viewer |
| `lumina.chat` | `/lumina/chat` | Conversations with the assistant | operator |
| `lumina.memory` | `/lumina/memory` | Engram browser: search, inspect, stats | operator |
| `lumina.persona` | `/lumina/persona` | Persona & behavior: traits, digest, context, prompt layers | operator |
| `lumina.routing` | `/lumina/routing` | Model & routing: fast/deep, thresholds, Chord lifecycle | viewer (writes: operator) |
| `lumina.tools` | `/lumina/tools` | Tools & skills: gate allowlist, catalog, learned skills, ClawHub | operator |
| `lumina.access` | `/lumina/access` | Users, roles, budgets, per-user credentials, vault status | operator |
| `lumina.setup` | `/lumina/setup` | **The onboarding wizard** (first-run entry + resumable) | operator |

First-run: when `GET /api/lumina/status` reports `onboarding_complete: false` for the admin,
the module landing redirects to `/lumina/setup`, and the Overview canvas card shows
"NEW · needs setup" (amber Badge) with a "Begin setup" primary Button. Viewer sees the card;
the wizard is operator-only.

# §3 Panels in depth (all consume the §7 contracts through the aggregation client)

## 3.1 `lumina.overview` — Assistant Dashboard
- **Identity Card** (brand Card, `glow` when online): assistant display info, StatusPill
  (online/idle/error from `/api/lumina/status`), uptime + version (mono), channel chips (one
  Badge per channel — green=connected, neutral=configured-off, amber=misconfigured).
- **Tile row** (MetricCards): memories (engram count + 24h delta), turns today, deep-turn
  share today, active users, reminders scheduled (degrade honestly when n/a).
- **Charts** (viz kit): *Memory growth* — 30-day area (single series `--series-1`, 10% fill);
  *Routing mix* — 14-day stacked bars fast vs deep (slots 1/2, legend, 2px gaps); *Top tools
  (7d)* — horizontal bar, single hue, value labels at tips (from `/api/lumina/analytics`).
- **Activity feed**: last 20 events (`/api/lumina/analytics?view=events`) in log-line voice.
- States: whole-panel degraded card when module health down; per-section ChartEmpty when a
  store is empty ("No memories yet — they'll appear as you talk").

## 3.2 `lumina.chat` — Conversations
- Single-conversation view v1 (no history-list API — honest scope): thread (user right /
  assistant left, assistant messages carry a small violet NodeBadge dot), mono timestamps,
  Markdown rendering with `textContent`-safe code blocks.
- Composer: multiline, Enter=send / Shift+Enter=newline, `/deep` + `/quick` chips (REAL
  router overrides) prefixing the message; disabled with a "thinking" StatusPill while
  awaiting the NON-STREAMING `POST /api/lumina/v1/chat/completions` (no fake streaming).
- Errors: `error.type` mapped inline (`rate_limit_error` → "Daily turn budget reached",
  amber; `upstream_error` → degraded card "Chord unreachable").
- Session context note: a subtle divider "session resumes · 30 min idle" when the idle
  window elapses (from `LUMINA_CONV_BUFFER_*` semantics).

## 3.3 `lumina.memory` — Engram browser
- Filter row: query (hybrid search), `memory_type` (Episodic/Semantic/Preference/Principle),
  `sensitivity` (7 categories), `visibility` (Private/Shared/System), user scope (admin
  only), limit.
- Results: DataTable — content preview (2-line clamp), type Badge (violet=Principle,
  blue=Semantic, green=Preference, neutral=Episodic — fixed mapping, legend in header),
  sensitivity Badge (Health/Finance/Personal always carry a lock glyph — `is_always_private`),
  confidence (mono 0–1), created_at, access_count. Row → Drawer with full `Memory` record,
  provenance, superseded_by link.
- Stats strip: total, by-type mini bars, DB size, embedding coverage (%), store health
  (key OK / `SecurityViolation` → error card naming the key env — NAME only).
- v1 READ-ONLY (no delete/edit). Empty store → onboarding pointer.

## 3.4 `lumina.persona` — Persona & Behavior
- **Trait quartet**: four horizontal slider rows (flair, spontaneity, humor, focus), each
  showing base marker, modifier delta, clamped effective value; rails render soft bounds
  0.15–0.85; violet fill = effective. 4-axis **radar thumbnail** (single series +
  fleet-default overlay in `--chart-deemphasis`) mirrors the quartet. Admin edits BASE;
  per-user modifier admin-on-behalf v1; Save = `PUT /api/lumina/persona/traits` with
  diff-preview ConfirmDialog (old→new per trait).
- **Knowledge digest** (read-only card) + **Active context** (editable textarea, gated) +
  **Layer inspector**: the 11 assembler layers with per-layer byte/token bars + enabled
  state (read-only; dynamic-prompt-off shows a "legacy prompt mode" warning).
- **Ceremony card**: onboarding marker status + "Re-run naming ceremony" (opens the wizard
  at the Identity step; equivalent of `/setup`).

## 3.5 `lumina.routing` — Model & Routing
- Resolved config card: fast model, deep model, escalation threshold, agentic mode,
  lifecycle wiring — all from `/api/lumina/routing` (env-derived, READ-ONLY; the card
  explains changes are an ops action, lists env var NAMES).
- **Cross-link to the Model Library** (CONST §6): each model name links to `/models/{name}`
  when the models module is available (degrade to plain text).
- "Verify routing" (operator): `POST /api/lumina/routing/verify` — 1-token probe through
  Chord for fast + deep; results as StatusPills + latency. Rule cheatsheet card: the 3
  routing layers rendered from the API (docs never drift from code).

## 3.6 `lumina.tools` — Tools & Skills
- **Gate tab**: allowlist editor — DataTable of the Chord-discovered catalog
  (`/api/lumina/tools`): name (mono), description, permission, source, allowed toggle
  (RoleGate operator; deny-all default surfaced as "everything off until allowed");
  always-on set as locked rows; per-user additive extras/denies expandable (admin). Save =
  `PUT /api/lumina/tools/allowlist` with ConfirmDialog listing adds/removes (green=grant,
  rose=revoke).
- **Skills tab**: learned `Skill` records read-only v1 + ClawHub discovery search (read-only
  by design; `SafetyLevel` Badge: Safe=green, Caution=amber, Dangerous=rose; NEVER an
  install button).
- Sandbox/egress card: WASM sandbox on/off, egress allowlist size (count only).

## 3.7 `lumina.access` — Users, Budgets, Vault
- **Users**: DataTable of `UserIdentity` (display_name, role Badge — violet=Admin,
  blue=Member, neutral=Guest — channel identities, last_seen, enabled toggle); role changes +
  disable via ConfirmDialog (server enforces last-admin protection — surface its error
  verbatim). Per-user budgets: numeric inputs with role defaults as placeholders;
  usage-today bars (amber ≥80%, rose at limit).
- **Credentials** (per user): the three `CredentialType`s as configured/not-configured rows
  (bool ONLY) with set/rotate (write-only password input, cleared on submit) and revoke —
  via `POST /api/lumina/credentials`. Flash messages verbatim (never echo values).
- **Vault status** card: key provider mode, vault reachable, secret-presence matrix — each
  REQUIRED key name (`ENGRAM_DB_KEY`, `LUMINA_USERS_DB_KEY`, `LUMINA_SETTINGS_DB_KEY`,
  `LUMINA_TRAINING_DB_KEY`, optional `LUMINA_EMBEDDING_KEY`, `LUMINA_HTTP_TOKEN`,
  `LUMINA_CHORD_SECRET`) with a present/absent StatusPill. Missing required → "operator
  action" card listing exactly what to add (NAMES + which store fails) — never values,
  never a GUI write path (S7).

# §4 The Agent Onboarding Wizard (`lumina.setup`) — the centerpiece

## 4.1 Frame & principles
Full-canvas experience (module rail collapses; shell frame stays): left = vertical **stepper**
(NodeBadge-style step dots — pending hollow, active violet + corepulse, done green, blocked
amber; connector hairlines `--line-soft`), right = the step card (`Card accent glow` on
active) over the starfield header band (sanctioned here). Every step: title (`--fs-h3`),
purpose line, controls, footer (Back ghost / Continue primary; Continue disabled until the
step validates). The wizard is **resumable and skippable** exactly like the ceremony it
drives; state persists server-side (§4.4).

**Completion criterion: a live, health-checked assistant** — ceremony marker written AND the
Step-8 self-test green on: chat round-trip, engram write/read, tool catalog reachable,
channel connected. Anything less leaves the wizard resumable at the failing step.

## 4.2 The steps

**Step 0 — Welcome & preflight** (auto-runs): the assistant constellation as four NodeBadges
— lumina-core (core), Chord (source), engram store (endpoint), channel (endpoint) — each with
a live StatusPill from `GET /api/lumina/onboarding/preflight`: lumina reachable+authed,
`CHORD_PROXY_URL` configured + Chord probe, vault status + required-key presence, channel
configs, first-run state. All REQUIRED green → Continue; any red → remediation card (env/
secret NAMES only) + Re-check; BLOCKED fail-closed (never "continue anyway" past a missing
`ENGRAM_DB_KEY`). Lumina itself unreachable = module degraded card.

**Step 1 — Name each other** *(ceremony Q1 Name)*: ceremony's own intro prose (`ADMIN_INTRO`,
fetched from `/api/lumina/onboarding/state` — copy comes from the API so GUI and chat flows
never drift), one text input, live preview chip. Validation: non-empty ≤100 chars. Writes
buffered client-side; submitted in order at Step-7 activate (§4.3).

**Step 2 — Style** *(Q2 DetailLevel + Q3 Personality)*: two segmented choices as brand option
cards — Detail: "Headlines" (default) vs "Deep dives"; Personality: "Quirky & warm" (default)
vs "Buttoned-up professional" — each with a one-line consequence preview. Live 4-axis radar
preview updates on toggle (`FOCUS_HEADLINES/FOCUS_DEEP_DIVE`, `QUIRKY_*/PRO_*` constants).
Free-text "or say it your way" escape hatch kept.

**Step 3 — Grounding** *(Q4 Location + Q5 UseCase)*: location input (helper: "weather,
commute, local time"; becomes settings `location` + commute seed) and primary-use input with
suggestion chips feeding `active-context.txt`. Location required; use case optional.

**Step 4 — Mind (models & routing)** — verification, not configuration: read-only routing
card with Model Library links; "Verify" runs the two Chord probes inline — result pills +
latency. If the models module is present, a compact capability strip per model (radar
thumbnail from CONST §8 dimensions). Validation: BOTH probes green; deep probe skippable
ONLY (cold-tier; amber "deep unverified" carries to Step 8).

**Step 5 — Hands (tools)**: three preset cards — **Essentials** (`TOOL_DISCOVERY_ALWAYS_ON`,
preselected locked), **Recommended** (essentials + weather/calendar/web-read, default),
**Custom** (full §3.6 catalog inline). Grant summary: "N tools allowed · everything else
stays off". Writes buffered → `PUT allowlist` at activation.

**Step 6 — Memory**: auto-runs `POST /api/lumina/engram/probe` (open-with-key → write probe
record → hybrid-search it back → secure-delete): three checklist rows animating green.
Privacy defaults card (read-only): "Health/Finance/Personal memories are always private" —
shown, not asked; embedding coverage note when `OLLAMA_EMBEDDING_URL` unset. Fail-closed on
`SecurityViolation` → key remediation card.

**Step 7 — Review & activate**: the whole assistant on one card — name line, style summary +
final radar, location/use chips, models verified pills, "N tools", memory ready, channels —
each row with an edit-link back. Primary action: **"Bring {assistant} online"** (Button
primary lg, glow). On activate (§4.3): submit buffered ceremony answers in order →
`complete` → apply tool allowlist → write settings → auto-advance. Activation idempotent
(half-applied activation resumes from server-side cursor).

**Step 8 — First light (health check)**: auto-runs `GET /api/lumina/onboarding/selftest`:
chat round-trip (the assistant's actual first words in a chat bubble), engram read, tool
catalog, channel status. All green → celebration: `CEREMONY_CLOSING` in the assistant's
bubble, starfield twinkle (reduced-motion: static), "Open conversations" primary + "Go to
dashboard" ghost. Any red → remediation card + honest summary; re-enterable at Step 8.

## 4.3 Ceremony integration decision (normative)
The ceremony API is strictly ordered (`process_answer` advances a cursor), so the wizard
**buffers answers locally per step and submits them in ceremony order at Step-7 activation**
(`POST /api/lumina/onboarding/answer` ×5 → `/complete`). Editing an earlier step before
activation is free; after activation, edits go through the persona panel. A half-submitted
activation resumes from the server-side `cursor` (buffered array replayed from `cursor`,
never restarted).

## 4.4 Progress persistence
Two layers, both server-side (survives browser + lumina restarts): the ceremony's own
`CeremonyState` (persisted at `{layers_root}/{user}/ceremony-state.json` exactly as the agent
loop does — C-3) and a `WizardState {step, buffered_answers, tool_preset, verified: {fast,
deep, engram}, updated_at}` JSON beside it (`wizard-state.json`), written by
`PUT /api/lumina/onboarding/state` on every step transition. Re-entering `/lumina/setup`
resumes at `WizardState.step`. Completing deletes both files (marker = source of truth). No
browser storage (the prefs seam is layout/density only).

# §5 Design & viz notes (deltas only — CONST §2/§4 govern)
New Lumina-specific components (composed from the CONST-17 kit): `TraitSlider` (rail with
soft-bound stops + base/modifier/effective markers), `WizardStepper` (NodeBadge-derived step
rail), `PreflightCheck` (StatusPill + remediation card row), `MemoryTypeBadge` /
`SensitivityBadge` (fixed tone mappings per §3.3), `ChatBubble`. Charts: memory growth =
single-series; routing mix + top-tools = categorical slots; trait radar = slot 1 vs
de-emphasis overlay. No new chart forms beyond CONST §4's set.

# §6 Terminus-side changes (small, LGUI-05)
1. `proxy_lumina` bearer injection: attach `CONSTELLATION_LUMINA_TOKEN` (point-of-use env
   read; absent → forward unauthenticated exactly as today). Never forwarded from, or
   exposed to, the browser; masking/audit unchanged.
2. `/api/health`'s lumina probe: a bare base-URL GET remains the reachability probe — no
   change needed.
3. `registerPanels.ts`: replace `LuminaStubPanel` with the module registration
   (`healthSystem:'lumina'`, `minRole` per §2 table).

# §7 Data contracts — the new Lumina JSON API (LGUI-01..04)

All routes mount at `/api/*` on lumina's HTTP server → reached by the UI as `/api/lumina/*`
through the Terminus proxy. Auth: existing bearer middleware. Every endpoint returns JSON;
errors reuse `{error:{message,type}}`. Admin-gating: endpoints marked (admin) resolve the
acting user via an `X-Lumina-User` header set by the proxy from the Constellation session
(operator session → the admin user; viewer sessions never reach mutating routes — CONST-27
403s them upstream). **CONFIRM BEFORE BUILD (C-1):** the user-resolution header is NEW
convention — confirm against `users/identity.rs` at LGUI-01 build (fallback: admin is the
sole GUI actor v1).

| Endpoint | Source (existing code) | Response sketch |
|---|---|---|
| `GET /api/status` | config presence + channel registry + uptime + `detect_first_run` | `{version, uptime_secs, state:'online'\|'idle'\|'error', channels:[{name, state, configured}], onboarding_complete, dynamic_prompt, chord_configured}` |
| `GET /api/vault/status` | vault + key checks (presence booleans ONLY) | `{key_provider, vault_ok, secrets:[{name, present, required, store}]}` |
| `GET /api/persona?user=` | trait files + digest + context + layer sizes | `{traits:{base, modifier, effective:{flair,spontaneity,humor,focus}}, bounds:{min:0.15,max:0.85}, knowledge_digest, active_context, layers:[{name, bytes, enabled}]}` |
| `PUT /api/persona/traits` (admin) | `SharedPersonality` apply fns | body `{base?, modifier?, user?}` → updated effective |
| `PUT /api/persona/context` (admin) | active-context write | `{active_context}` |
| `GET /api/engram/stats` | `EngramStore` counts | `{total, by_type, by_sensitivity, db_bytes, embedded_pct, store_ok}` |
| `GET /api/engram/search?q=&type=&sensitivity=&visibility=&user=&limit=` | hybrid retrieval | `{results:[Memory]}` — embedding OMITTED (never ship vectors) |
| `POST /api/engram/probe` (admin) | open→insert→search→secure_delete | `{open_ok, write_ok, search_ok, cleanup_ok, detail?}` |
| `GET /api/routing` | router env + rules constants | `{fast_model, deep_model, escalation_threshold, agentic_mode, lifecycle_configured, rules:{layer1:[...], layer2:[...], layer3}}` |
| `POST /api/routing/verify` (admin) | 1-token Chord probe per model | `{fast:{ok, latency_ms, detail?}, deep:{ok, latency_ms, detail?}}` |
| `GET /api/tools?user=` | ToolGate + ToolCatalog | `{always_on:[...], tools:[{name, description, permission, allowed, source}], user_extra, user_denied, sandbox_enabled, egress_allowlist_len}` |
| `PUT /api/tools/allowlist` (admin) | gate save_allowlist + UserToolConfig | body `{allow:[{name,permission}], deny:[...], user?}` → applied summary |
| `GET /api/skills?user=` | SkillStore + SkillHub search (`?hub_q=`) | `{skills:[Skill sans embedding], hub:[SkillInfo incl. safety]}` |
| `GET /api/users` (admin) | users store | `{users:[UserIdentity + identities + limits + usage_today]}` |
| `PUT /api/users/{id}` (admin) | role/enabled/limits | server enforces last-admin rule |
| `POST /api/credentials` (admin) | soma credentials logic adapted JSON | body `{user, set:{google_app_password?, caldav_url?, imap_host?}, revoke:[...]}` → `{stored, deleted, unchanged, flash}`; values SecretString, never logged/echoed |
| `GET /api/analytics?view=summary\|events&days=` | soma analytics render fns → JSON | `{top_tools:[{name,count}], failure_rate, escalation_rate, avg_duration_ms, daily:[{date, turns, deep, tool_calls}], events?:[...]}` |
| `GET /api/onboarding/state` | ceremony + wizard files (§4.4) | `{first_run, is_admin, cursor, questions:[{kind, prompt}], wizard:{step, buffered_answers, tool_preset, verified}}` |
| `PUT /api/onboarding/state` (admin) | wizard-state persist | echo |
| `GET /api/onboarding/preflight` (admin) | composite (§4.2 step 0) | `{checks:[{id, label, ok, required, detail?, remediation?:{env_names:[...]}}]}` |
| `POST /api/onboarding/answer` (admin) | `NamingCeremony::process_answer` | `{cursor, next?:{kind,prompt}, done}` |
| `POST /api/onboarding/complete` (admin) | `NamingCeremony::complete` + settings apply | `{seed:{display_name, traits}, marker_written}` |
| `GET /api/onboarding/selftest` (admin) | composite (§4.2 step 8) | `{chat:{ok, reply?, latency_ms}, engram:{ok}, tools:{ok, count}, channels:[{name, ok}], all_ok}` |

**Confirm-before-build flags:** C-1 (user-resolution header); **C-2** — analytics JSON field
names derive from `soma/routes/analytics.rs` render params (never wired — re-verify);
**C-3** — `ceremony-state.json` persistence location: reuse the SAME location the agent loop
uses so GUI and chat flows share one resumable state; **C-4** — engram probe secure-delete:
confirm `secure_delete.rs` exposes a single-record path; else probe a throwaway
`wizard-probe` user store and delete the file.

# §8 Charts summary (dataviz-rule compliance)
Memory growth (area, 1 series, crosshair+tooltip, table twin) · Routing mix (stacked bars,
2 slots, legend, gaps) · Top tools (h-bars, single hue, tip labels) · Trait radar (4 axes,
1 series + de-emphasis reference, vertex tooltips) · Usage-vs-budget meters (amber/rose
thresholds are SEMANTIC). All via the CONST-17 viz kit; no dual axes; every chart has its
DataTable twin; refetch keeps the frame.

---

# §9 Build decomposition — LUM items LGUI-01..13

Pipeline: full moosenet-spec v4.2+ per item. Multi-repo rule: LGUI-01..04 are
`moosenet/lumina-constellation` PRs; LGUI-05..13 are `moosenet/Terminus` PRs; the API item a
panel consumes merges first. Both repos `mirror_ready` — S1 discipline in every diff.

### LGUI-01: Lumina JSON API foundation — status, vault, persona, routing (read)
- **Priority:** High · **Repo:** lumina-constellation · **Estimate:** 6h
- Read-only foundation routes on the EXISTING authed axum router: `GET /api/status`,
  `GET /api/vault/status`, `GET /api/persona`, `GET /api/routing` per §7, plus the
  `X-Lumina-User` resolution convention (C-1).
- ACs: four read endpoints behind bearer auth, shapes as specced; no secret values in any
  response (negative test); C-1 resolved + documented; README updated; no hardcoded infra;
  existing tests pass.

### LGUI-02: Engram + analytics + tools/skills/users read APIs
- **Priority:** High · **Repo:** lumina-constellation · **Estimate:** 7h
- `GET /api/engram/{stats,search}` (embedding omitted), `GET /api/analytics` (convert the
  never-wired soma render logic to JSON — C-2), `GET /api/tools`, `GET /api/skills`,
  `GET /api/users` per §7. Analytics JSON and soma HTML share ONE aggregation source.
- ACs: five read endpoints; embedding vectors never serialized; C-2 verified; README; no
  hardcoded infra; tests pass.

### LGUI-03: Onboarding API — ceremony endpoints, wizard state, preflight, selftest
- **Priority:** Critical · **Repo:** lumina-constellation · **Estimate:** 8h
- The six `onboarding/*` endpoints: expose the DPROMPT-15 machine over HTTP, persist
  `CeremonyState` + `WizardState` (§4.4, C-3), composite preflight + selftest probes,
  idempotent activation sequencing (§4.3). Atomic wizard-state writes (temp+rename).
- ACs: all six endpoints; ceremony state SHARED with chat flow (C-3 tested); activation
  idempotent + resumable (tests); preflight fail-closed, remediation NAMES only; README;
  no hardcoded infra; tests pass.

### LGUI-04: Write APIs — traits, context, allowlist, users, credentials
- **Priority:** High · **Repo:** lumina-constellation · **Estimate:** 7h
- Admin-gated mutations: `PUT persona/traits`, `PUT persona/context`, `PUT tools/allowlist`,
  `PUT users/{id}`, `POST credentials` (soma form contract → authed JSON; CSRF replaced by
  bearer+session double door, documented), `POST engram/probe`, `POST routing/verify` (C-4).
  Every mutation: admin check → validate → apply via existing subsystem fn (never bypass
  last-admin / SENSITIVE_KEYS) → audit-log (S6-sanitized); credentials SecretString
  end-to-end; trait writes clamp server-side.
- ACs: all seven mutations admin-gated + audited + sanitized; credential values unloggable +
  unechoable (negative tests); subsystem invariants server-side; README; no hardcoded infra;
  tests pass.

### LGUI-05: Terminus proxy — lumina bearer injection + module registration
- **Priority:** High · **Repo:** Terminus · **Estimate:** 2h
- §6: `proxy_lumina` attaches `CONSTELLATION_LUMINA_TOKEN` (point-of-use env read; absent →
  unauthenticated passthrough unchanged); forward `X-Lumina-User` from the session
  principal; register the lumina ModuleDescriptor replacing the stub.
- FILES: src/constellation/proxy.rs, .env.example;
  constellation-web/src/panels/registerPanels.ts, src/panels/lumina/ (stub removal)
- EDGE CASES: token set but lumina rejects (401 upstream) → degraded
  `detail:'lumina auth failed'`, never a raw 401 loop.
- ACs: server-side auth injection tested; browser can neither supply nor read the token
  (enforceHeaders strips browser-supplied Authorization/X-Lumina-User); lumina module
  registered (healthSystem 'lumina', §2 roles); stub removed; README + .env.example; no
  hardcoded infra; tests pass.

### LGUI-06: Overview panel (assistant dashboard)
- **Priority:** High · **Repo:** Terminus · **Estimate:** 5h
- §3.1 in full: identity card, tile row, three charts, activity feed, first-run redirect +
  "needs setup" canvas-card state. FILES: constellation-web/src/panels/lumina/
  {OverviewPanel,IdentityCard}.tsx, src/hooks/useLumina.ts, mock fixtures matching §7.
- ACs: §3.1 complete on mocks + live shapes; charts pass CONST §4 floor; first-run redirect
  + canvas-card state; README; no hardcoded infra; tests pass.

### LGUI-07: Conversations panel
- **Priority:** High · **Repo:** Terminus · **Estimate:** 4h
- §3.2: thread UI, composer with `/deep` `/quick` chips, honest non-streaming wait state,
  error-type mapping, session-idle divider. XSS fixture (script tag in reply) inert.
- ACs: chat end-to-end on mocks; no fake streaming; errors mapped; injection-safe (test);
  viewer sees read-only placeholder; README; no hardcoded infra; tests pass.

### LGUI-08: Memory (engram) browser panel
- **Priority:** Medium · **Repo:** Terminus · **Estimate:** 5h
- §3.3: filter row, results DataTable with type/sensitivity badges, record Drawer, stats
  strip; read-only; server-side filtering only; lock glyph on always-private categories.
- ACs: functional on mocks; badges + lock semantics; no vector/secret data rendered;
  read-only enforced; README; no hardcoded infra; tests pass.

### LGUI-09: Persona & behavior panel
- **Priority:** Medium · **Repo:** Terminus · **Estimate:** 5h
- §3.4: TraitSlider quartet (base/modifier/effective), radar mirror, digest/context cards,
  layer inspector, ceremony card, diff-preview save; client-side clamp rails 0.15–0.85.
- ACs: trait editing end-to-end with diff confirm; bounds both sides; radar + sliders never
  disagree (single state source, tested); README; no hardcoded infra; tests pass.

### LGUI-10: Routing + tools & skills panels
- **Priority:** Medium · **Repo:** Terminus · **Estimate:** 6h
- §3.5 + §3.6: routing read card + verify action + Model Library cross-links; tools gate
  editor with grant/revoke semantics, skills + ClawHub read surfaces, sandbox/egress card;
  allowlist edits batched into one PUT with summarizing ConfirmDialog; ClawHub rows NEVER
  render an install affordance.
- ACs: both panels per spec; env-derived config visibly read-only w/ var NAMES; deny-all
  default legible; grants/revokes semantically colored + confirmed; README; no hardcoded
  infra; tests pass.

### LGUI-11: Access panel (users, budgets, credentials, vault)
- **Priority:** Medium · **Repo:** Terminus · **Estimate:** 5h
- §3.7: users table + role/enable/limits editing, usage-vs-budget meters, credential
  set/rotate/revoke rows (write-only password fields cleared on submit), vault status
  matrix + operator-action card. Sole-admin (n=1); unlimited limits render "∞".
- ACs: all §3.7 surfaces; secret values write-only end-to-end (tested); vault matrix NAMES +
  store impact only; README; no hardcoded infra; tests pass.

### LGUI-12: The onboarding wizard
- **Priority:** Critical · **Repo:** Terminus · **Estimate:** 8h
- §4 in full: WizardStepper + 9 step screens, buffered-ceremony submission (§4.3),
  server-persisted resume (§4.4), preflight/verify/probe/selftest integrations, celebration
  state, brand starfield header. One state machine hook (`useOnboardingWizard`) owning step
  state + buffered answers + server sync per transition; steps are pure renderers; copy
  fetched from the ceremony API (never duplicated in UI); reduced-motion + full keyboard.
- FILES: …/panels/lumina/setup/{WizardShell,WizardStepper,Step0Preflight,Step1Name,
  Step2Style,Step3Grounding,Step4Mind,Step5Tools,Step6Memory,Step7Review,Step8FirstLight}.tsx
- EDGE CASES: ceremony advanced via chat mid-wizard (cursor reconciliation); deep-model
  amber skip carried to step 8; activation retry after partial apply; non-admin → RoleGate.
- ACs: all 9 steps per §4.2 (fields, validation, states, remediation); completion = live
  health-checked assistant (selftest green, demonstrated on mocks); resume-anywhere +
  idempotent activation proven by tests; ceremony copy from the API (grep: no ceremony prose
  literals in UI); README (wizard section); no hardcoded infra; tests pass.

### LGUI-13: Module QA — a11y, brand conformance, docs
- **Priority:** Medium · **Repo:** Terminus · **Estimate:** 3h
- CONST-29-style pass scoped to the Lumina module: keyboard/ARIA/reduced-motion/contrast
  audit across the 8 panels + wizard, chart table-twin check, degraded-copy consistency,
  screenshots-vs-brand-guide, README sweep; ≤30min fixes inline, larger → LUM follow-ups.
- ACs: checklist committed; zero unfixed ≤30min findings; larger filed in Plane (LUM); brand
  screenshots attached; README matches shipped behavior; no hardcoded infra; tests pass.

# §10 Sequencing & dependencies

## 10.1 Order
Phase A (lumina repo): LGUI-01 → 02 → 03 → 04. Phase B (terminus): LGUI-05 (after 01 exists
to test against) → 06/07/08 in parallel → 09/10/11 (need 04) → 12 (needs 03/04/05) → 13
last. Two builders can run the repos in parallel once 01 merges.

## 10.2 CONST dependencies (TERM-tracked; consume, don't duplicate)
- **CONST-16** (shell, module registry v2, card canvas, prefs seam) — required by every panel.
- **CONST-17** (brand token sheet, component kit, viz kit, palettes) — required by all UI
  items; TraitSlider/WizardStepper compose its primitives.
- **CONST-27** (viewer role) — §2 role gating assumes it; until it lands, panels render
  operator-only (single-role fallback, no code change needed).
- NOT required: CONST-18 (`/ws` relay) — lumina has no event stream; polling-only by design.
- Operator prerequisites (ops, not code): `CONSTELLATION_LUMINA_URL` reachable from the
  terminus host; `LUMINA_HTTP_TOKEN` + `CONSTELLATION_LUMINA_TOKEN` provisioned in <secret-manager>
  (same value, two consumers).

## 10.3 Risks
1. Live-code drift on the four C-flags (§7) — each owning item verifies before binding.
2. Ceremony dual-driver races (chat + GUI) — C-3's shared-state decision + cursor
   reconciliation; tested in LGUI-03/12.
3. Secret-handling regressions — every mutating item carries negative tests; reviewers
   instructed to reject on S7 grounds alone.
