# Tool index

[← docs index](../README.md)

Terminus provides ~53 **tools**, one per integrated service (`src/registry.rs`'s
`register_all` / `register_personal` — see
[`architecture/federation.md`](../architecture/federation.md) for which
registry serves which tool). Each tool exposes a set of **actions** — the
individually-named MCP callables an MCP client sees in `tools/list` (e.g. the
GitHub tool's `github_create_repo`, `github_list_repos`, …). Those actions vary
with the backing service and change over time, so counts here are approximate;
across all tools there are ~302 individual MCP actions in total. This page
groups every tool into one of five domains plus the **MINT** flagship harness,
with a one-line description sourced from that tool's own top-of-file doc comment
or registration site — never guessed. Each tool links to its
reference page(s) under `docs/tools/<domain>/`; those pages are
filled in by sibling doc pages, one per tool, covering the exact input
schema, output shape, error paths, and a worked example.

Tool counts below are read directly off each module's `register()` /
`register_all()` call site (`Box::new(...)` entries registered into the
`ToolRegistry`) as of this doc pass — they will drift as modules gain or lose
tools; treat them as approximate, not a frozen contract.

## MINT flagship

MINT is Terminus's flagship harness: the model-intake / serving-profile
system that loads a fleet model, runs graduated context/code/agent suites
against it, and stores a derived operational profile (safe/absolute context
ceilings, throughput curve, recommended timeouts, degradation point, and — as
of the serving-profile extension — per-(model × backend) launch runtime,
measured VRAM/RAM peak, cold-load time, and `keep_warm`/`exclusion_reason`
metadata) in Postgres. It ships two front doors over the same library entry
points ([`src/intake/`](../../src/intake/)):

- The **`intake`** tool module (6 MCP tools: `model_intake`,
  `model_intake_status`, `model_intake_compare`, `model_intake_fleet`, the
  read-only `model_fleet_catalog`, and the read-only
  `model_discovery_brochure`) — callable from any MCP client.
  `model_fleet_catalog` is the SQL-free coverage registry: it reads the
  persisted Model Fleet Catalog and returns, per model, one cell per
  (test_type × task_category) with its coverage status (`run` | `stale` |
  `not_run` | `non_viable`), metrics (pass_rate, n_samples, variance), last run,
  and harness_version, plus a `not_run`/`stale` gap summary so "what has NOT
  been run" is one field away. Filters (all optional): `model`, `status` (e.g.
  `not_run`), `test_type`; `format` is `json` (default, structured) or
  `markdown` (a compact coverage matrix). Read-only — it reads what the MINT
  harness's end-of-run refresh persisted, and never recomputes.
  `model_discovery_brochure` (DISC-02, see below) is the sibling read tool
  over the newer discovery brochure — a different lifecycle stage, never
  confused with the fleet catalog above.
- The **`mint`** CLI binary ([`src/bin/mint.rs`](../../src/bin/mint.rs)) — a
  clap-derived subcommand tree (`mint sweep coder`, `mint sweep assistant`,
  `mint case`, `mint gaps`, `mint gpu status/acquire/release`, `mint
  supervisor run/install/uninstall`, `mint fetch-model`) that is a more
  discoverable operator front door over the *same* run functions the legacy
  standalone binaries (`intake_coder_sweep`, `intake_coder_case`,
  `intake_coder_gaps`, `intake_assistant_sweep`) call — nothing is
  duplicated, and the legacy binaries remain first-class.

### The discovery brochure — a separate lifecycle stage from the fleet catalog

S114 (TERM #251, DISC-01) adds a second, DELIBERATELY DISTINCT registry: the
**brochure** (`model_discovery_candidate`, [`src/intake/discovery/`](../../src/intake/discovery/)).
Where `model_fleet_catalog` above answers "what has been TESTED, and how did it
score?" for models already in the fleet, the brochure answers an earlier
question: "what's a CANDIDATE — newly available on HuggingFace, not yet
acquired or tested?" The two tables relate ONLY via a `model_name` join; the
brochure never gains fleet-catalog fields and vice versa. This is also
deliberately not "catalog": Chord's `src/catalog.rs` is a third, unrelated
thing (the MCP *tool* catalog) — see `src/intake/discovery/schema.rs`'s module
doc for the full naming rationale.

A brochure row moves through an explicit lifecycle (`CandidateStatus`):
`discovered` → `fetching` → `cold_stored` → `marked_for_fleet` → `swept` →
`evicted` (an evicted row may re-enter at `discovered` only, if the model
reappears in a later HF listing); `discovered`/`fetching` can also terminate
at `rejected` (failed the VRAM/gfx1151 fit check, never fetched). Each row also
carries a `category` tag (`tool_router` | `writer_slm` | `assistant` | `coder`
| `embedding` | `visual` | `voice` — which fleet category the candidate
targets) and a `retained_profile` JSONB blob that survives eviction, so a
pruned candidate's discovery profile is never lost even after its cold-storage
copy is reclaimed.

DISC-01 is storage-only (the table, the migration, and the
`FleetCategory`/`CandidateStatus` Rust types with `as_str()`/`from_str()`
round-tripping and a clean parse error for any unrecognized string — never a
silent default). Later items build on this schema without changing it: DISC-02
adds the read-only `model_discovery_brochure` MCP tool (mirroring
`model_fleet_catalog`'s `json`/`markdown` filter/render pattern); DISC-03 adds
the one write API (`upsert_candidate`/`transition_status`/`record_eviction`)
every other discovery item uses to mutate rows.

DISC-02 (TERM #252) adds that read-only tool: **`model_discovery_brochure`**
([`src/intake/discovery/tool.rs`](../../src/intake/discovery/tool.rs)),
registered on the core registry alongside `model_fleet_catalog`. It reads the
persisted brochure via the ONE shared Postgres pool
(`crate::intake::storage::get_pool`, reused rather than a second pool) and
applies a pure filter/render layer, unit-testable without a live DB — the
same split `model_fleet_catalog` uses. Filters (all optional): `category`
(`tool_router`|`writer_slm`|`assistant`|`coder`|`embedding`|`visual`|`voice`),
`status` (`discovered`|`fetching`|`cold_stored`|`marked_for_fleet`|`swept`|
`evicted`|`rejected`), `min_discovery_score`, `gfx1151_class`
(`confirmed`|`experimental`|`unknown`), and `model` (exact `model_name`
match — unknown value returns an empty result plus a note, never an error);
an invalid `category`/`status`/`format` enum value is a clean
`ToolError::InvalidArgument`. `format` is `json` (default, structured) or
`markdown` (a compact table: model | category | status | gfx1151_class |
vram_gb | discovery_score | last_seen_at). An `Evicted` candidate (its
`retained_profile` populated) is never hidden by default — only an explicit
`status` filter excluding it removes it. The tool's own description states
the brochure/catalog distinction explicitly so an agent's tool-selection
reasoning picks the right one: query `model_discovery_brochure` first to
discover new models, query `model_fleet_catalog` for test coverage/scores on
a model already in the fleet.

### The unified MINT harness (two run kinds)

MINT runs two sweep **families** — a **coder** sweep (code-generation cases,
scored/stored in `code_profile_runs`) and a Lumina **assistant** sweep (the
seven behavioral dimensions, stored via `assistant::schema`). Both share this
one `src/intake/` tree and the one `lumina_intake` Postgres, and both are now
driven through a single orchestrator, **`MintHarness`**
([`src/intake/mod.rs`](../../src/intake/mod.rs)):

- **`RunKind::Coder` | `RunKind::Assistant`** selects the family. A given
  process runs exactly one kind; the two are independent, so running one kind
  never blocks the other.
- `MintHarness` owns the common run lifecycle — resolve config → confirm the
  shared intake DB is reachable via the **one** canonical resolver both
  families use (`config::intake_database_url()`, i.e. `INTAKE_DATABASE_URL`
  falling back to `DATABASE_URL`) → stamp a harness run-identity for log
  correlation → dispatch to the per-kind **sub-runner** (which implements the
  small shared `SweepRunner` trait). If the intake DB URL is unset the harness
  surfaces a clean per-kind *NotConfigured* message instead of crashing deeper
  in a sub-runner.
- The two standalone binaries (`intake_coder_sweep`, `intake_assistant_sweep`)
  are now **thin entrypoints** — each is a one-line
  `MintHarness::run(RunKind::…)`. All binary-specific orchestration (env-config
  resolution for coder, the end-of-run summary for assistant) moved into the
  sub-runners.
- This is a **structural** unification only: it does not change what either
  sweep measures — the coder cases and the assistant's seven dimensions run
  exactly as before, each under its existing sub-runner and driver.

See [`mint/`](mint/) for the full flagship manual: the sweep/case/gaps
lifecycle, the GPU-authority lock (`gpu_authority`), the permanent
jam-detect supervisor daemon, and the Chord `PullCoordinator` re-pull
delegation.

**ACQ-01 (Terminus TERM #244) — model acquisition is Chord-only, not a
presence check.** Both sweep families acquire every model they profile via
Chord's control-API pull endpoint (`chord_pull::acquire_via_chord`, the same
`PullCoordinator` delegation `mint fetch-model`/`breakfix` already use),
which promotes the model from this fleet's tiered/cold-storage archive — it
is **never** an internet fetch. This replaced two prior gaps: the coder
sweep's HFIX-05 pre-flight used to only check whether a model happened to
already be present (skipping it otherwise, even if the archive had it), and
the assistant sweep's `ShellAcquirer` used to shell out directly to
`ollama pull` and an HF-fetch binary for its `ollama_pull`/`hf_fetch`
nominations. A model Chord cannot acquire (unknown/missing from the archive,
insufficient host disk, unauthorized, or Chord unreachable/unconfigured) is a
clean skip — recorded as a terminal non-viable `code_profile_runs` row
(`failure_class` = `non_viable_unavailable` or `non_viable_resource`, the
same finalized-row mechanism MINT2-02 introduced for over-VRAM skips) rather
than silently vanishing from the data or being retried forever.

## Domains

### Code & Git — 8 tools · [domain index](code-git/README.md)

Source control, dev workspace access, agentic coding, code-graph analysis,
and documentation generation.

| Tool | Actions | What it does | Page |
| --- | --- | --- | --- |
| `gitea` | ~20 | Gitea source-control API — repos, files, branches, PRs (create/merge/close/diff), Cargo-registry publish/yank; every write runs the PII gate first. | [`code-git/gitea.md`](code-git/gitea.md) |
| `github` | ~9 | GitHub tool + the git-public mirror engine subtools (`git_public_mirror_status/_prepare/_approve/_push`) that drive the PII-swept public-mirror pipeline. | [`code-git/github.md`](code-git/github.md) |
| `forge` | ~4 | The provider-agnostic `git_private`/`git_public` dispatch tools (plus their `*_capabilities` introspection companions) — one endpoint vocabulary across 11 self-hosted/hosted forge providers, split by governance posture. | [`code-git/forge.md`](code-git/forge.md) |
| `dev` | ~6 | Path-jailed read/write/run access to a dev workstation over SSH — the workspace tools an agentic coding session uses. | [`code-git/dev.md`](code-git/dev.md) |
| `openhands` | ~3 | Drives the OpenHands agentic-coding runtime over its HTTP API (run task, list conversations, get status). | [`code-git/openhands.md`](code-git/openhands.md) |
| `cortex` | ~10 | Code-graph / blast-radius / risk-scoring system — architecture, dependency, and review-flow analysis over a repo. | [`code-git/cortex.md`](code-git/cortex.md) |
| `scribe` | ~5 | Standing documentation agent — generates READMEs, wikis, and other knowledge-infrastructure artifacts from a repo. | [`code-git/scribe.md`](code-git/scribe.md) |
| `docgen` | ~5 | **S95.** The sovereign, in-house documentation engine (replacing Mintlify): per-project doc-target config (readme/wiki/pdf/notion/obsidian/blog), PII-swept generation via Chord's SLM router, multi-format rendering, versioning, and `docgen_run` — the post-feat build-skill trigger (DOCGEN-08) that runs the whole flow and returns versioned artifacts for the harness to place. | [`code-git/docgen.md`](code-git/docgen.md) |

### Project & Planning — 7 tools

Work tracking, task/dev-loop queues, inter-agent messaging, and scheduled
reminders.

| Tool | Actions | What it does | Page |
| --- | --- | --- | --- |
| `plane` | ~43 | Full Plane CE work-management surface over its REST API — issues, modules, multi-identity (`PLANE_PAT_<NAME>`) CRUD, prefix registry (incl. `plane_prefix_promote`, the durable baseline PR path). The largest single tool in the hub. | [`project-planning/plane/README.md`](project-planning/plane/README.md) |
| `axon` | ~4 | Postgres-backed work-order / task queue (submit, status, list, cancel). | [`project-planning/axon.md`](project-planning/axon.md) |
| `vector` | ~11 | Autonomous dev-loop agent control over a Postgres-backed queue (submit, status, queue depth, halt). | [`project-planning/vector.md`](project-planning/vector.md) |
| `nexus` | ~5 | Postgres-backed inter-agent inbox (send, check, read, ack, history). | [`project-planning/nexus.md`](project-planning/nexus.md) |
| `reminder` | ~4 | Postgres-backed one-shot scheduled alerts (set, list, cancel, poll). | [`project-planning/reminder.md`](project-planning/reminder.md) |
| `routines` | ~7 | Named, cron-like scheduler routines owned by an external scheduler service. | [`project-planning/routines.md`](project-planning/routines.md) |
| `skills` | ~3 | Filesystem CRUD over `active/`/`proposed/` skill directories (create, list, read). | [`project-planning/skills.md`](project-planning/skills.md) |

### Infra & Ops — 14 tools

Fleet health, automation, secrets, networking, and admin surfaces.

| Tool | Actions | What it does | Page |
| --- | --- | --- | --- |
| `ansible` | ~4 | Gated Ansible playbook execution — run, list playbooks, last-run status, view run log. | [`infra-ops/ansible.md`](infra-ops/ansible.md) |
| `dura` | ~7 | Sysadmin/health-check tools (constellation health, service checks, smoke tests) — a hardened rewrite of a shell-heavy legacy tool. | [`infra-ops/dura.md`](infra-ops/dura.md) |
| `network` | ~5 | Network diagnostics — ping, port check, DNS lookup, service reachability. | [`infra-ops/network.md`](infra-ops/network.md) |
| `<container-mgr>` | ~8 | Read-only Docker container management queries via the <container-mgr> API. | [`infra-ops/<container-mgr>.md`](infra-ops/<container-mgr>.md) |
| `prometheus` | ~7 | Read-only PromQL queries, alerts, and targets against a LAN Prometheus server. | [`infra-ops/prometheus.md`](infra-ops/prometheus.md) |
| `<secret-manager>` | ~5 | Read-only secret queries against <secret-manager> — status/list/get, never a write path. | [`infra-ops/<secret-manager>.md`](infra-ops/<secret-manager>.md) |
| `approval` | ~2 | The per-occurrence human-approval gate shared by every guarded tool (OpenHands, <secret-manager> writes, the mirror engine) — grant/deny. | [`infra-ops/approval.md`](infra-ops/approval.md) |
| `sysversion` | 1 | `system_version` — a single never-fail tool reporting the version and reachability of every constellation component. | [`infra-ops/sysversion.md`](infra-ops/sysversion.md) |
| `synapse` | ~3 | Watches for and manages proactive-message triggers on the fleet host (status, trigger, mute). | [`infra-ops/synapse.md`](infra-ops/synapse.md) |
| `vigil` | ~2 | Morning/afternoon fleet-host briefing generation and reporting. | [`infra-ops/vigil.md`](infra-ops/vigil.md) |
| `sentinel` | ~3 | Triggers operational checks and logging on the fleet host. | [`infra-ops/sentinel.md`](infra-ops/sentinel.md) |
| `soma` | ~10 | The Lumina Constellation admin panel/API — status, modules, cost summary, backup status, validation runs, skill approval, agent rename. | [`infra-ops/soma.md`](infra-ops/soma.md) |
| `gateway` | ~2 | Surfaces the Lumina API Gateway / dashboard (`dashboard_refresh` and related). | [`infra-ops/dashboard.md`](infra-ops/dashboard.md) |
| `sundry` | ~6 | Small one-off utility tools that don't warrant their own module: `health`, `echo`, `utc_now`, `constellation_version`, `vector_onboard`, `searxng_search`. | [`infra-ops/sundry.md`](infra-ops/sundry.md) |

### Models & Review — 8 tools

Model inference plumbing, local/multi-provider code review, and model
selection/profiling (MINT's tool-facing side).

| Tool | Actions | What it does | Page |
| --- | --- | --- | --- |
| `intake` | 6 | The MINT model-intake profiling framework's MCP-facing tools (`model_intake`, `model_intake_status`, `model_intake_compare`, `model_intake_fleet`, the read-only `model_fleet_catalog` coverage registry, and the read-only `model_discovery_brochure` candidate registry, DISC-02) — see [MINT flagship](#mint-flagship) above. | [`mint/`](mint/) |
| `dgem` | ~4 | Drives a persistent DiffusionGemma (`llama-diffusion-daemon`) HTTP daemon for near-zero-cost local code review and generation. | [`models-review/dgem.md`](models-review/dgem.md) |
| `review` | 1 | `review_run` — dispatches a review prompt to 1–5 providers concurrently, in one of several output structures, for multi-provider/multi-structure code review. | [`models-review/review.md`](models-review/review.md) |
| `wizard` | ~3 | Deep-reasoning "council" consultation routed through the Chord proxy (`CHORD_PROXY_URL`). | [`models-review/wizard.md`](models-review/wizard.md) |
| `model_advisor` | ~3 | Recommends model fleets from available VRAM/unified memory and use case; checks whether a specific model+quant fits a target. | [`models-review/model_advisor.md`](models-review/model_advisor.md) |
| `litellm` | ~6 | Read-only status and model queries against the LiteLLM proxy. | [`models-review/litellm.md`](models-review/litellm.md) |
| `tools` | ~3 | A small grouping of additional tool modules that live under `src/tools/` rather than the crate root. | [`models-review/serving.md`](models-review/serving.md) |

### Personal & Life — 17 tools

Finance, health, travel, home, media, and general life-admin integrations —
the bulk of the `terminus_personal` registry.

| Tool | Actions | What it does | Page |
| --- | --- | --- | --- |
| `meridian` | ~5 | Simulated paper-trading crypto portfolio sandbox (portfolio, market data, analysis, report, reset). | [`personal-life/meridian.md`](personal-life/meridian.md) |
| `odyssey` | ~8 | Trip planning — bucket list, loyalty cards, trip log, deals, research, optimize. | [`personal-life/odyssey.md`](personal-life/odyssey.md) |
| `ledger` | ~8 | Finance tracking via the Actual Budget HTTP API — accounts, transactions, budget status, categories. | [`personal-life/ledger.md`](personal-life/ledger.md) |
| `relay` | ~8 | Vehicle/maintenance tracking via the LubeLogger REST API — vehicles, fuel log, service history, cost summary. | [`personal-life/relay.md`](personal-life/relay.md) |
| `myelin` | ~9 | LLM cost-tracking — status, daily/weekly/monthly rollups, runaway-spend check, burn-plan, by-model breakdown. | [`personal-life/myelin.md`](personal-life/myelin.md) |
| `vitals` | ~11 | Health tracking (weight, sleep, and other logs; summary/recent/today; program creation) via a REST API backend. | [`personal-life/vitals.md`](personal-life/vitals.md) |
| `hearth` | ~7 | Pantry/meal-planning tools via Grocy — what-can-I-make, pantry list, meal plan. | [`personal-life/hearth.md`](personal-life/hearth.md) |
| `<media-service>` | ~8 | Read-only media request queries against <media-service> (Plex/Jellyfin request management). | [`personal-life/<media-service>.md`](personal-life/<media-service>.md) |
| `media` | 10 (11 with taste-memory) | **S94, complete.** Sovereign media-stack orchestration domain (Radarr/Sonarr/Prowlarr/qtor/Plex/<media-service>/TMDb) — vault(env)-backed service clients, a config-status tool, the read/search surface (`media_search`, `media_status`), (MEDIA-03) `media_request` — tiered-confirmation add/request driving Radarr/Sonarr → the download client, (MEDIA-04) `media_organize`/`media_delete`/`media_cleanup` — non-destructive organize plus hard-typed-confirmation destructive delete/bulk cleanup, (MEDIA-05) `media_recommend`/`media_on_deck`/`media_recently_added` — stateless watch-history-driven recommendations + engagement surface, (MEDIA-06) a toggleable taste-memory personalization layer (`media_taste_feedback` when enabled), and (MEDIA-07) the Lumina conversational surface (intent routing + confirmation narration). | [`personal-life/media.md`](personal-life/media.md) |
| `commute` | ~8 | Traffic-aware routing (TomTom) and Bay Area public-transit planning (511.org). | [`personal-life/commute.md`](personal-life/commute.md) |
| `weather` | 1 | Current conditions and forecasts via OpenWeatherMap. | [`personal-life/weather.md`](personal-life/weather.md) |
| `news` | ~3 | Headlines, search, and topic feeds. | [`personal-life/news.md`](personal-life/news.md) |
| `crucible` | ~10 | Learning-tracker system — reading list, tracks, streak, dashboard, status log. | [`personal-life/crucible.md`](personal-life/crucible.md) |
| `council` | ~4 | The "Obsidian Circle" deep-reasoning council — convene, history, presets, status. | [`personal-life/council.md`](personal-life/council.md) |
| `lumina_ext` | ~6 | The remaining `lumina_*` tools not yet moved to a dedicated module (AICPB rankings, claw awesome-list/hub search/skill-detail, clawmart browse, weather, web fetch). | [`personal-life/lumina_ext.md`](personal-life/lumina_ext.md) |
| `seer` | ~3 | Research-backend integration — query, recent, status. | [`personal-life/seer.md`](personal-life/seer.md) |
| `google` | ~9 | Calendar (CalDAV) and email (IMAP read / SMTP send) integration. | [`personal-life/google.md`](personal-life/google.md) |

---

Every module above registers through either `register_all` (the CORE
registry, served by `terminus-primary`/Chord) or `register_personal` (the
PERSONAL registry, served by `terminus_personal`) — some register into both.
See [`../architecture/federation.md`](../architecture/federation.md) for
exactly which registry serves which module and how `terminus-primary`
aggregates them into one client-visible catalog.

[← docs index](../README.md)
