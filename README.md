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

- **Infrastructure** — duty/health checks (`dura`), Ansible, <container-mgr>,
  Prometheus.
- **Code & Git** — Gitea, GitHub, dev workspace tools, OpenHands.
- **Search & Memory** — Seer (research) and knowledge-digest queries.
- **Review (local)** — DiffusionGemma (`dgem`) local code review at near-zero
  cost.
- **Models & Inference** — LiteLLM, system-version reporting, and the model
  intake / profiling suite.
- **Calendar & Comms** — Google calendar/email, reminders.
- **Secrets & Network** — <secret-manager> (vault-backed), network diagnostics.
- **Media & Project** — <media-service>, Plane, weather, and others.

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

Guarded tools (e.g. `openhands_run_task`, <secret-manager> secret access) pass through
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
| `gitea` | Gitea git forge | `gitea_create_repo`, `gitea_read_file`, `gitea_create_pr`, `gitea_merge_pr`, `gitea_cargo_publish` |
| `github` | GitHub | `github_create_repo`, `github_list_repos`, `github_push_repo`, `github_pii_scan` |
| `plane` | Plane work management | `plane_create_work_item`, `plane_list_issues_by_state`, `plane_update_work_item`, `plane_create_module`, `plane_update_module`, `plane_delete_module`, `plane_add_issue_to_module`, `plane_remove_issue_from_module`, `plane_list_identities`, `plane_whoami`, `plane_prefix_check`, `plane_prefix_register` |
| `nexus` | Inter-agent inbox | `nexus_send`, `nexus_check`, `nexus_read`, `nexus_ack`, `nexus_history` |
| `axon` | Work-queue agent control | `axon_submit`, `axon_status`, `axon_list`, `axon_cancel` |
| `vector` | Dev-loop agent control | `vector_submit`, `vector_status`, `vector_queue_depth`, `vector_halt` |
| `seer` | Research queries | `seer_query`, `seer_recent`, `seer_status` |
| `wizard` | Deep-reasoning council | `wizard_consult`, `wizard_history`, `wizard_status` |
| `dgem` | DiffusionGemma local review | `dgem_review`, `dgem_generate`, `dgem_batch`, `dgem_status` |
| `litellm` | LiteLLM proxy management | `litellm_list_models`, `litellm_model_status`, `litellm_request_log` |
| `sysversion` | System/version reporting | `system_version` |
| `ansible` | Gated Ansible execution | `ansible_run_playbook`, `ansible_list_playbooks`, `ansible_last_run_status` |
| `<container-mgr>` | <container-mgr> containers | `portainer_list_containers`, `portainer_container_logs`, `portainer_status` |
| `prometheus` | Prometheus queries | `prometheus_query`, `prometheus_alerts`, `prometheus_targets` |
| `dura` | Infra/constellation health | `dura_constellation_health`, `dura_service_check`, `dura_smoke_test` |
| `network` | Network diagnostics | `net_ping`, `net_port_check`, `net_dns_lookup`, `net_check_services` |
| `<secret-manager>` | Vault-backed secrets (read-only) | `infisical_get_secret`, `infisical_list_secrets`, `infisical_status` |
| `google` | Calendar & email | `google_calendar_today`, `google_email_send`, `google_email_summary` |
| `reminder` | Reminders | `reminder_set`, `reminder_list`, `reminder_cancel`, `reminder_poll` |
| `commute` | Traffic & transit | `commute_estimate`, `route_traffic`, `transit_plan` |
| `weather` | Weather | weather lookups |
| `news` | News API | `news_headlines`, `news_search`, `news_topic` |
| `<media-service>` | Media requests | `jellyseerr_search`, `jellyseerr_requests`, `jellyseerr_status` |
| `hearth` | Pantry / meal planning | `hearth_what_can_i_make`, `hearth_pantry_list`, `hearth_meal_plan` |
| `ledger` | Personal finance ledger | `ledger_balance`, `ledger_recent`, `ledger_budget_summary` |
| `relay` | Vehicle / maintenance log | `relay_vehicles`, `relay_next_due`, `relay_cost_summary` |
| `myelin` | LLM cost reporting | `myelin_today`, `myelin_monthly`, `myelin_cap_check` |
| `vitals` | Health logging | `vitals_log_weight`, `vitals_log_sleep`, `vitals_summary` |
| `gateway` | Dashboard refresh | `dashboard_refresh` |

See [`src/lib.rs`](src/lib.rs) for the full module list and
[`src/registry.rs`](src/registry.rs) for the registration order.

## Cargo registry publishing (`gitea_cargo_publish`)

Publishing a Rust crate to the Gitea Cargo registry goes **through Terminus**,
not through a `cargo publish` token on a build or dev host. `cargo publish` is,
on the wire, an authenticated HTTP `PUT` of a packaged `.crate` to the registry;
`gitea_cargo_publish` recreates exactly that request and signs it with
**Terminus's own `GITEA_TOKEN`**, so the publish credential lives in one place
(the runtime secret store that materializes `GITEA_TOKEN`) and never has to be
copied onto the dev box or spread across containers. There is a **single
publisher identity** — this is deliberately not a multi-user path.

Workflow:

1. On the dev box, package token-lessly: `cargo package -p <crate>` produces
   `target/package/<name>-<version>.crate`. Also assemble the crate's Cargo
   **publish-wire** metadata for the required `metadata` argument — the exact
   object cargo PUTs to a registry (`deps` with `version_req`, `features`, ...),
   **not** `cargo metadata` output (a different schema). The most reliable source
   is the normalized `Cargo.toml` cargo embeds in the `.crate`, mapped to the
   publish schema.
2. Relay that `.crate` to the host running Terminus.
3. Call `gitea_cargo_publish`:

   | Input | Required | Meaning |
   | --- | --- | --- |
   | `crate_path` | yes | Path to the local `.crate` file on the Terminus host. |
   | `name` | yes | Crate name. |
   | `version` | yes | Crate version. |
   | `owner` | no | Registry owner/org (defaults to `GITEA_OWNER`, normally `moosenet`). |
   | `metadata` | yes | Cargo **publish-wire** metadata object (the schema cargo PUTs: `deps` with `version_req`, `features`, license, repository, ...) — not `cargo metadata` output. `name`/`vers` are forced to the explicit arguments. Required because a name+version-only publish would write an incorrect registry index for any crate with dependencies. |

   It frames the standard Cargo publish body
   (`u32-LE(len)‖metadata_json‖u32-LE(len)‖crate_bytes`) and `PUT`s it to
   `{GITEA_URL}/api/packages/{owner}/cargo/api/v1/crates/new` with
   `Authorization: token <GITEA_TOKEN>` (the same PAT scheme every other Gitea
   call uses). On success it returns the published
   name/version and the registry package URL; a `403` is surfaced explicitly as
   a likely missing `write:package` token scope.

> The publishing `GITEA_TOKEN` must carry the **`write:package`** scope. Without
> it the registry returns `403` and the tool reports the missing scope.

**Input safety.** `crate_path` is validated before any bytes are read: it must
be an existing **regular** `.crate` file (directories and devices such as
`/dev/zero` are refused) no larger than `CARGO_PUBLISH_MAX_CRATE_BYTES` (default
64 MiB). Set `CARGO_PUBLISH_ARTIFACT_DIR` to confine reads to a dedicated
staging directory — the canonicalized crate path must then live inside it — so
the tool cannot be turned into an arbitrary host-file read.

| Env var | Default | Purpose |
| --- | --- | --- |
| `CARGO_PUBLISH_MAX_CRATE_BYTES` | `67108864` (64 MiB) | Reject artifacts larger than this before reading. |
| `CARGO_PUBLISH_ARTIFACT_DIR` | unset | When set, `crate_path` must resolve inside this directory (path jail). |

## PII gate (Rust, authoritative)

PII/secret detection is a single Rust engine in
[`src/github/pii.rs`](src/github/pii.rs). It serves three surfaces from one rule
set, so their coverage can never drift apart:

- **Runtime write gate** — `pii_gate(content)` hard-blocks every GitHub write
  tool before any network request fires. No flag or env var disables it.
- **Tree sweep** — `PiiRuleSet::scan_tree(path)` walks a directory and returns
  structured `TreeViolation { file, line, pattern_kind, context }`. `context` is
  always a redacted snippet — the full matched secret is never stored or logged.
- **`github_pii_scan` tool** — a read-only diagnostic (core registry) that scans
  a `content` string or a `tree_path` and returns the same structured findings.

The rule set combines generic built-in patterns (RFC-1918 ranges, API-key
prefixes, JWTs, PEM keys, cloud keys, emails, phones) with **config-driven**
repo-specific terms — no repo's hostnames or service names are hardcoded in the
engine. Config is an optional repo-root `pii-gate.toml` (or a path in
`TERMINUS_PII_CONFIG`):

```toml
extra_terms        = ["my-internal-host", "my-service"]   # word-boundary, case-insensitive
extra_patterns     = ['''AKIA[A-Z0-9]{16}''']              # raw regexes; invalid ones are skipped
allowed_emails     = ["@example.com"]                       # allow-listed author/placeholder emails
excluded_files     = ["generated.rs"]
excluded_extensions = ["snap"]
excluded_dirs      = ["vendor"]
```

The `// pii-test-fixture` marker is the only exemption: it clears a **single
tagged line** (never a blanket bypass) so deliberate PII-shaped test literals
pass, exactly as the crate's own `no_pii_in_own_source_tree` self-check does.

### Pre-push hook binary

[`src/bin/pii_gate.rs`](src/bin/pii_gate.rs) is the Rust pre-push/pre-commit
hook that replaces the legacy Python `.githooks/pii_gate.py`. It exits `0` when
clean and `1` on any violation (hard block).

```sh
cargo build --release --bin pii_gate

# install as the git pre-push hook (replacing the Python gate)
ln -sf ../../target/release/pii_gate .git/hooks/pre-push

# modes
pii_gate                 # git pre-push: scans the pushed commit range (stdin protocol)
pii_gate --staged        # git pre-commit: scans staged files
pii_gate --tree [PATH]   # full-tree sweep (defaults to repo root) — used by the mirror engine
pii_gate --json          # machine-readable JSON report
```

## Plane identities (`PLANE_PAT_<NAME>` convention)

The Plane tool module supports multiple **named identities** so a call can act
as whichever agent should own the resulting work item, rather than always using
one shared token. Identities are configured purely through this process's own
environment — the tool never reads another process's files.

### Configuration

| Variable | Purpose |
| --- | --- |
| `PLANE_API_URL` | Base URL of the Plane instance (required at call time). |
| `PLANE_API_KEY` | The **default** (unsuffixed) token, used when no named identity is selected. |
| `PLANE_WORKSPACE` | Workspace slug. |
| `PLANE_IDENTITY_NAME` | Optional human name for the default `PLANE_API_KEY` token. |
| `PLANE_PAT_<NAME>` | A **named identity**. Each such variable registers the identity `<name>` (lowercased) with its own token — e.g. `PLANE_PAT_CLAUDE`, `PLANE_PAT_HARMONY`. |

Named identities are matched only by the `PLANE_PAT_` prefix; the unsuffixed
`PLANE_API_KEY` default and unrelated `PLANE_*` variables are never scanned as
identities, and a set-but-empty `PLANE_PAT_<NAME>` is treated as absent. These
values are provisioned into the running process by each deployment's own
secret-materialization step (the vault-backed secret store, surfaced as
`INFISICAL_*`-configured fetches at startup) — never hardcoded into a unit file
or committed anywhere.

### Acting as an identity: the `identity` argument

Every Plane **CRUD tool** (`plane_list_projects`, `plane_create_work_item`,
`plane_update_work_item`, `plane_close_work_item`, `plane_create_comment`, …)
accepts an **optional `identity` argument**. Set it to a configured
`PLANE_PAT_<NAME>` name (e.g. `"claude"`, `"harmony"`) and that single call is
authenticated as that identity's token — so the work item is created/updated
under the right actor. Omit it and the call acts as the **active default**
identity.

The active default is resolved once at startup:

- If `PLANE_IDENTITY_NAME` names a configured `PLANE_PAT_<NAME>` identity, the
  default token **is that identity's token** — e.g. `PLANE_IDENTITY_NAME=lumina`
  makes every no-`identity` call genuinely act as `PLANE_PAT_LUMINA`, not just
  display the label.
- Otherwise it falls back to the unsuffixed `PLANE_API_KEY`. A deployment that
  configures only `PLANE_API_KEY` (no named default) behaves exactly as before —
  full backward compatibility.

Selection is centralized: all CRUD tools route through the same
`PlaneClient::resolve_identity` → `for_identity` dispatch, so the rule lives in
one place. The `identity` argument is used only to pick the token — it is never
written into a request body and never logged, and switching identity never
crosses the per-token GET cache.

### Checking a token's health: `plane_whoami` with `verify`

`plane_whoami` reports the active identity and, given `identity`, whether that
name is configured. Pass **`verify: true`** to make a real authenticated read as
the selected identity (explicit `identity`, else the active default) and report
whether its token is currently **valid (200)** or **rejected (401/403 — likely
expired or revoked)**. This is the per-identity health check a future audit uses
to find expired PATs. It reports the auth outcome only — never a token value.

### Listing identities: `plane_list_identities`

`plane_list_identities` returns the names of every configured `PLANE_PAT_<NAME>`
identity (sorted, stable), the active default, and the prefix — **names only,
never token values**, matching `plane_whoami`'s safety posture. Call it to see
which identity you can act as before creating or assigning Plane work. With no
named identities configured it returns an empty list plus an explanatory note
(not an error).

### Which identity to use (assignment convention)

Create or transition a work item under the identity of whoever should **act on**
it, mapped from a spec item's `Agent:` field, rather than always using the
ingesting agent's own identity:

- `PLANE_PAT_CLAUDE` — this agent's own identity; use for all Claude-driven
  create/update/comment work unless explicitly assigning to another actor.
- `PLANE_PAT_HARMONY` — work intended for Harmony's own dispatch to pick up.
- `PLANE_PAT_MOOSE` — operator human-action items.
- `PLANE_PAT_GEMINI` / `PLANE_PAT_CODEX` — work intended for those agent types.
- `PLANE_PAT_LUMINA` — the assistant persona (the default identity).

Select the identity through the tool's own mechanism (`plane_list_identities` /
the client's `for_identity()` resolution) — **never** fetch a `PLANE_PAT_*`
value yourself to make a raw API call; that is a second, unsanctioned access
path. This convention is the same one carried normatively by the moosenet-spec
build pipeline (v3.8, "Plane access — ONE sanctioned path" / "Plane identity
convention"); this section is the Terminus-local, discoverable copy of it, not a
competing source of truth.

## Plane module management

Modules (sprint/epic groupings inside a project) have a full CRUD + membership
surface, all through the single sanctioned Plane tool path:

| Tool | Endpoint | Purpose |
| --- | --- | --- |
| `plane_list_modules` | `GET …/modules/` | List a project's modules |
| `plane_get_module` | `GET …/modules/{id}/` | One module's detail |
| `plane_create_module` | `POST …/modules/` | Create a module |
| `plane_update_module` | `PATCH …/modules/{id}/` | Rename / re-describe / re-status / re-date |
| `plane_delete_module` | `DELETE …/modules/{id}/` | Delete the module (its issues stay in the project) |
| `plane_list_module_issues` | `GET …/modules/{id}/module-issues/` | Issues currently in a module |
| `plane_add_issue_to_module` | `POST …/modules/{id}/module-issues/` | Add one (`issue_id`) or many (`issue_ids`) issues |
| `plane_remove_issue_from_module` | `DELETE …/modules/{id}/module-issues/{issue}/` | Remove an issue from a module (issue kept in project) |

Issue↔module membership is a **separate endpoint** in Plane CE, not an issue
field, so `plane_create_work_item` and `plane_update_work_item` each accept an
optional **`module_id`**: the issue is created/updated first, then linked to the
module via `module-issues` in the same call. On `plane_update_work_item`,
`module_id` may be supplied **alone** (no other field) as a pure "move this issue
into that module" operation — that skips the issue PATCH entirely and only links.

Every one of these tools honors the optional `identity` argument (the same
`PLANE_PAT_<NAME>` dispatch as the rest of the Plane CRUD surface), and
`plane_delete_module` follows the same posture as `plane_delete_work_item` — a
direct destructive call, guarded (if at all) by the registry's guarded-tool layer,
not re-implemented in the tool.

## Plane request pacing & caching (optional shared Redis)

The Plane tool paces its own outbound requests (a shared rate limiter) and caches
GET responses briefly (per active token + URL). By default both live purely
**in-process**. Point them at a Redis instance and they become **shared across
every terminus process** that talks to Plane — one GET cache and one coordinated
rate budget against Plane's API, instead of each process keeping its own copy and
independently hammering the API.

| Variable | Purpose |
| --- | --- |
| `PLANE_RPM` / `PLANE_RATE_SHARE` | Proactive pacing (default 60 RPM / share of 3 = a 3s minimum interval). |
| `PLANE_CACHE_TTL_SECS` | GET response cache TTL (default 5s). |
| `PLANE_REDIS_URL` | Redis endpoint (`redis://host:port/db`). **Unset/empty ⇒ pure in-process cache + limiter** (default; unchanged behavior). |
| `PLANE_REDIS_PASSWORD` | Optional Redis AUTH password, kept out of the URL (materialized from the vault at runtime, never hardcoded). |
| `PLANE_REDIS_TIMEOUT_MS` | Per-op Redis timeout (default 200ms). |

**Robust fail-open.** Every Redis operation is bounded by a short timeout and
guarded by a circuit breaker. On any Redis error, timeout, or outage the call
transparently falls back to the in-process cache/limiter for that operation — a
Redis outage never blocks, never slows (beyond one short timeout), and never
fails a Plane call. When Redis recovers, coordination resumes automatically with
no restart. The in-process cache is always kept warm as the instant fallback.
Cache keys are namespaced and hashed (`plane:cache:<hash>` of active-token + URL),
so no token material lands in Redis keys and one identity never sees another's
cached response. TLS is not required on the internal LAN; a `rediss://` URL door
is documented in `Cargo.toml`.

## Prefix registry (`plane_prefix_*`)

A queryable, maintainable library of the **USED/ACTIVE sub-project + issue
prefixes** — the 2–8 char item-ID prefixes like `SCRB`, `ROUT`, `RMDR`, `LSEC`.
These are the per-spec item prefixes, **not** the per-repo Plane *project*
prefixes (`HARM`/`LUM`/`CHRD`/`TERM`/`RAIL`/`HW`/`PSH`). It gives the
"a prefix must be unique — check the registry" rule real programmatic backing
instead of a stale hand-maintained doc table. It is a **sub-module of the Plane
helper** (`src/plane/prefix.rs`), so its tools register alongside `plane_*` in
both the core Chord registry and the personal registry.

**Hybrid store.**

- **Baseline** — a git-versioned TOML file (`data/prefix_registry.toml`), the
  reviewed source of truth. It is compiled into the binary via `include_str!`,
  so baseline reads always succeed regardless of working directory or Redis
  state.
- **Overlay** — a runtime claim store in the **same shared Plane Redis**
  (`PLANE_REDIS_URL`; see the pacing/caching section above). `plane_prefix_register`
  writes a claim here immediately (fast, cross-instance-visible). Promotion of an
  overlay claim into the baseline TOML happens later via a small reviewed PR (add
  a `[[prefix]]` block, drop the overlay claim).
- **Fail-open** — every overlay op is short-timeout-bounded. If Redis is
  unconfigured or unreachable, reads fall back to the baseline alone, and
  register/retire return a clear "overlay unavailable — use the file/PR path"
  result rather than erroring.

**Tools.**

| Tool | Purpose |
| --- | --- |
| `plane_prefix_list` | List/filter all known prefixes (baseline + overlay). Filters: `status`, `project`, `source` (`baseline`/`overlay`/`pending`), `include_retired`. Reports which claims are overlay-only (pending promotion). |
| `plane_prefix_check` | Is-free check for a candidate prefix + a few next-available suggestions. **Run this before writing a new spec** to satisfy the uniqueness rule. |
| `plane_prefix_register` | Claim a new prefix. Rejects on collision with the baseline **or** overlay; on success writes the claim to the overlay (flagged pending promotion). |
| `plane_prefix_get` | Fetch one prefix's merged metadata (with source flags). |
| `plane_prefix_retire` | Mark a prefix retired via an overlay status override. |

Metadata per prefix: `prefix`, `name`, `project`, `spec_id`, `status`
(`active`/`retired`/`ingested`/`complete`), `description`, `created`.

## License

MIT
