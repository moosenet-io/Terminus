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

## What's NOT here yet

- The unconditional PII sweep gate on doc-gen input (DOCGEN-02).
- Chord's SLM router integration (DOCGEN-03) and the generation orchestration
  that calls it (DOCGEN-05).
- Per-format rendering (DOCGEN-06) and artifact version control (DOCGEN-07).
- The build-skill post-feat trigger (DOCGEN-08) and the behavior-contract
  mismatch detector (DOCGEN-10).
