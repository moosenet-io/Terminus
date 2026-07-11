[← Tool index](../README.md) · [← Docs index](../../README.md)

# docgen — the sovereign documentation engine (scaffold)

**Status: scaffold only (DOCGEN-01, spec `S95-documentation-engine`, Plane
TERM-143).** This page documents the module as it exists after DOCGEN-01:
core types, the per-project doc-target config schema, and one read-only
inspection tool. Generation, multi-format rendering, and versioning are
separate, later items (DOCGEN-05/06/07) — this page will grow as they land.

`docgen` lives at `src/tools/docgen/` (`mod.rs`, `config.rs`), registered via
`crate::tools::register` in `src/tools/mod.rs`, itself called from
`register_all()` in `src/registry.rs` — the same core registration path
`plane`/`gitea`/`github`/`scribe` use. It is a **separate module from
`scribe`** (`src/scribe/`): Scribe is the standing knowledge-vault
documentation agent; `docgen` is the sovereign, config-driven, multi-format
"replaces Mintlify" engine described in `S95-documentation-engine.md`. Later
docgen items reuse `crate::scribe::{inspect, vault}` and `crate::github::pii`
rather than duplicating them — see the module's own doc comment for the
reuse plan.

## Why this exists

Per the S95 design overview: after every feat merges and verifies, the doc
engine reads what was actually built (the merged diff + spec), deepens a
project's documentation, and renders whatever artifacts that project has
declared — README, wiki, PDF, Notion/Obsidian notes, a dev blog post — via
Chord's SLM router, after an unconditional PII sweep gates the input. This
item ships none of that flow yet; it ships the schema the rest of the engine
is built on top of.

## Per-project doc-target config (`config.rs`)

A project declares which output artifacts it wants as a list of targets,
each one of `readme` | `wiki` | `pdf` | `notion` | `obsidian` | `blog`, plus
free-form per-target rendering options:

```json
{
  "targets": [
    { "type": "readme" },
    { "type": "notion", "options": { "database_id": "..." } }
  ]
}
```

- **No config declared** (absent, or an empty/missing `targets` list) →
  the safe default: README-only. An unconfigured project is valid, not
  malformed — the engine never forces docs on a project that hasn't opted
  in.
- **An unknown target type** → a clear, typed error (`ToolError::InvalidArgument`
  naming the bad value and the valid set), never a panic.
- **A declared target whose credential is missing** (e.g. a `notion` target
  with no `NOTION_TOKEN` available) → that target resolves as disabled with
  a human-readable hint; every other declared target still resolves
  independently. `config.rs` never reads a secret *value* to determine
  this — [`ProjectDocConfig::resolve`] takes the caller-supplied set of
  available credential **key names** and only ever names the key a target
  needs via `DocTargetType::credential_key()`. Resolving that key to an
  actual value via `vault::manager().get()` / `SecretManager::get()` is
  deferred to the generation/render items that actually call out to a
  target's API.

## `docgen_status`

The one tool this scaffold registers — read-only, config-inspection only:

```json
{
  "name": "docgen_status",
  "arguments": {
    "project_config": { "targets": [{ "type": "readme" }, { "type": "notion" }] },
    "available_credential_keys": ["NOTION_TOKEN"]
  }
}
```

Returns which targets the config declares (or the README-only default), and
— when `available_credential_keys` is supplied — which are enabled vs.
disabled for a missing credential. It generates and renders nothing.

## `docgen_generate_changelog` (DOCGEN-17, Plane TERM-168)

Generates a Keep-a-Changelog-formatted `CHANGELOG.md` section plus a
separate, human-readable release-notes document from a list of merged
commits, parsed as Conventional Commits (`feat(...)/fix(...)/docs(...)/...`
— this repo's own commit convention already fits). Lives at
`src/tools/docgen/changelog.rs`.

**git-cliff vs. built-in:** the originating research pointed at `git-cliff`
(Rust CLI, Tera templates, Keep-a-Changelog preset). No `git-cliff` binary
is available in this build environment, and it is a standalone binary, not
a library crate to vendor — so this item ships a minimal, dependency-free,
in-process Conventional-Commit parser and Keep-a-Changelog/release-notes
renderer instead, producing the same grouped-by-type, dated shape of
output. A future item may shell out to a real `git-cliff` binary once one
is provisioned on a build host.

- Commits are grouped into fixed Keep-a-Changelog sections (Breaking
  first, then Added/Changed/Fixed/Documentation/Performance/Tests/Build &
  CI/Reverted/Chore/Other) in deterministic order.
- A commit that doesn't match Conventional Commit shape is never dropped —
  it's included under `Other` with its original subject line.
- Merge commits (`Merge pull request ...` / `Merge branch ...`) are
  excluded as noise but counted (`merge_commits_excluded`).
- Breaking changes (`type(scope)!: ...` or a `BREAKING CHANGE:` footer) are
  flagged and rendered first in both artifacts.
- **Deterministic, no hidden I/O**: like `versioning.rs`/`render.rs`, this
  module never reads the system clock — the caller supplies `version` and
  `date`. The same input always produces byte-identical output.
- **Write-model inversion**: like `render.rs`, this RETURNS the two
  artifacts as strings; it never places them into a repo. Versioning them
  (DOCGEN-07) is the caller's job, via the existing
  `versioning::VersionStore` using `ArtifactKey::new(project, "changelog")`
  / `ArtifactKey::new(project, "release_notes")` — no second version store.

```json
{
  "name": "docgen_generate_changelog",
  "arguments": {
    "project": "Terminus",
    "version": "1.6.0",
    "date": "2026-07-11",
    "commits": [
      {"hash": "abc1234", "message": "feat(docgen): DOCGEN-17 -- changelog generation"},
      {"hash": "def5678", "message": "fix(docgen): correct grouping bug"}
    ]
  }
}
```

**DOCGEN-08 trigger wiring:** DOCGEN-08 (the post-feat build-skill trigger)
has not shipped yet. This item exposes the API DOCGEN-08 will call once it
lands, rather than wiring an automatic trigger that doesn't exist yet —
tracked as a follow-up on DOCGEN-08 itself.

## What's NOT here yet

- The build-skill post-feat trigger (DOCGEN-08) that automatically invokes
  changelog generation (and the rest of docgen) after every merged feat.
