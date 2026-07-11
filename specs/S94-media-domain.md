# Media Domain — Sovereign Media Orchestration (Lumina as surface)
plane_project: TERM
module: Terminus
prefix: MEDIA
spec_id: S94-media-domain

## Metadata
- **Author:** <operator> (Moose)
- **Session:** S94
- **Date:** (current)
- **Lumina version:** 1.0.0
- **Module version:** Terminus (terminus-rs infra layer) + optional Engram tie-in (toggleable)
- **Estimated total:** ~46h (8 items: 7 code + 1 doc)
- **Context:** A new **media** tool domain in the Rust terminus-rs infra hub, giving Lumina full
  conversational control of the self-hosted media stack: search, download/request, organize, and personal
  recommendations. Lumina (personality agent) is the interaction surface; these tools are the muscle the
  Terminus tool-subagent chains from conversational intent. Built sovereign — vault-backed secrets, PII
  gate, no third-party MCP server, everything through the one hardened hub. It orchestrates the REAL stack
  (Radarr/Sonarr/Prowlarr/qtor/Plex) rather than wrapping <media-service>'s thin API — <media-service> participates
  for request-tracking + discovery where it's genuinely the right layer, but is not the interface.

  **Design pillars:**
  1. **Conversation-first.** Tools return compact, natural-language-friendly results Lumina can talk about —
     not raw JSON dumps. Fuzzy natural-language titles ("that dark sci-fi thing with the AI") resolve to real
     media IDs via TMDb lookup.
  2. **Tiered mutation safety** (recommended model): reads free; a specific unambiguous request the user
     explicitly asked for executes with a light "here's what I grabbed" (no blocking gate); ambiguous / bulk
     / high-impact (whole series, huge 4K remux, "everything by X") require explicit confirm; destructive
     (delete/remove/purge/quality-profile change) require hard typed confirmation. Confirmation weight scales
     with irreversibility + ambiguity, never a blanket rule.
  3. **Toggleable taste-memory module.** Media works fully STATELESS (Plex history + arr data drive
     recommendations). With the memory toggle ON, recommendations are enriched by Lumina's Engram memory of
     the user's tastes + curation. The toggle is a hard on/off — media functions without it; flipping it off
     never breaks media, only de-personalizes suggestions.

## Pre-flight
- Repository: standalone `terminus` (terminus-rs) on Gitea
- Vault secrets required (all via vault, never literals): `RADARR_URL` + `RADARR_API_KEY`, `SONARR_URL` +
  `SONARR_API_KEY`, `PROWLARR_URL` + `PROWLARR_API_KEY` (or indexer equivalents), `QTOR_URL` +
  `QTOR_CREDS` (download client), `PLEX_URL` + `PLEX_TOKEN`, `JELLYSEERR_URL` + `JELLYSEERR_API_KEY`,
  `TMDB_API_KEY` (title resolution). Add any missing to the vault + `.env.example` (names only).
- Plane access: via the Terminus Plane tool (per v3.5 skill — one sanctioned path).
- Baseline tests: record terminus-rs current count — never regress.
- All internal/LAN except TMDb lookup (the one external call — title→ID resolution; no PII sent).

---

## Design Overview (read before executing)

**Tool surface (the media domain), grouped by capability:**
- **Search/resolve (read):** resolve fuzzy title → real media, check library presence, availability, quality.
- **Request/download (mutation, tiered):** add/request a movie (Radarr) or series/season/episode (Sonarr),
  which drive the download client (qtor) via the arr apps; track via <media-service> where useful.
- **Organize (mutation, tiered→destructive):** library management, quality profiles, collection curation,
  cleanup of watched.
- **Recommend/engage (read, memory-optional):** recommendations from Plex watch history + (toggle on) Lumina
  taste memory; what's on deck / continue watching; analytics.

**Layer roles:** Radarr=movies, Sonarr=TV, Prowlarr/indexers+qtor=acquisition, Plex=library/consumption/
history, <media-service>=request-tracking + discovery feed (secondary, not the interface), TMDb=title resolution.

**Lumina-as-surface contract:** every tool's response is shaped for the personality agent to narrate — a
short natural-language summary + structured data the subagent can act on. No raw dumps. Ambiguity is
surfaced as options Lumina can ask about ("did you mean the 2021 or 1984 version?").

---

### MEDIA-01: Media domain scaffold + vault-backed service clients
- **Priority:** High
- **Labels:** terminus, media, scaffold
- **Agent:** claude
- **Estimate:** 6h
- **Description:** Establish the `media` tool domain in terminus-rs and the vault-backed clients for each
  service (Radarr, Sonarr, Prowlarr, qtor, Plex, <media-service>, TMDb). No user-facing tools yet — the client
  layer + config + auth all tools build on.

  ## FILES
  - `src/tools/media/mod.rs` — domain registration + shared types
  - `src/tools/media/clients/{radarr,sonarr,prowlarr,qtor,plex,<media-service>,tmdb}.rs` — one thin client each
  - `src/config.rs` — media service URL/key helpers (vault lookups, no literals)
  - `.env.example` — document media service env var NAMES (no values)

  ## APPROACH
  1. Each client reads its URL + credential via `vault::manager().get()` — never literals, never std::env for secrets.
  2. Thin, typed clients: the operations the domain needs (search, add, status, library, history), not a
     full API mirror.
  3. Graceful per-service degradation: a service being unreachable disables its tools with a clear message,
     never crashes the domain (e.g. Plex down → library tools unavailable, search/request still work).
  4. Audit-log mutations (sanitized per S6); reads need not log.

  ## TEST PLAN
  - `cargo test --workspace`
  - Unit: each client constructs from vault config; missing key → clear error, not panic (negative test)
  - Unit: unreachable service → tools disabled with message, domain still loads (negative test)
  - Verify no hardcoded URLs/keys; secrets via vault
  - Verify no PII in code

  ## EDGE CASES
  - A service not configured (no vault key) → its tools absent/disabled with a setup hint, others work
  - Credential present but wrong (401) → clear per-service error surfaced to Lumina, not a crash

  ## ACCEPTANCE CRITERIA
  - [ ] `media` domain registered; per-service clients build from vault config
  - [ ] Missing/unreachable service degrades that service's tools only, domain still loads (negative test)
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] Secrets accessed via SecretManager, not env vars
  - [ ] All existing tests still pass

---

### MEDIA-02: Search + fuzzy title resolution (read)
- **Priority:** High
- **Labels:** terminus, media, search
- **Agent:** claude
- **Estimate:** 6h
- **Description:** The read/search surface: resolve a natural-language title to real media (TMDb), and check
  presence/availability/quality across Radarr/Sonarr/Plex. Conversation-first responses.

  ## FILES
  - `src/tools/media/search.rs` — search/resolve tools + response shaping
  - tests

  ## APPROACH
  1. `media_search(query)` — resolve fuzzy query via TMDb to candidate title(s) + IDs; where ambiguous,
     return ranked options (Lumina asks the user).
  2. `media_status(id)` — is it in Radarr/Sonarr already, is it in Plex (available to watch), what quality.
  3. Shape responses for narration: short summary + structured options; never a raw JSON dump.
  4. No mutations here — pure read.

  ## TEST PLAN
  - `cargo test --workspace`
  - Unit (mocked TMDb/arr/plex): fuzzy query → ranked candidates; exact query → single resolve
  - Unit: ambiguous query returns options, not a wrong guess (negative test)
  - Unit: response shape is narration-friendly (summary + structured), asserted
  - Verify no hardcoded infra; secrets via vault

  ## EDGE CASES
  - Query resolves to nothing → "couldn't find that", suggest refinements, not an error
  - Multiple strong matches (remake/original) → return both with disambiguating detail (year/director)
  - Already-in-library → say so (don't offer to re-download without noting it's present)

  ## ACCEPTANCE CRITERIA
  - [ ] Fuzzy query resolves via TMDb to real media IDs; ambiguity returns ranked options
  - [ ] Library/availability/quality status across arr + Plex
  - [ ] Responses narration-shaped (summary + structured), not raw dumps (asserted)
  - [ ] No hardcoded infrastructure values; secrets via vault
  - [ ] All existing tests still pass

---

### MEDIA-03: Request/download with tiered mutation safety
- **Priority:** Critical
- **Labels:** terminus, media, download, mutation, safety
- **Agent:** claude
- **Estimate:** 7h
- **Description:** The acquisition surface with the tiered confirmation model. Add/request movies (Radarr) +
  TV (Sonarr) which drive qtor; track via <media-service> where useful. Confirmation weight scales with
  irreversibility + ambiguity.

  ## FILES
  - `src/tools/media/request.rs` — request/download tools + the tiering logic
  - tests

  ## APPROACH
  1. Tier the action before executing:
     - **specific + unambiguous + user-asked** (one movie / one named season) → execute, return "here's what
       I grabbed" (light, non-blocking).
     - **ambiguous / bulk / high-impact** (whole series, huge 4K remux over a size threshold, "everything by
       X") → return a confirmation request with the specifics (title, year, size, quality) — do NOT execute
       until confirmed.
     - The tier decision is explicit + testable (a function mapping request shape → tier), not vibes.
  2. Execute via Radarr/Sonarr → download client; register <media-service> request where it adds tracking value.
  3. Every mutation audit-logged (sanitized). Size/quality surfaced so Lumina can tell the user before big pulls.
  4. Never auto-execute an ambiguous or bulk request — that path MUST hit confirmation.

  ## TEST PLAN
  - `cargo test --workspace`
  - Unit: tiering function — specific→light, bulk/ambiguous/oversized→confirm-required (negative test: a bulk
    request never returns "executed" without a confirm step)
  - Integration (mocked arr/qtor): a confirmed request drives the add; an unconfirmed bulk request does NOT
  - Unit: size/quality surfaced in the confirmation payload
  - Verify mutations audit-logged sanitized; no hardcoded infra; secrets via vault

  ## EDGE CASES
  - Request for something already in library/queue → say so, don't duplicate
  - Oversized single item (a 4K remux) → treated as high-impact → confirm even though it's "one item"
  - arr accepts but download client rejects → surface the real failure, don't report false success

  ## ACCEPTANCE CRITERIA
  - [ ] Tiering: specific→light execute; ambiguous/bulk/oversized→confirm-before-execute (negative test: no bulk auto-execute)
  - [ ] Requests drive Radarr/Sonarr→download client; <media-service> tracking where useful
  - [ ] Size/quality surfaced before big pulls
  - [ ] Mutations audit-logged, sanitized per S6
  - [ ] No hardcoded infrastructure values; secrets via vault
  - [ ] All existing tests still pass

---

### MEDIA-04: Organize + destructive-op hard gating
- **Priority:** High
- **Labels:** terminus, media, organize, mutation, safety
- **Agent:** claude
- **Estimate:** 6h
- **Description:** Library organization — quality profiles, collection curation, cleanup of watched — with
  DESTRUCTIVE operations (delete media, remove from library, purge, profile changes) behind hard typed
  confirmation, same discipline as destructive ops elsewhere in the fleet.

  ## FILES
  - `src/tools/media/organize.rs` — organize/cleanup tools + destructive gating
  - tests

  ## APPROACH
  1. Non-destructive organize (tag, collection add, view) → tiered like MEDIA-03.
  2. DESTRUCTIVE (delete/remove/purge/quality-profile change that could trigger re-download or data loss) →
     HARD confirmation: explicit typed confirm, the action names exactly what will be deleted/changed, and
     it never fires on a light ack.
  3. "Clean up watched" and similar bulk-destructive → enumerate what would be removed, confirm, then act.
  4. All destructive actions audit-logged with the exact target (sanitized).

  ## TEST PLAN
  - `cargo test --workspace`
  - Unit: destructive op requires typed confirm; a light ack does NOT trigger it (negative test)
  - Unit: bulk cleanup enumerates targets before acting (negative test: no blind purge)
  - Integration (mocked): confirmed delete removes; unconfirmed does nothing
  - Verify destructive actions audit-logged with target; no hardcoded infra; secrets via vault

  ## EDGE CASES
  - "Clean up watched" that would remove something unwatched-by-another-user (multi-user Plex) → flag, don't
    silently remove
  - Profile change that would re-download a whole library → treat as high-impact destructive, hard-confirm
  - Delete of something not present → no-op with a clear message

  ## ACCEPTANCE CRITERIA
  - [ ] Destructive ops require hard typed confirmation, naming the exact target (negative test)
  - [ ] Bulk cleanup enumerates before acting; no blind purge (negative test)
  - [ ] Non-destructive organize uses the MEDIA-03 tiering
  - [ ] Destructive actions audit-logged with target, sanitized
  - [ ] No hardcoded infrastructure values; secrets via vault
  - [ ] All existing tests still pass

---

### MEDIA-05: Recommendations + engagement (stateless core)
- **Priority:** High
- **Labels:** terminus, media, recommend, plex
- **Agent:** claude
- **Estimate:** 6h
- **Description:** The recommendation + engagement surface, STATELESS core (no Lumina memory yet — that's
  MEDIA-06's toggle). Recommendations from Plex watch history + arr data; what's on deck / continue watching;
  basic analytics. Conversation-shaped for Lumina.

  ## FILES
  - `src/tools/media/recommend.rs` — recommendation + engagement tools (stateless)
  - tests

  ## APPROACH
  1. `media_recommend()` — from Plex watch history (genres, directors, actors, recency) + library, produce
     suggestions with a narration-friendly rationale ("because you watched X").
  2. `media_on_deck()` / continue-watching / recently-added — engagement surface.
  3. Basic analytics (viewing stats) where Plex/Tautulli-style data is available.
  4. Stateless: no Engram calls here — this must work with the memory module OFF. MEDIA-06 layers memory ON.

  ## TEST PLAN
  - `cargo test --workspace`
  - Unit (mocked Plex): history → recommendations with rationale; on-deck returns current items
  - Unit: works with NO memory module (stateless path asserted)
  - Unit: responses narration-shaped
  - Verify no hardcoded infra; secrets via vault

  ## EDGE CASES
  - Sparse history (new user) → fall back to library/trending, note the thin signal
  - Multi-user Plex → per-user history if available, don't blend users
  - Plex unreachable → recommendations degrade to arr/trending, say so

  ## ACCEPTANCE CRITERIA
  - [ ] Recommendations from Plex history + library with narration-friendly rationale
  - [ ] On-deck / continue-watching / recently-added engagement tools
  - [ ] Fully functional STATELESS (no memory module) — asserted
  - [ ] No hardcoded infrastructure values; secrets via vault
  - [ ] All existing tests still pass

---

### MEDIA-06: Toggleable taste-memory module (Engram tie-in)
- **Priority:** Medium
- **Labels:** terminus, media, engram, memory, toggle
- **Agent:** claude
- **Estimate:** 6h
- **Description:** The toggleable personalization layer: when ON, enrich MEDIA-05 recommendations with
  Lumina's Engram memory of the user's tastes + curation. Hard on/off toggle — media works fully without it;
  flipping off de-personalizes but never breaks media.

  ## FILES
  - `src/tools/media/taste_memory.rs` — the memory-enrichment layer + toggle
  - `src/config.rs` — `MEDIA_TASTE_MEMORY_ENABLED` feature flag (non-secret behavioral config)
  - tests

  ## APPROACH
  1. A feature flag (`MEDIA_TASTE_MEMORY_ENABLED`, default OFF) gates the entire module. OFF → MEDIA-05's
     stateless path is used unchanged.
  2. ON → read the user's taste/curation signals from Engram (liked/disliked, curation notes, stated
     preferences) and blend into the recommendation ranking + rationale ("you told me you're into slow-burn
     sci-fi").
  3. WRITE-BACK (optional within this module): capture engagement signals (what was requested, watched,
     dismissed) into Engram as taste memory — so curation improves over time. This write path also gated by
     the flag.
  4. The module NEVER hard-depends on Engram being present — if the flag is on but Engram is unreachable,
     degrade to stateless with a logged note, don't break recommendations.

  ## TEST PLAN
  - `cargo test --workspace`
  - Unit: flag OFF → stateless path, no Engram calls (negative test)
  - Unit: flag ON → recommendations incorporate taste memory + rationale reflects it
  - Unit: flag ON but Engram unreachable → graceful degrade to stateless, logged (negative test)
  - Unit: write-back captures engagement signals only when flag on
  - Verify no hardcoded infra; secrets via vault; no PII leaked in taste signals stored

  ## EDGE CASES
  - Flag on, empty taste memory (cold start) → behaves ~stateless, starts learning
  - Conflicting signals (liked then disliked a genre) → recency-weight, don't hard-flip
  - Engram write fails → recommendation still returns, write logged as failed, not surfaced as an error

  ## ACCEPTANCE CRITERIA
  - [ ] `MEDIA_TASTE_MEMORY_ENABLED` flag (default OFF) gates the whole module
  - [ ] OFF → stateless MEDIA-05 path, no Engram calls (negative test)
  - [ ] ON → recommendations + rationale incorporate taste memory
  - [ ] ON + Engram unreachable → graceful degrade to stateless (negative test)
  - [ ] No hardcoded infrastructure values; secrets via vault
  - [ ] All existing tests still pass

---

### MEDIA-07: Lumina surface integration (conversational contract + subagent wiring)
- **Priority:** High
- **Labels:** terminus, media, lumina, integration
- **Agent:** claude
- **Estimate:** 5h
- **Description:** Make Lumina the interaction surface: ensure the media domain's tools are discoverable +
  chainable by the Terminus tool-subagent from conversational intent, and that responses drive natural
  Lumina narration (including the confirmation prompts, which Lumina delivers conversationally).

  ## FILES
  - `src/tools/media/surface.rs` — tool descriptions/intent hints for subagent selection; confirmation-prompt
    shaping for Lumina
  - tests

  ## APPROACH
  1. Rich tool descriptions/intent metadata so the subagent picks the right media tool from fuzzy intent
     ("put something on" → recommend; "grab that show" → search→request chain).
  2. Confirmation prompts (from MEDIA-03/04 tiering) are shaped so Lumina delivers them in-voice ("that's a
     ~60GB 4K grab — want me to go ahead?"), not as raw tool output.
  3. Multi-step chains (search → resolve → status → request) compose cleanly for the subagent.
  4. No mutation logic here — this is surface/wiring; the gates live in 03/04.

  ## TEST PLAN
  - `cargo test --workspace`
  - Unit: intent metadata routes representative phrases to the right tool/chain
  - Unit: a confirmation prompt is narration-shaped, carries the specifics (title/size/quality)
  - Integration (mocked subagent): "grab that show I was watching" composes search→status→request with the
    confirm gate intact
  - Verify no hardcoded infra; secrets via vault

  ## EDGE CASES
  - Intent spans multiple tools → the chain composes, confirm gate still fires at the mutation step
  - Under-specified intent → surface as a question Lumina asks, not a wrong action
  - The subagent must not be able to bypass the confirm gate via chaining (assert the gate holds mid-chain)

  ## ACCEPTANCE CRITERIA
  - [ ] Tool intent metadata routes conversational phrases to correct media tools/chains
  - [ ] Confirmation prompts narration-shaped with specifics, delivered in Lumina's voice
  - [ ] Multi-step chains compose; the mutation confirm gate holds mid-chain (negative test)
  - [ ] No hardcoded infrastructure values; secrets via vault
  - [ ] All existing tests still pass

---

### MEDIA-08: Documentation
- **Priority:** Medium
- **Labels:** terminus, media, docs
- **Agent:** gemini
- **Estimate:** 4h
- **Type:** documentation
- **Description:** Document the media domain: the tools, the tiered mutation-safety model, the taste-memory
  toggle, and how Lumina surfaces it.

  ## AUDIENCE
  <operator> (operator) + future contributors.

  ## OUTLINE
  - Overview (~150w): what the media domain does, sovereign design, Lumina as surface.
  - The stack it orchestrates (~150w): Radarr/Sonarr/Prowlarr/qtor/Plex/<media-service>/TMDb roles.
  - Mutation-safety tiers (~200w): read / light / confirm / hard-confirm — with examples of each.
  - Taste-memory toggle (~150w, operator): how to turn personalization on/off, what it does, cold-start.
  - Example conversations (~150w): "put something on", "grab that show", "clean up watched" — showing the gates.

  ## SOURCES
  - MEDIA-01..07 implementation
  - Vault secret names (by name only, no values)

  ## TONE
  Technical reference; plain-language operator sections. No hardcoded infra values.

---

## Notes for the executing agent
1. **Sovereign build — no third-party MCP server.** This is a native terminus-rs domain, vault-backed,
   PII-gated. Do not shell out to or wrap an external media MCP server.
2. **Orchestrate the real stack, not <media-service>'s thin API.** <media-service> participates for request-tracking +
   discovery; Radarr/Sonarr/Prowlarr/qtor/Plex do the real work.
3. **Tiered mutation safety is the load-bearing safety design** — read free, specific-light, ambiguous/bulk/
   oversized-confirm, destructive-hard-confirm. The subagent must not bypass a gate via chaining (MEDIA-07
   asserts this).
4. **Memory is a toggle, default OFF.** Media works fully stateless; the taste-memory module only enriches.
   Never let the memory path become a hard dependency.
5. **Lumina is the surface.** Responses and confirmation prompts are shaped for the personality agent to
   narrate in-voice — not raw JSON. Emphasis on tastes + personal curation when memory is on.
6. **Plane via the Terminus Plane tool; full v3.5 pipeline** (worktree → test → dual review → merge), ingest
   into Plane project TERM.

## Operator execution constraints (S94, verbatim)
- Wait to BUILD until after the plane-helper agent finishes all its modification-task queue — the Plane tool
  is required for this build. Use the full build-pipeline skill and method.
- Do NOT under any circumstances touch the running ARR stack. Observe only — no writes to any code or API.
- Do NOT crash Plex or any ARR-stack component (most run in docker CTs).
- For now: PREPARE the project only — organize the work and get ready for Plane to be available to write to
  the Terminus project as a sub-scope.
