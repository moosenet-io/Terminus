# Tool index

[← docs index](../README.md)

Terminus provides ~51 **tools**, one per integrated service (`src/registry.rs`'s
`register_all` / `register_personal` — see
[`architecture/federation.md`](../architecture/federation.md) for which
registry serves which tool). Each tool exposes a set of **actions** — the
individually-named MCP callables an MCP client sees in `tools/list` (e.g. the
GitHub tool's `github_create_repo`, `github_list_repos`, …). Those actions vary
with the backing service and change over time, so counts here are approximate;
across all tools there are ~300 individual MCP actions in total. This page
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

- The **`intake`** tool module (4 MCP tools: `model_intake`,
  `model_intake_status`, `model_intake_compare`, `model_intake_fleet`) —
  callable from any MCP client.
- The **`mint`** CLI binary ([`src/bin/mint.rs`](../../src/bin/mint.rs)) — a
  clap-derived subcommand tree (`mint sweep coder`, `mint sweep assistant`,
  `mint case`, `mint gaps`, `mint gpu status/acquire/release`, `mint
  supervisor run/install/uninstall`, `mint fetch-model`) that is a more
  discoverable operator front door over the *same* run functions the legacy
  standalone binaries (`intake_coder_sweep`, `intake_coder_case`,
  `intake_coder_gaps`, `intake_assistant_sweep`) call — nothing is
  duplicated, and the legacy binaries remain first-class.

See [`mint/`](mint/) for the full flagship manual: the sweep/case/gaps
lifecycle, the GPU-authority lock (`gpu_authority`), the permanent
jam-detect supervisor daemon, and the Chord `PullCoordinator` re-pull
delegation.

## Domains

### Code & Git — 7 tools · [domain index](code-git/README.md)

Source control, dev workspace access, agentic coding, code-graph analysis,
and documentation generation.

| Tool | Actions | What it does | Page |
| --- | --- | --- | --- |
| `gitea` | ~16 | Gitea source-control API — repos, files, PRs, merges, Cargo-registry publish/yank; every write runs the PII gate first. | [`code-git/gitea.md`](code-git/gitea.md) |
| `github` | ~9 | GitHub tool + the git-public mirror engine subtools (`git_public_mirror_status/_prepare/_approve/_push`) that drive the PII-swept public-mirror pipeline. | [`code-git/github.md`](code-git/github.md) |
| `forge` | ~4 | The provider-agnostic `git_private`/`git_public` dispatch tools (plus their `*_capabilities` introspection companions) — one endpoint vocabulary across 11 self-hosted/hosted forge providers, split by governance posture. | [`code-git/forge.md`](code-git/forge.md) |
| `dev` | ~6 | Path-jailed read/write/run access to a dev workstation over SSH — the workspace tools an agentic coding session uses. | [`code-git/dev.md`](code-git/dev.md) |
| `openhands` | ~3 | Drives the OpenHands agentic-coding runtime over its HTTP API (run task, list conversations, get status). | [`code-git/openhands.md`](code-git/openhands.md) |
| `cortex` | ~10 | Code-graph / blast-radius / risk-scoring system — architecture, dependency, and review-flow analysis over a repo. | [`code-git/cortex.md`](code-git/cortex.md) |
| `scribe` | ~5 | Standing documentation agent — generates READMEs, wikis, and other knowledge-infrastructure artifacts from a repo. | [`code-git/scribe.md`](code-git/scribe.md) |

### Project & Planning — 7 tools

Work tracking, task/dev-loop queues, inter-agent messaging, and scheduled
reminders.

| Tool | Actions | What it does | Page |
| --- | --- | --- | --- |
| `plane` | ~37 | Full Plane CE work-management surface over its REST API — issues, modules, multi-identity (`PLANE_PAT_<NAME>`) CRUD, prefix registry. The largest single tool in the hub. | [`project-planning/plane/README.md`](project-planning/plane/README.md) |
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

### Models & Review — 7 tools

Model inference plumbing, local/multi-provider code review, and model
selection/profiling (MINT's tool-facing side).

| Tool | Actions | What it does | Page |
| --- | --- | --- | --- |
| `intake` | 4 | The MINT model-intake profiling framework's MCP-facing tools (`model_intake`, `model_intake_status`, `model_intake_compare`, `model_intake_fleet`) — see [MINT flagship](#mint-flagship) above. | [`mint/`](mint/) |
| `dgem` | ~4 | Drives a persistent DiffusionGemma (`llama-diffusion-daemon`) HTTP daemon for near-zero-cost local code review and generation. | [`models-review/dgem.md`](models-review/dgem.md) |
| `review` | 1 | `review_run` — dispatches a review prompt to 1–5 providers concurrently, in one of several output structures, for multi-provider/multi-structure code review. | [`models-review/review.md`](models-review/review.md) |
| `wizard` | ~3 | Deep-reasoning "council" consultation routed through the Chord proxy (`CHORD_PROXY_URL`). | [`models-review/wizard.md`](models-review/wizard.md) |
| `model_advisor` | ~3 | Recommends model fleets from available VRAM/unified memory and use case; checks whether a specific model+quant fits a target. | [`models-review/model_advisor.md`](models-review/model_advisor.md) |
| `litellm` | ~6 | Read-only status and model queries against the LiteLLM proxy. | [`models-review/litellm.md`](models-review/litellm.md) |
| `tools` | ~3 | A small grouping of additional tool modules that live under `src/tools/` rather than the crate root. | [`models-review/serving.md`](models-review/serving.md) |

### Personal & Life — 16 tools

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
