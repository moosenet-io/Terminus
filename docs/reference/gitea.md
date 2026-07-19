# gitea

`src/gitea` — 292 KG symbols.

The Gitea subsystem is the direct tool suite for the fleet's self-hosted Gitea
instance: 20 tools covering repos, files, branches, pull requests, issues, and
releases over the Gitea REST API, plus the merge queue that serializes the
build pipeline's PR merges. Authentication uses the named-identity model:
every call can act as a `GITEA_PAT_<NAME>` identity (mirroring the Plane
convention), and the unsuffixed `GITEA_TOKEN` is deliberately gone — there is
no anonymous write path. Write operations pass a PII gate that scans content
for private IP ranges and API-key patterns before anything reaches the forge —
a protection the legacy Python implementation lacked.

## Key types and functions

| Symbol | Kind | File | Description |
|---|---|---|---|
| `gitea::GiteaClient` | struct | `src/gitea/mod.rs` | The typed REST client: `base_url`, `api`, `get`, `auth_header`, `with_token`. |
| `gitea::with_identity_param` / `gitea::identity_param_schema` | fns | `src/gitea/mod.rs` | The shared optional-`identity` schema convention (same shape as `plane`'s). |
| `gitea::merge_queue::MergeQueue` | struct | `src/gitea/merge_queue.rs` | Serialized PR merge queue for the build pipeline. |
| `gitea::merge_queue::MergeQueueConfig` | struct | `src/gitea/merge_queue.rs` | Queue tuning (polling, retries). |
| `gitea::merge_queue::MergeQueueSnapshot` | struct | `src/gitea/merge_queue.rs` | Point-in-time queue state for status reporting. |
| `gitea::types` | module | `src/gitea/types.rs` | Request/response types (`GiteaCreatePrRequest`, `GiteaFileContent`, `GiteaBranchInfo`, ...). |
| `gitea::mock_client` | fn | `src/gitea/mod.rs` | Test constructor — the suite tests against mocks, no live Gitea needed. |

## How it connects

Registered on **both** registries (core and personal) — one of the four
deliberate core/personal overlaps (plane, gitea, github, sundry). The
`mesh::principal` canonical name selects which `GITEA_PAT_<NAME>` a call uses.
The newer provider-agnostic [forge](forge.md) subsystem serves the same Gitea
instance through its `gitea_family` adapter for the `git_private`/`git_public`
domains; this module remains the direct, per-endpoint suite. The Cargo registry
side (publishing/consuming crates via Gitea's Cargo registry) is a consumer of
the same instance but is driven by the `compiler` publish flow, not by these
tools.

## Configuration

- `GITEA_URL` — base URL (required; tools return `NotConfigured` without it).
- `GITEA_PAT_<NAME>` — named-identity personal access tokens.
- `GITEA_IDENTITY_NAME` — active default identity when a call passes no
  `identity` argument (default `moose`; note this differs from Plane's default).
- `GITEA_OWNER` — default repo owner/organisation (default `moosenet`).

## Notes and gaps

The unsuffixed `GITEA_TOKEN` fallback was removed (S105/GPAT) — older external
notes referencing it are stale. This page does not enumerate all 20 tool
schemas or the merge-queue state machine in detail — see
[docs/tools/code-git/gitea.md](../tools/code-git/gitea.md).
