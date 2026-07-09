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
| `github` | GitHub | `github_create_repo`, `github_list_repos`, `github_push_repo`, `github_pii_scan`, `github_mirror_status`, `github_mirror_prepare`, `github_mirror_approve`, `github_mirror_push` |
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
`gitea_cargo_publish` recreates exactly that request and signs it with the
**resolved `GITEA_PAT_<NAME>` identity token** (the active default
`GITEA_IDENTITY_NAME`, or the optional `identity` argument — see
[Gitea identities](#gitea-identities-gitea_pat_name-convention) below), so the
publish credential lives in the runtime secret store and never has to be copied
onto the dev box or spread across containers.

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
   | `identity` | no | Which `GITEA_PAT_<NAME>` identity to publish as (defaults to the active default `GITEA_IDENTITY_NAME`, normally `moose`). |
   | `metadata` | yes | Cargo **publish-wire** metadata object (the schema cargo PUTs: `deps` with `version_req`, `features`, license, repository, ...) — not `cargo metadata` output. `name`/`vers` are forced to the explicit arguments. Required because a name+version-only publish would write an incorrect registry index for any crate with dependencies. |

   It frames the standard Cargo publish body
   (`u32-LE(len)‖metadata_json‖u32-LE(len)‖crate_bytes`) and `PUT`s it to
   `{GITEA_URL}/api/packages/{owner}/cargo/api/v1/crates/new` with
   `Authorization: token <GITEA_PAT_NAME>` (the same PAT scheme every other Gitea
   call uses). On success it returns the published
   name/version and the registry package URL; a `403` is surfaced explicitly as
   a likely missing `write:package` token scope.

> The publishing identity's `GITEA_PAT_<NAME>` token must carry the
> **`write:package`** scope. Without it the registry returns `403` and the tool
> reports the missing scope.

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

### GitHub mirror engine subtools (GHMR-04)

The public `moosenet-io/*` mirrors are **PII-swept derivatives** of internal
`main` with their own linear history (they share no ancestor with internal main).
Four github **core-tool** subtools drive that engine over a per-repo *clean work
dir* (`<TERMINUS_MIRROR_WORKDIR_ROOT>/<repo>`). All git operations run **on the
dev box** — the sanctioned git-transport host — while the logic lives in
terminus-rs; no other host holds a GitHub credential.

| Tool | Posture | What it does |
| --- | --- | --- |
| `github_mirror_status` | read-only | Reports internal-main HEAD, whether it is already approved, work-dir HEAD, and the set of `mirror-approved/*` tags (divergence + last-approved). |
| `github_mirror_prepare` | write (work dir only) | Syncs internal main's committed tree into the work dir, runs the mechanical sweep + PII gate, commits the swept derivative, and tags `mirror-approved/<internal-sha>` **only** when 0 residual violations remain. Residuals are returned for GHMR-05 cleaning; nothing is tagged. |
| `github_mirror_approve` | **guarded** | Operator authorisation of a clean snapshot. Refuses (without prompting the operator) while residual violations are pending; on a clean tree it confirms the tag and, after the one-time approval code, blesses the snapshot for push. |
| `github_mirror_push` | **guarded**, ff-only | Publishes the approved commit to the repo's GitHub remote — **fast-forward only**. Refuses any non-fast-forward (and an un-bootstrapped remote), pointing at the GHMR-07 bootstrap; **never force-pushes**. |

Common args: `repo` (logical name) and `source` (the dev-box internal-`main`
checkout path). `github_mirror_push` also takes `github_remote` (or
`TERMINUS_MIRROR_REMOTE_<REPO>` / `TERMINUS_MIRROR_REMOTE`). The push reads
`GITHUB_TOKEN` (materialised from <secret-manager> into the process env at startup) only
at the moment of transport and injects it via `GIT_ASKPASS` — the token is never
placed in the remote URL or argv, and never logged.

The guarded tools use the same per-occurrence approval gate as `openhands` /
<secret-manager>: the first call returns an `APPROVAL REQUIRED` code, and the operator
authorises the single call out of band. The one-time force re-baseline that
establishes shared lineage with each public mirror is **GHMR-07's**
operator-blessed bootstrap — never performed by these tools.

#### Residual cleaning (GHMR-05)

The mechanical sweep rewrites deterministically-fixable PII (private IPs,
container IDs, config-mapped hosts) to placeholder tokens, but leaves **residual**
violations that need judgment — a raw leaked secret, prose embedding an infra
fact. When `github_mirror_prepare` finds residuals it runs an **operationalized,
bounded cleaning pass** rather than just returning them:

1. Dispatch a scoped **cleaning subagent** — a command configured in
   `TERMINUS_MIRROR_CLEAN_CMD`, run once per round with `MIRROR_WORK_DIR` (the
   work dir it may edit) and `MIRROR_RESIDUALS_FILE` (a JSON list of the residual
   `{file, line, pattern_kind, context}` spots) in its environment, cwd set to the
   work dir. It remediates the flagged spots **in the work dir only** — the source
   repo is never handed to it and never touched. The command runs with a **cleared
   environment** (only `PATH`, `HOME`, and the two `MIRROR_*` vars are passed), so
   the external subagent never inherits the parent's service credentials.
2. Re-run the sweep + authoritative gate. If 0 residuals remain, the cleaned tree
   is committed and tagged `mirror-approved/<sha>` (tag-able).
3. Repeat up to **3 rounds** (the infinite-loop guard, which also stops early on a
   round that makes no progress). On exhaustion, the exact `file:line` spots are
   **escalated to the operator** (`cleaning.escalated: true`,
   `cleaning.escalation_spots: [...]`); nothing is committed or tagged.

When `TERMINUS_MIRROR_CLEAN_CMD` is unset, prepare escalates the residual spots
immediately (0 rounds) rather than silently passing residual PII through. The gate
is re-verified after every round, so a cleaner that under-delivers can never
smuggle residual PII into an approved tag.

**Security boundary.** The engine applies in-process defense-in-depth around the
cleaner (redacted inputs only, cleared environment, an in-memory `.git`
snapshot/restore around each round, command-line hook disabling, and running the
cleaner in its own process group which is killed as a unit afterward). These
contain buggy or casually-hostile cleaners but cannot fully contain arbitrary local
code execution — that is a fundamental limit, not a fixable bug. **The configured
cleaning command MUST be run under an OS filesystem/process sandbox** (e.g. `bwrap`
/ `nsjail` / a container with only the work dir bind-mounted, no network, killed as
a unit). That sandbox is the operator's deployment responsibility and the real
trust boundary.

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

## Gitea identities (`GITEA_PAT_<NAME>` convention)

The Gitea tool module is **multi-identity, exactly like Plane** (S105/GPAT). A
call can act as whichever agent should own the resulting commit/PR/publish,
rather than always using one shared token. Identities are configured purely
through this process's own environment — the tool never reads another process's
files.

**BREAKING:** the tool **no longer reads an unsuffixed `GITEA_TOKEN`** — that key
is gone. The effective token is always a `GITEA_PAT_<NAME>` identity token.

### Configuration

| Variable | Purpose |
| --- | --- |
| `GITEA_URL` | Base URL of the Gitea instance (required — the only hard-required Gitea var). |
| `GITEA_OWNER` | Default repo owner/organisation (default `moosenet`). |
| `GITEA_IDENTITY_NAME` | Which named identity is the **active default** when a call passes no `identity` argument. **Default `moose`** — Gitea is the operator's infra git storage. (This differs from Plane, whose default is `lumina`.) |
| `GITEA_PAT_<NAME>` | A **named identity**. Each such variable registers the identity `<name>` (lowercased) with its own token — e.g. `GITEA_PAT_MOOSE`, `GITEA_PAT_HARMONY`, `GITEA_PAT_LUMINA`. |

Named identities are matched only by the `GITEA_PAT_` prefix; a set-but-empty
`GITEA_PAT_<NAME>` is treated as absent. These values are provisioned into the
running process by the deployment's own secret-materialization step (the
vault-backed store, surfaced as `INFISICAL_*`-configured fetches at startup) —
never hardcoded into a unit file or committed anywhere.

### Acting as an identity: the `identity` argument

Every Gitea tool (`gitea_list_repos`, `gitea_create_file`, `gitea_update_file`,
`gitea_create_pr`, `gitea_merge_pr`, `gitea_cargo_publish`, …) accepts an
**optional `identity` argument**. Set it to a configured `GITEA_PAT_<NAME>` name
(e.g. `"moose"`, `"harmony"`, `"lumina"`) and that single call is authenticated
as that identity's token. Omit it and the call acts as the **active default**
identity (`GITEA_IDENTITY_NAME`, default `moose`).

Selection is centralized: all tools route through the same
`GiteaClient::resolve_identity` → `for_identity` dispatch, so the rule lives in
one place. The `identity` argument is used only to pick the token — it is never
written into a request body and never logged.

### Listing identities: `gitea_list_identities`

`gitea_list_identities` returns the names of every configured `GITEA_PAT_<NAME>`
identity (sorted, stable), the active default, and the prefix — **names only,
never token values**. Call it to see which identity you can act as before
performing Gitea work. With no named identities configured it returns an empty
list plus an explanatory note (not an error).

Select the identity through the tool's own mechanism (`gitea_list_identities` /
the client's `for_identity()` resolution) — **never** fetch a `GITEA_PAT_*` value
yourself to make a raw API/git call; that is a second, unsanctioned access path.

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

## Forge provider abstraction (`src/forge/`)

Terminus's git tooling is being reshaped from provider-specific tools (a "Gitea
tool", a "GitHub tool") into two provider-**agnostic** domains that share one
comprehensive endpoint surface and differ only by provider pool and governance
posture:

- **git-private** — self-hosted source-of-truth forges (full operator R/W).
- **git-public** — public/mirror forges (the outbound exfiltration surface,
  where the PII gate is load-bearing on every write).

Both expose the **same** endpoint vocabulary — a forge is a forge. The split is
provider pool + posture, not capability. `src/forge/` is the foundation both
tools sit on (GITX-01); the concrete adapters and the two-tool assembly land in
later items.

<!-- GITX-07 reconcile: verify tool/domain names below against merged GITX-05 assembly -->

### The two domains

| Domain | Replaces | Provider pool | Placement | Posture |
| --- | --- | --- | --- | --- |
| **git-private** | the old `gitea` tool | self-hosted forges (Gitea, Forgejo, GitLab CE, Gogs, OneDev) | **PERSONAL** registry (`terminus_personal`) | full operator R/W; source of truth; destructive/history-rewrite ops human-gated |
| **git-public** | the old `github` tool | hosted/public forges (GitHub, Codeberg, GitLab SaaS, Bitbucket, SourceHut, Radicle) | **CORE** registry (Chord-embedded) | the outbound exfiltration surface — the PII gate is an unconditional hard block on every write |

git-private sits on `terminus_personal` because it is the operator's own
infra-credentialed door to self-hosted source control — the same placement
rationale as the existing Gitea identity model. git-public stays a **CORE**
tool (Chord-embedded, alongside the GHMR mirror engine subtools already listed
above) because publishing to a public host is a shared, governed, PII-gated
operation every agent in the constellation goes through the same way, not a
personal credential a single operator identity holds. Confirm this exact
placement at the GITX-05 assembly step — it is the design intent from the
spec, not yet load-bearing code.

Per the spec's **"one surface, two pools, two postures"** principle: the
endpoint vocabulary a caller can *name* is identical on both tools (see "One
surface" below); what differs is *which providers* answer to a given tool and
*how cautiously* a write on that tool is allowed to proceed.

### Provider manifest

One Gitea-compatible client (`GiteaForge`, GITX-02) serves Gitea, Forgejo, and
Codeberg — they speak the same REST v1 API and differ only by base URL +
credential. One GitLab v4 client (GITX-04, in progress) will similarly serve
both the CE (self-hosted) and SaaS (hosted) variants by config, mapping GitLab's
merge-request/project terminology onto the shared pull-request/repo vocabulary.

| Provider id | Pool | Adapter | Status | Notes |
| --- | --- | --- | --- | --- |
| `gitea` | git-private | Gitea-family client (`forge::gitea_family::GiteaForge`) | **shipped** (GITX-02) | current source-of-truth; reuses the existing `GiteaClient` and S105 `GITEA_PAT_<NAME>` identity model wholesale |
| `forgejo` | git-private | Gitea-family client | **shipped** (GITX-02) | single `FORGEJO_TOKEN` credential |
| `gitlab_ce` | git-private | GitLab v4 client | pending (GITX-04) | optional; shares the v4 client with `gitlab_saas` |
| `gogs` | git-private | — | pending (GITX-06 stub) | optional/minimal reduced surface |
| `onedev` | git-private | — | pending (GITX-06 stub) | optional/minimal reduced surface |
| `github` | git-public | `github::adapter::GitHubAdapter` | **shipped** (GITX-03) | current mirror target; REST v3 + a GraphQL v4 helper; egress-isolated |
| `codeberg` | git-public | Gitea-family client | **shipped** (GITX-02) | recommended public target — non-profit/EU/no-AI-training, Forgejo lineage, reuses the Gitea-compatible client; single `CODEBERG_TOKEN` |
| `gitlab_saas` | git-public | GitLab v4 client | pending (GITX-04) | shares the v4 client with `gitlab_ce` |
| `bitbucket` | git-public | — | pending (GITX-06 stub), optional | Cloud REST 2.0 |
| `sourcehut` | git-public | — | pending (GITX-06 stub), optional | REST+GraphQL; **reduced capability set** — no web-PR flow, no package registry, so its capability map will mark `pull_requests_*` and `packages_*` `unsupported` rather than claim a surface it cannot offer |
| `radicle` | git-public | — | pending (GITX-06 stub), experimental/future | p2p forge; every endpoint expected `experimental` at best until the adapter matures |

Adapter reuse is deliberate: three providers (`gitea`/`forgejo`/`codeberg`)
share one Gitea-compatible client today, and two more (`gitlab_ce`/
`gitlab_saas`) will share one GitLab v4 client — a forge's *wire protocol*, not
its pool membership, decides which client implements it.

<!-- GITX-07 reconcile: confirm final gitlab_ce/gitlab_saas provider ids and any GITX-06 stub capability-map specifics against merged GITX-04/GITX-06 code -->

### Capability introspection — reading the per-provider map

Every adapter's `capabilities()` returns a `CapabilityMap` covering the
*entire* constant vocabulary (see "One surface" above), not just what it
implements — an endpoint absent from the map still reports `unsupported`
rather than silently disappearing. `ForgeProvider::capability_report()` (or
`CapabilityMap::report()` directly) renders it as JSON grouped by
`ForgeDomain` (`repos`, `branches`, `commits`, `pull_requests`, `issues`,
`releases`, `webhooks`, `packages`, `content`, `org`):

```json
{
  "repos": { "repos_list": "supported", "repos_mirror_config": "unsupported" },
  "pull_requests": { "pull_requests_create": "supported" },
  "packages": { "packages_publish": "unsupported" }
}
```

Read it before calling an endpoint on an unfamiliar provider — e.g. before
assuming SourceHut can open a pull request, or that GitHub can configure a pull
mirror. Calling an endpoint the map marks `unsupported` never reaches the
network: `ForgeProvider::dispatch` rejects it locally with a clean
`ForgeError::Unsupported` naming the provider (see "The trait and the
'unsupported' negative path" below). An endpoint marked `supported` or
`experimental` is *attempted*; `experimental` is the honest label for a
reduced-confidence or partial implementation (e.g. an early Radicle
endpoint) — it will be called, but treat its result with the same caution the
label implies.

### Governance postures

The two domains carry deliberately different write postures, matched to what
each pool is *for*:

**git-private — operator source-of-truth.** Full operator read/write against
self-hosted forges; per-provider vault credentials (see "Operator: adding or
activating a provider" below); every write is audit-logged (sanitized per the
standing audit rules — tokens and secrets redacted, large payloads truncated).
Destructive operations — repository delete, force-push, history rewrite —
require confirmation and are human-gated, mirroring the posture the guarded
`openhands`/<secret-manager> tools already use (a pending request with a single-use
code that only an out-of-band operator approval releases).

**git-public — the exfiltration surface.** Every write/push/publish passes an
**unconditional** PII gate — the same Rust engine documented above under "PII
gate (Rust, authoritative)" — before anything reaches the network. A failing
sweep **withholds** the push (the content stays private; nothing partial is
sent), logs the finding, and flags it. There is no bypass flag, no env var
override, and no cadence fast-path — the gate always wins over "we're behind
on publishing." Reads are unrestricted (a public forge's read surface is, by
definition, already public). Beyond the gate, egress isolation applies (see
the GitHub adapter's `host_allowed` allowlist above; each public provider
gets its own host allowlist) so a public-pool adapter cannot be pointed at an
arbitrary endpoint. **First publish is human-gated**: the first time any
repo is published to a given public provider, an explicit, once-per-
repo/provider operator confirmation (the `mirror_activated` model) is
required before subsequent pushes proceed automatically — this mirrors the
GHMR mirror engine's one-time bootstrap force re-baseline (see "GitHub mirror
engine subtools" above), generalized to every git-public provider rather than
just GitHub.

<!-- GITX-07 reconcile: confirm the exact posture-enforcement mechanism (where in the call path the PII gate + first-publish gate are wired) against the merged GITX-05 tool assembly -->

### The mirror engine as git-public's swept-write path

The already-shipped GHMR mirror engine (see "GitHub mirror engine subtools
(GHMR-04)" above — `github_mirror_status` / `_prepare` / `_approve` / `_push`,
the per-repo clean work-dir derivative, the mechanical sweep + Rust PII gate,
the bounded residual-cleaning pass) is **not rebuilt** for this overhaul. It
becomes git-public's general **provider-agnostic swept-write path**: the
engine's clean work-dir + PII-gate model is how git-public reconciles "the
operator's real source tree usually is not public-clean" with "every
git-public write must be gate-clean" — a mirror always routes an internal
`git-private` source through the sweep before it ever reaches a `git-public`
provider. `GitHub` is the only configured mirror target today
(`.moosenet-pipeline.yaml`'s `mirror_ready` + `github_remote`); the pipeline
config gains a provider/target selector so the same swept-work-dir mechanism
can address Codeberg or another public pool member without re-deriving the
PII engine per provider.

### Operator: adding or activating a provider

A provider only activates if its configuration is present — an unconfigured
provider simply does not register, no error. Activating one is two things:
runtime vault credentials, and (where applicable) a base-URL config var.

**Credential key-name convention.** Every provider follows the same shape
already established by Gitea/GitHub/Plane (S105/S94):

| Credential shape | Meaning | Example |
| --- | --- | --- |
| `<PROVIDER>_PAT_<NAME>` | a named identity's token (multi-identity providers) | `GITEA_PAT_MOOSE`, `GITHUB_PAT_HARMONY` |
| `<PROVIDER>_TOKEN` | a single unsuffixed token (single-credential providers, or the legacy fallback identity) | `FORGEJO_TOKEN`, `CODEBERG_TOKEN` |
| `<PROVIDER>_URL` | the provider's base API URL, where it is not a fixed public host | `GITEA_URL`, `FORGEJO_URL`, `CODEBERG_URL` (optional — defaults to Codeberg's own host) |

None of these are ever literals in source, config, or `.moosenet-pipeline.yaml`.
As actually wired in the merged `gitea_from_env()` / `forgejo_from_env()` /
`codeberg_from_env()` / `GitHubAdapter::from_env()` constructors (GITX-02/03),
tokens and URLs are both read via `std::env::var(...)` at adapter-construction
time — this crate's sanctioned vault path is that
[`crate::secrets_bootstrap`](src/secrets_bootstrap.rs) materializes the runtime
secret store into this process's own environment at startup, so that env read
IS the vault read (never another process's files, never a literal). This is
the same posture the pre-existing Gitea/GitHub/Plane identity docs above use.
`CredentialRef` (`forge::provider::CredentialRef`, see "Credentials" above) is
GITX-01's key-name-reference *type* for the trait; the concrete GITX-02/03
adapters do not yet route through it end-to-end — they resolve directly via
`env::var` in their `*_from_env` constructors. `config.rs` does not currently
carry forge-specific URL helpers; base URLs are read the same way as tokens,
directly via `env::var` in each adapter's constructor.

<!-- GITX-07 reconcile: if GITX-05's assembly wires adapters through
     CredentialRef / adds config.rs helpers for forge URLs, tighten this
     paragraph to match — flagged by agy review during GITX-07's own gate. -->

**Steps to add a new provider to an existing adapter family** (e.g. pointing
the Gitea-family client at a second self-hosted Forgejo instance, or turning
on Codeberg as a mirror target):

1. Provision the credential(s) in the runtime secret store under the
   convention above (an operator/ops action — never hardcoded by an agent).
2. Set the provider's base URL config var if it is not a fixed public host
   (Gitea and Forgejo require this; GitHub, Codeberg, and GitLab SaaS default
   to their public hosts and only need it to point at a self-hosted mirror).
3. If the provider is new to git-public's mirror rotation, add its
   `mirror_ready`/target entry to `.moosenet-pipeline.yaml`'s provider
   selector (see "The mirror engine as git-public's swept-write path" above)
   and complete the one-time, operator-blessed bootstrap force re-baseline for
   that provider before routine pushes can fast-forward.
4. Confirm activation by reading the tool's capability introspection for that
   provider id (see "Capability introspection" above) — a provider that
   registered will report its real support map; one that is still
   unconfigured will not appear at all.

Nothing else changes: the shared endpoint vocabulary, the capability model,
and the governance posture (private vs. public pool) all apply automatically
once a provider is configured — there is no separate "wire up this provider's
posture" step per provider.

<!-- GITX-07 reconcile: verify the exact MCP tool names/config-key spellings the operator actually calls (e.g. `git_private_*`/`git_public_*` vs retained `gitea_*`/`github_*` names, and any new env-var names for provider selection) against the merged GITX-05 assembly — this section documents the spec's design intent, not yet-merged code. -->

### One surface: the endpoint vocabulary

`forge::capability` defines the shared surface as a constant, machine-enumerable
vocabulary (`ForgeEndpoint`, grouped by `ForgeDomain`): repos, branches/refs,
commits, pull/merge requests, issues, releases/tags, webhooks,
packages/registry, content, and org/collaboration. `ForgeEndpoint::all()`
iterates the whole vocabulary. The vocabulary is constant across providers; only
availability varies.

### Availability varies: capability introspection

Each adapter advertises which endpoints it supports via a `CapabilityMap`
(per-endpoint `SupportLevel`: `supported` / `experimental` / `unsupported`; an
absent entry defaults to `unsupported`). `CapabilityMap::report()` /
`ForgeProvider::capability_report()` return the per-adapter support map as JSON,
grouped by domain — the introspection surface both forge tools expose so callers
can see what a given provider can and cannot do.

### The trait and the "unsupported" negative path

Every adapter implements `ForgeProvider` (`forge::provider`). The trait pairs the
vocabulary with a capability-gated `dispatch`: an endpoint the adapter does not
advertise returns a clean `ForgeError::Unsupported` naming the provider
(`"endpoint 'repos_delete' is unsupported by provider 'sourcehut'"`), and an
advertised-but-unwired endpoint returns `ForgeError::NotImplemented` — the
surface **never fabricates** a result for a call it cannot make. Adapters
implement `execute_endpoint` for the endpoints they support and declare them in
`capabilities()`.

### Credentials

`CredentialRef` references a credential by its runtime secret **key name**
(e.g. `GITEA_PAT_<NAME>`, `GITHUB_PAT_<NAME>`) — never the secret value. Adapters
resolve the actual token at call time from the secret store via
`SecretManager` / `vault::manager().get(key_name)`, so no credential literal ever
appears in source, config, or logs.

### GitHub adapter (`github`, git-public pool) — GITX-03

`github::adapter::GitHubAdapter` is the first concrete adapter: the **git-public**
pool's GitHub provider, implementing `ForgeProvider` over the GitHub REST v3 API
(with a `GitHubAdapter::graphql` v4 helper for the endpoints REST cannot express).
It carries the existing `github_*` tool logic into the trait shape; the standalone
`github_list_repos` / `github_create_repo` / `github_push_branch` tools and the
GHMR mirror engine keep working unchanged. This item builds only the adapter — the
git-public MCP *tool* and its PII-gate write posture are assembled later (GITX-05).

- **Capability map.** GitHub advertises nearly the whole vocabulary as
  `supported`. The two honest gaps are left `unsupported` so the map never claims a
  call the adapter cannot make: `repos_mirror_config` (GitHub has no pull-mirror
  configuration REST endpoint) and `packages_publish` (publishing goes through a
  registry wire protocol — npm/Cargo/OCI — not a single REST call).
- **Per-identity credentials.** Tokens resolve, in order: a request's `identity`
  → `GITHUB_PAT_<NAME>`; else the active-default identity (`GITHUB_IDENTITY_NAME`,
  default `moose`) → its `GITHUB_PAT_<NAME>`; else the unsuffixed `GITHUB_TOKEN`
  fallback. This mirrors the Gitea `GITEA_PAT_<NAME>` model (S105). Every resolved
  token is `.trim()`-ed (a trailing newline is a classic silent-`401`) and is never
  logged — the `Debug` impl redacts it. Missing/blank credential → a clean
  `ForgeError::Auth`, never an empty `Authorization` header. `startup` materializes
  `GITHUB_PAT_*` from the secret store alongside `GITEA_PAT_*`/`PLANE_PAT_*` (the
  env read is this crate's sanctioned vault path).
- **Public-pool marker.** `GitHubAdapter::is_public_pool()` returns `true` so the
  GITX-05 tool assembly applies the exfiltration-surface posture (unconditional PII
  gate on writes). The adapter itself does not gate — it advertises the pool.
- **Egress isolation.** Every outbound call passes `GitHubAdapter::host_allowed`
  first: only the configured API base authority plus the `github.com` family
  (extendable via `GITHUB_EGRESS_ALLOWLIST`) may be dialed. Exact-authority
  matching — a non-allowlisted host is refused locally rather than dialed, so the
  adapter cannot be pointed at an arbitrary exfil endpoint.
- **Error mapping.** `401`/`403` → `ForgeError::Auth` (the auth/scope-failure
  surface); other non-2xx → `ForgeError::Transport`; an unsupported endpoint is
  rejected by `dispatch` before any transport.
- **Config.** `GITHUB_API_BASE` (override, test), `GITHUB_ORG` (default owner,
  `moosenet-io`), `GITHUB_IDENTITY_NAME` (default identity), `GITHUB_EGRESS_ALLOWLIST`
  (extra hosts). None are required — capability introspection needs no credential.

### Gitea-family adapter (`forge::gitea_family`, GITX-02)

The first concrete adapter — `GiteaForge` — is **one** Gitea-compatible-REST-API
client that implements `ForgeProvider` and, parameterised by base URL +
credentials, serves **three** providers that all speak the Gitea REST v1 API:

| Provider id | Pool | Config | Credential model |
|---|---|---|---|
| `gitea` | git-private | `GITEA_URL` + `GITEA_PAT_<NAME>` | S105/GPAT multi-identity (default identity `moose`) |
| `forgejo` | git-private | `FORGEJO_URL` + `FORGEJO_TOKEN` | single token |
| `codeberg` | git-public | `CODEBERG_URL` (defaults to Codeberg's host) + `CODEBERG_TOKEN` | single token |

Construct with `GiteaForge::gitea_from_env()` / `forgejo_from_env()` /
`codeberg_from_env()` (only configured providers activate). The three differ
**only** by base URL + credential source — the wire protocol is identical, so a
single dispatch path drives all of them; nothing branches on provider.

- **Reuses the existing Gitea client wholesale.** The `gitea` provider wraps the
  very same `GiteaClient` the concrete `gitea_*` tools use, so the S105
  `GITEA_PAT_<NAME>` identity model, `gitea_cargo_publish`, and the dev-box
  git-relay posture all carry forward unchanged. The existing `gitea_*` tools
  remain registered and behave exactly as before — this adapter is additive
  (the git-private/git-public tool assembly, provider routing, and the
  unconditional PII gate on public writes land later in GITX-05).
- **Full capability set.** Gitea REST v1 covers essentially the entire shared
  vocabulary, so the Gitea-family `CapabilityMap` advertises **every**
  `ForgeEndpoint` as `supported` (Forgejo/Codeberg share the same API and map).
  `capability_report()` returns the complete grouped map.
- **Endpoints.** Repos (list/get/create/update/delete/fork/mirror-config/
  visibility/metadata), branches + generic refs, commits (list/get/compare/
  status), pull requests (list/get/create/update/review/comment/merge/close),
  issues (list/get/create/update/comment/label/assign/close), releases + tags,
  webhooks (incl. test), packages (list/get/publish/delete — publish reuses the
  shared Cargo publish framing), content (read/write file, list tree, raw
  fetch), and org/collaboration — each mapped to its Gitea REST path.
- **Content writes run the content PII gate** (same check as `gitea_create_file`
  / `gitea_update_file`); a `sha` argument routes an update (`PUT`) versus a
  create (`POST`).
- **Token hygiene — the `.trim()` fix.** Every token-loading path trims the
  credential value: `scan_gitea_identities` (for `GITEA_PAT_<NAME>`) and
  `GiteaClient::with_token` (for `FORGEJO_TOKEN` / `CODEBERG_TOKEN`) both
  `.trim()` before storage, so a trailing newline or surrounding whitespace in a
  stored PAT can never corrupt the `Authorization: token <PAT>` header again (a
  whitespace-only value trims to empty and is treated as absent). This closes a
  real failure mode previously hit on `GITEA_PAT_MOOSE`.
- **Honest failure surfaces.** An unreachable instance yields a clean
  `ForgeError::Transport` (not a panic or fabricated result); a missing/invalid
  or under-scoped credential yields `ForgeError::Auth`.

## License

MIT
