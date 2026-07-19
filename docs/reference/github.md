# github

`src/github` — 255 KG symbols.

Two things live here: the GitHub org tools, and — more load-bearing — the
authoritative **PII engine** that guards every path by which content can leave
the private network for a public forge. The tools are `github_list_repos`,
`github_create_repo`, `github_push_repo` (builds the mirror push command), and
`github_push_branch`, which creates or fast-forwards a branch purely through
the Git Data API (blobs → tree → commit → ref) with no git wire protocol and no
subprocess. Every write tool runs its outbound content through the PII gate
before any network request fires, and there is no flag, env var, or argument
that disables it.

## Key types and functions

| Symbol | Kind | File | Description |
|---|---|---|---|
| `github::pii::scan_for_pii` | fn | `src/github/pii.rs` | Scan content, returning every `PiiViolation` (private IPs, container ids, internal hostnames/terms, key-shaped strings). |
| `github::pii::scan_and_redact` | fn | `src/github/pii.rs` | Scan and return redacted content plus the violation list. |
| `github::pii::PiiRuleSet` | struct | `src/github/pii.rs` | The configurable rule set (repo-specific terms, extra patterns, allowlist) behind both scan entry points. |
| `github::pii::clear_allow` | fn | `src/github/pii.rs` | Test/reset hook for the allowlist state. |
| `github::adapter` | module | `src/github/adapter.rs` | The typed GitHub REST adapter (`req`, `test_adapter`) the tools call through. |
| `github::adapter::GitHubAdapter::resolve_token` | fn | `src/github/adapter.rs` | Identity-resolved credential lookup: `GITHUB_PAT_<NAME>` for the active identity, with the unsuffixed `GITHUB_TOKEN` kept only as a legacy fallback. |
| `github::cfg` / `github::cfg_with_base` | fns | `src/github/mod.rs` | Config constructors (the latter overrides the API base for httpmock-backed tests). |

## Consumers of the PII engine

- The GitHub write tools (mandatory pre-network gate).
- The `forge::mirror` engine — residual detection after the mechanical sweep
  reuses this exact gate, so "mirror-approved" means the same thing as
  "push-gate clean".
- The `pii_gate` binary (`src/bin/pii_gate.rs`) — the git pre-push/pre-commit
  hook: scans committed blobs being pushed (or `--staged` index blobs, or a
  whole `--tree`), reads git objects rather than the working tree, and emits
  human or `--json` reports.
- `tools::docgen`'s prompt-input sweep shares the same repo-config convention.

## How it connects

Registered on both registries (core and personal). `mesh::principal`'s
canonical name selects the `GITHUB_PAT_<NAME>` identity
(`GITHUB_IDENTITY_NAME` default `moose`). The newer [forge](forge.md)
abstraction covers GitHub-shaped mirror targets provider-agnostically; this
module remains both the direct tool suite and the home of the shared PII
engine.

## Configuration

`GITHUB_PAT_<NAME>` (required; `GITHUB_TOKEN` legacy fallback only),
`GITHUB_IDENTITY_NAME`, `GITHUB_ORG` (default `moosenet-io`), `GITEA_URL`
(referenced when building the mirror command), `GITHUB_API_BASE` (test-only
override), `TERMINUS_PII_CONFIG` / repo-root `pii-gate.toml` (repo-specific
gate terms, patterns, allowlist).

## Notes and gaps

If no credential is configured, `NotConfigured` stubs register so callers get a
clear error instead of a panic. This page does not document the individual PII
pattern classes or placeholder conventions — see the gate config
(`pii-gate.toml`) and [docs/tools/code-git/github.md](../tools/code-git/github.md).
