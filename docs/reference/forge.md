# forge

`src/forge` — 836 KG symbols.

Forge is the provider-agnostic git abstraction. Instead of one tool per forge
product, Terminus exposes two governance *domains* that share a single endpoint
vocabulary: **git-private** (self-hosted source-of-truth forges, full operator
read/write) and **git-public** (public/mirror forges — the exfiltration surface,
where the PII gate is load-bearing on every write). Concrete adapters implement
the same `ForgeProvider` trait; a capability map keeps each adapter honest about
what its provider actually supports ("vocabulary constant; availability
varies"). On top sits the mirror engine, which maintains PII-swept public
derivatives of internal `main` with their own linear git history.

## Key types and functions

| Symbol | Kind | File | Description |
|---|---|---|---|
| `forge::capability::ForgeEndpoint` | enum | `src/forge/capability.rs` | The constant endpoint vocabulary, grouped by `ForgeDomain`. |
| `forge::capability::ForgeDomain` | enum | `src/forge/capability.rs` | `git_private` vs `git_public` — the two governance postures. |
| `forge::capability::CapabilityMap` | struct | `src/forge/capability.rs` | Per-adapter supported-endpoint map with JSON introspection report. |
| `forge::provider::ForgeProvider` | trait | `src/forge/provider.rs` | The adapter contract; capability-gated dispatch returns `ForgeError::Unsupported` rather than faking a call. |
| `forge::provider::ForgeRequest` | struct | `src/forge/provider.rs` | A typed endpoint invocation handed to `dispatch`. |
| `forge::provider::CredentialRef` | struct | `src/forge/provider.rs` | Vault-key reference for adapter credentials — key names, never literals. |
| `forge::gitea_family` | module | `src/forge/gitea_family.rs` | One Gitea-compatible REST adapter (`GiteaForge`) serving Gitea, Forgejo (private) and Codeberg (public). |
| `forge::gitlab` | module | `src/forge/gitlab.rs` | The GitLab adapter (GITX-04). |
| `forge::mirror::sweep` | module | `src/forge/mirror/sweep.rs` | Mechanical PII rewrite: private IPs, container ids, internal paths/URLs → placeholder tokens; reports residuals. |
| `forge::mirror::workdir::run_git` | fn | `src/forge/mirror/workdir.rs` | Git plumbing for the clean work-dir manager (per-repo swept derivative with its own history). |
| `forge::mirror::clean` | module | `src/forge/mirror/clean.rs` | Bounded (≤3 rounds) residual-cleaning pass; escalates exact `file:line` spots when it can't reach zero. |
| `forge::mirror::runner::RunnerConfig` | struct | `src/forge/mirror/runner.rs` | Configuration for the idempotent per-repo "run once" mirror orchestration (`git_public_mirror_run`). |

## Tools

`register_all` registers the **git-public** tool surface (`register_public`);
`register_personal` registers **git-private** (`register_private`) — the write
door to source-of-truth forges is deliberately kept off the core/Chord-served
registry. Mirror subtools: `git_public_mirror_status`, `_prepare`, `_approve`,
`_push`, `_sync_source`, `_replay_pr`, and `git_public_mirror_run` (the
timer-driven orchestration, `deploy/terminus-mirror-runner.timer`).

## How it connects

Residual detection reuses `github::pii` — the same authoritative gate as the
`pii_gate` binary and the GitHub write tools, so "clean" means the same thing
everywhere. `cortex_calibrate` reaches PRs and diffs exclusively through
`ForgeRegistry::from_env().resolve(..)` + `ForgeProvider::dispatch` (it imports
no HTTP client of its own — asserted structurally in its tests). The mirror
engine writes only into its own work dirs, never the source repo, and never
force-pushes.

## Configuration

Mirror behavior: `TERMINUS_MIRROR_AUTHOR_MAP`, `TERMINUS_MIRROR_AUTO_APPROVE`,
`TERMINUS_MIRROR_AUTO_BASELINE`, `TERMINUS_MIRROR_BLACKLIST`,
`TERMINUS_MIRROR_CLEAN_CMD`, `TERMINUS_MIRROR_GITHUB_HOST`; domain activation:
`TERMINUS_GIT_PUBLIC_ACTIVATED_STATE`. Adapter credentials resolve through
`CredentialRef` vault keys (per-provider PAT names). Names only.

## Notes and gaps

Experimental adapter stubs (Bitbucket, SourceHut, Gogs, OneDev, Radicle) exist
with honest reduced capability maps; every unimplemented endpoint returns
`ForgeError::NotImplemented`. This page does not cover the per-endpoint
vocabulary or mirror runbook detail — see
[docs/tools/code-git/forge.md](../tools/code-git/forge.md) and
[docs/tools/code-git/mirror-runner.md](../tools/code-git/mirror-runner.md).
