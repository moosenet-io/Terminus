# tools

`src/tools` — 772 KG symbols.

Most tool modules live at the crate root, one directory per integration. The
`tools/` namespace hosts the two surfaces that sit *on top of* other subsystems
rather than wrapping a single external service: **docgen**, the sovereign
documentation engine, and **serving_tools**, the model-serving control/status
tools built on `intake::serving` and the Chord control plane.

Docgen generates and places repository documentation (landing README + `docs/`
tree) as a byproduct of the build pipeline. Its design invariants: placement is
fail-closed and single-writer (`place.rs` is the only component that writes to a
working tree — never a network, git, or forge call); every LLM input and diagram
source passes a PII sweep; a no-loss preservation check guards against dropping
real content from an old README; and a docgen failure never fails the build item
that triggered it.

## Key types and functions

| Symbol | Kind | File | Description |
|---|---|---|---|
| `tools::docgen::repo_facts` | module | `src/tools/docgen/repo_facts.rs` | Deterministic RepoFacts extraction (KG subsystem rollup, entry points, config surface) that grounds generation. |
| `tools::docgen::pii_gate::sweep_input` | fn | `src/tools/docgen/pii_gate.rs` | PII sweep every generation input passes before reaching a prompt. |
| `tools::docgen::pii_gate::PiiGateOutcome` | struct | `src/tools/docgen/pii_gate.rs` | Sweep result; `sanitized_content()` is what generation is allowed to see. |
| `tools::docgen::config::ProjectDocConfig` | struct | `src/tools/docgen/config.rs` | Per-project docgen opt-in and options; `parse` reads the project's doc config. |
| `tools::docgen::versioning::VersionStore` | struct | `src/tools/docgen/versioning.rs` | Versioned artifact store keyed by `ArtifactKey` (doc artifact + git ref). |
| `tools::docgen::crate_graph::CrateGraphModel` | struct | `src/tools/docgen/crate_graph.rs` | Crate/module dependency model behind the generated architecture material (`sort` orders it deterministically). |
| `tools::docgen::changelog::CommitInput` | struct | `src/tools/docgen/changelog.rs` | Normalized commit input for `docgen_generate_changelog`. |
| `tools::docgen::search_index::tokenize` | fn | `src/tools/docgen/search_index.rs` | Tokenizer for the generated docs search index. |
| `tools::docgen::backfill` | module | `src/tools/docgen/backfill.rs` | `docgen_backfill`: one guarded pass migrating an existing bloated README into a concise landing + `docs/` hierarchy, preserving old-README content. |
| `tools::docgen::trigger` | module | `src/tools/docgen/trigger.rs` | The post-merge trigger; structurally infallible (`TriggerOutcome` — a docgen failure never fails the feat). |
| `tools::docgen::quality` | module | `src/tools/docgen/quality.rs` | Landing/page quality lints enforced before placement. |
| `tools::serving_tools` | module | `src/tools/serving_tools.rs` | The `serving_*` control/status tools over the serving intake foundation. |

## Tools

`docgen_run`, `docgen_backfill`, `docgen_status`, `docgen_facts`,
`docgen_generate_changelog`, `docgen_drift_check`, `docgen_mismatch_detect`,
plus three `serving_*` tools.

## How it connects

Docgen runs inside the terminus binary and consumes `scribe::graph` natively —
the same per-project Atlas store the `kg_*` tools serve — for facts, subsystem
rollups, and diagrams. Long-form prose generation dispatches through the
`review` subsystem's daemon seam (`review::dispatch`), and
`docgen_mismatch_detect` reuses `review`'s provider model. `docgen_backfill` is
the tool that produced this repository's previous README; the placement writer
(`place.rs`) is the single component in the crate allowed to write generated
docs into a working tree.

## Configuration

Per-project options come from `ProjectDocConfig` (repo-side config file), not env
vars. The docgen PII gate shares the repo-level `pii-gate.toml` /
`TERMINUS_PII_CONFIG` convention with the `pii_gate` binary.

## Notes and gaps

This page does not document the render layer (`render/`, `readme_layers.rs`,
`diagram.rs`, `svg_assets.rs`) file by file, nor the drift/mismatch detection
heuristics — see [docs/tools/code-git/docgen.md](../tools/code-git/docgen.md).
The serving tools' operational semantics live with the MINT serving docs
([docs/tools/mint/serving-profiles.md](../tools/mint/serving-profiles.md)).
