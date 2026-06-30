<p align="center"><img src="assets/banner.svg" alt="Terminus" width="640"></p>

<p align="center"><img src="assets/badges.svg" alt="badges"></p>

# Terminus

A Rust MCP tool hub — one authenticated gateway for agent tooling.

## Overview

Terminus is the Model Context Protocol (MCP) tool hub for the Lumina
constellation: a single Rust registry through which agents reach every external
system — git forges, project trackers, infrastructure, finance, calendars,
secrets, model inference, and more. Rather than each agent embedding its own
clients and credentials, agents speak MCP to one governed surface, and Terminus
dispatches each call to a typed, sandboxed tool implementation.

Originally an in-tree crate of the Lumina constellation, Terminus is now
extracted as a standalone, versioned crate/service (`terminus-rs`) so it can be
built, tested, and deployed on its own.

Every tool implements one small trait (`RustTool`): a stable name, a JSON Schema
for its arguments, a description, and an async `execute`. Implementations use
typed HTTP clients (`reqwest`) and parameterized SQL (`sqlx`) for all external
I/O — never shell-outs — and are registered into a central `ToolRegistry` that
handles dispatch, duplicate detection, and catalog listing.

## Architecture

<img src="assets/architecture.svg" alt="Terminus architecture" width="100%">

MCP clients (the Lumina and Harmony agents) connect over stdio or HTTP
transports to the **Terminus core MCP server**. The core is the tool registry:
it handles dispatch, JSON-Schema validation, and governance. Governance is
mandatory and layered — a path-jailed filesystem, vault-only secret access (no
raw environment reads for secrets), a PII gate, and a sanitized audit log. Tools
are read-only by default; write scopes are explicit.

Behind the registry sit the domain tool modules — one authenticated surface for
the whole stack. Each module owns its own typed client and credentials:

- **Infrastructure** — duty/health checks (`dura`), Ansible, Portainer,
  Prometheus.
- **Code & Git** — Gitea, GitHub, dev workspace tools, OpenHands.
- **Search & Memory** — Seer (research) and knowledge-digest queries.
- **Review (local)** — DiffusionGemma (`dgem`) local code review at near-zero
  cost.
- **Models & Inference** — LiteLLM, system-version reporting, and the model
  intake / profiling suite.
- **Calendar & Comms** — Google calendar/email, reminders.
- **Secrets & Network** — Infisical (vault-backed), network diagnostics.
- **Media & Project** — Jellyseerr, Plane, weather, and others.

Alongside the external-system tools, Terminus carries the **intake / inference
profiling** primitives: a framework that loads a fleet model, runs graduated
context, code, and agent suites against it, and stores a derived operational
profile (safe/absolute context ceilings, throughput curve, recommended
timeouts, degradation point) in Postgres for later comparison.

As of **v1.1** this extends into a **serving-profile** dimension
([`src/intake/serving`](src/intake/serving)): for each (model × serving backend)
it records the chosen launch runtime and its env (gfx override, CPU lib, mmap /
flash-attn flags), measured throughput / VRAM-or-RAM peak / cold-load time, a
`keep_warm` hint for large slow-loading MoEs, and typed `exclusion_reason` /
`recheck_trigger` enums explaining why a faster runtime was skipped and whether
a llama.cpp build bump should prompt a re-probe. The probe layer is trait-driven
(launcher + VRAM-release gate) so the suite runs on CI with no real GPU. This
profile is the contract Chord consumes to place and launch models.

### Tool-selection subagent (context-churn reduction)

A constellation this size carries ~100 tools across its modules. Dumping every
schema into an orchestrator's prompt on every turn is expensive and actively
harmful — more tools means more tokens, slower turns, and more chances for the
model to pick the wrong one. Terminus is built to be narrowed instead of
flooded: the `ToolRegistry` ([`src/registry.rs`](src/registry.rs)) keeps each
tool's name, description, and JSON Schema as a first-class catalog entry rather
than baking them into one giant prompt, so a caller can ask for *only the
relevant few* per request.

The selection itself is a deliberately cheap, model-free keyword matcher (no
extra inference call to decide which tools to expose). Chord — the inference
front door that fronts Terminus — exposes a `discover(query, max)` over the
merged catalog: both the user's query and each tool's `name`+`description` are
tokenized into lowercase words, stopwords are dropped (so `my` no longer matches
**MY**elin and `in` no longer matches everything), matching is whole-word, and a
hit in the tool *name* outscores a hit in the description. Chord's agentic loop
uses exactly this to assemble a small per-turn toolset (~14 discovered tools
plus a handful of always-on essentials) when the caller passes no explicit list.
The payoff is structural: the orchestrator reasons over a handful of relevant
tools per turn instead of the whole hub, which is cheaper, faster, and less
error-prone — and because the scoring is plain tokens, it is deterministic and
debuggable rather than another opaque LLM judgement.

### Governance

Guarded tools (e.g. `openhands_run_task`, Infisical secret access) pass through
a per-occurrence human-approval gate before they execute. On first call the gate
creates a pending request with a short single-use code and refuses to run; an
operator approves out of band, and only then does the stored call re-dispatch
and run exactly once. The model can never approve its own request.

## Tools

Tools are registered by `register_all` in
[`src/registry.rs`](src/registry.rs). The current domain modules and a sample of
their tools:

| Module | Purpose | Example tools |
| --- | --- | --- |
| `approval` | Guarded-tool approval gate (internal) | `approval_grant`, `approval_deny` |
| `intake` | Model profiling / inference primitives | `model_intake`, `model_intake_status`, `model_intake_compare`, `model_intake_fleet` |
| `serving` | Serving-profile inspect / operate (v1.1) | `serving_profile_get`, `serving_residency_status`, `serving_profile_refresh` |
| `dev` | Path-jailed dev workspace | `dev_read_file`, `dev_write_file`, `dev_run_command`, `dev_list_workspaces` |
| `openhands` | Agentic coding runs (guarded) | `openhands_run_task`, `openhands_list_conversations`, `openhands_get_status` |
| `gitea` | Gitea git forge | `gitea_create_repo`, `gitea_read_file`, `gitea_create_pr`, `gitea_merge_pr` |
| `github` | GitHub | `github_create_repo`, `github_list_repos`, `github_push_repo` |
| `plane` | Plane work management | `plane_create_work_item`, `plane_list_issues_by_state`, `plane_update_work_item` |
| `nexus` | Inter-agent inbox | `nexus_send`, `nexus_check`, `nexus_read`, `nexus_ack`, `nexus_history` |
| `axon` | Work-queue agent control | `axon_submit`, `axon_status`, `axon_list`, `axon_cancel` |
| `vector` | Dev-loop agent control | `vector_submit`, `vector_status`, `vector_queue_depth`, `vector_halt` |
| `seer` | Research queries | `seer_query`, `seer_recent`, `seer_status` |
| `wizard` | Deep-reasoning council | `wizard_consult`, `wizard_history`, `wizard_status` |
| `dgem` | DiffusionGemma local review | `dgem_review`, `dgem_generate`, `dgem_batch`, `dgem_status` |
| `litellm` | LiteLLM proxy management | `litellm_list_models`, `litellm_model_status`, `litellm_request_log` |
| `sysversion` | System/version reporting | `system_version` |
| `ansible` | Gated Ansible execution | `ansible_run_playbook`, `ansible_list_playbooks`, `ansible_last_run_status` |
| `portainer` | Portainer containers | `portainer_list_containers`, `portainer_container_logs`, `portainer_status` |
| `prometheus` | Prometheus queries | `prometheus_query`, `prometheus_alerts`, `prometheus_targets` |
| `dura` | Infra/constellation health | `dura_constellation_health`, `dura_service_check`, `dura_smoke_test` |
| `network` | Network diagnostics | `net_ping`, `net_port_check`, `net_dns_lookup`, `net_check_services` |
| `infisical` | Vault-backed secrets (read-only) | `infisical_get_secret`, `infisical_list_secrets`, `infisical_status` |
| `google` | Calendar & email | `google_calendar_today`, `google_email_send`, `google_email_summary` |
| `reminder` | Reminders | `reminder_set`, `reminder_list`, `reminder_cancel`, `reminder_poll` |
| `commute` | Traffic & transit | `commute_estimate`, `route_traffic`, `transit_plan` |
| `weather` | Weather | weather lookups |
| `news` | News API | `news_headlines`, `news_search`, `news_topic` |
| `jellyseerr` | Media requests | `jellyseerr_search`, `jellyseerr_requests`, `jellyseerr_status` |
| `hearth` | Pantry / meal planning | `hearth_what_can_i_make`, `hearth_pantry_list`, `hearth_meal_plan` |
| `ledger` | Personal finance ledger | `ledger_balance`, `ledger_recent`, `ledger_budget_summary` |
| `relay` | Vehicle / maintenance log | `relay_vehicles`, `relay_next_due`, `relay_cost_summary` |
| `myelin` | LLM cost reporting | `myelin_today`, `myelin_monthly`, `myelin_cap_check` |
| `vitals` | Health logging | `vitals_log_weight`, `vitals_log_sleep`, `vitals_summary` |
| `gateway` | Dashboard refresh | `dashboard_refresh` |

See [`src/lib.rs`](src/lib.rs) for the full module list and
[`src/registry.rs`](src/registry.rs) for the registration order.

## License

MIT
