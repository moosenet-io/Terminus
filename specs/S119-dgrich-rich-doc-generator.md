# Rich, KG-grounded multi-level doc generator (DGRICH)
plane_project: TERM
module: Terminus
prefix: DGRICH
spec_id: S119-dgrich-rich-doc-generator

## Metadata
- **Author:** Moose
- **Session:** S119
- **Date:** 2026-07-18
- **Module version:** Terminus (docgen engine)
- **Estimated total:** ~26h autonomous agent work
- **North-Star layer:** kernel (Terminus internal doc engine; not a user-facing module)
- **Context:** The S119 DLAND landing-hierarchy work made top-level READMEs concise but
  over-corrected: bare ~50-line landings that are all chrome, thin/near-empty sub-pages,
  an identical 3-box design diagram on every repo, and wrong repo identity (Terminus's
  tagline latched onto "docgen_backfill tool", the last feature the single generation prompt
  saw). Root cause: one thin generation prompt fed only the feat diff + hardcoded chrome
  (`shields_badges`/`why_bullets`/`default_architecture_mermaid_source`). This spec replaces
  that with a 5-pass, KG-grounded generator (design: Fable, `fable-docgen-redesign.md`):
  a deterministic RepoFacts layer (subsystem rollup + call-edge matrix from the Atlas graph),
  a structured identity pass, per-subsystem reference pages, a derived architecture diagram,
  and a deterministically-assembled landing that cannot latch or emit generic chrome by
  construction. All existing invariants (place_docs sole writer, no forge calls, PII sweep on
  every LLM input, infallible trigger, no-loss preservation) are preserved.

## Pre-flight
- Repository: `moosenet/Terminus` on Gitea, on `main`.
- Build/test-gate: via the Terminus compiler tool (`compiler_build(module=terminus, ref=<branch>, mode=test, host=heavy)`) — single build door, never ad-hoc cargo on a shared host.
- KG grounding: the Atlas per-project graphs (TERM/CHRD/LUM/HARM/MUSE) are populated and served from the terminus host via `crate::scribe::graph` (same store `SCRIBE_KG_STORE_DIR`).
- Reviews: through `review_run` (single review door), default panel `[codex, agy, free]` — avoid `agy` if quota-exhausted, substitute `opus`.
- Baseline: existing docgen tests pass on main; DLAND-01..05 + DLAND-RELOC merged.

---

### DGRICH-01: RepoFacts deterministic grounding layer
- **Priority:** High
- **Labels:** terminus, docgen, kg
- **Agent:** claude
- **Estimate:** 4h
- **Description:** New module `src/tools/docgen/repo_facts.rs` implementing Pass 0 of the design
  (§2): a deterministic `RepoFacts` builder that derives the repo's real structure from the
  Atlas code knowledge graph plus a checkout scan — with zero LLM calls. This is the grounding
  foundation every later pass consumes.

  ## FILES
  - `src/tools/docgen/repo_facts.rs` — **new**: `RepoFacts` struct + builder.
  - `src/tools/docgen/mod.rs` — register the new module.

  ## APPROACH
  1. Define `RepoFacts` with: `kg_grounded: bool`; scale/hotspots (node+edge counts, `by_kind`
     map, top-20 PageRank hotspots); `subsystems: Vec<Subsystem>` (name, source dir, node count,
     kind breakdown, top-8 symbols by PageRank); `edge_matrix` (weighted directed
     subsystem→subsystem call counts); `entry_points` (Cargo.toml `[[bin]]` targets + workspace
     members + `register_all`/`serve`/`main`-shaped symbols); `config_surface` (env-var accessor
     fn names from `config.rs`-shaped modules — NAMES only, never values); `prose_anchors`
     (Cargo.toml `description`, crate-root `//!` docs, per-subsystem module `//!` docs, ~40 lines
     each); `old_readme_sections` (via existing `preserve::split_old_sections`, labeled
     legacy-claims-to-verify).
  2. Call the graph NATIVELY through `crate::scribe::graph::{store, query, community, rank}` —
     the same functions the MCP `kg_*` tools wrap — NOT over HTTP/MCP. Take an injectable graph
     handle so the builder is unit-testable with a fixture graph. Keep the builder a pure
     function of (graph handle, checkout path, project_id).
  3. Subsystem rollup: group nodes by top-level path prefix (`crate::<mod>`; `<pkg>::src` for TS
     trees). Selection rule: keep prefixes with ≥ max(30 nodes, 1% of repo nodes); cap at 16,
     ranked by node count × aggregate PageRank; fold the remainder into a synthetic `misc`
     subsystem inventory.
  4. Edge matrix: iterate all `calls` edges, count cross-prefix pairs into a weighted directed
     map. This is the architecture diagram's source (DGRICH-04 consumes it).
  5. Checkout scan for entry points / config surface / prose anchors uses the existing
     `scribe::inspect` worktree helpers where available; read files repo-relative.
  6. PII sweep: RepoFacts contains code identifiers + paths — run the whole struct through the
     existing `sweep_input`/`SweptFeatContext` gate before it is serialized into any slice (S1
     placeholder rules apply — this content can reach a mirror). Never emit a raw config VALUE.
  7. Per-pass slice serialization: `identity_slice()` (~6–8 KB: scale + rollup + entry points +
     prose anchors + legacy headings), `subsystem_slice(name)` (~4–6 KB: that subsystem's top
     symbols + signatures + in/out neighbors + module docs + mapped legacy section).
  8. Fallback: KG `found:false` → degrade to items 4–7 (Cargo/module-doc/README), set
     `kg_grounded=false`; downstream omits KG-derived numbers rather than inventing them.

  ## TEST PLAN
  - Build via `compiler_build(module=terminus, ref, mode=test, host=heavy)`.
  - Unit: fixture graph with 3 subsystems → rollup keeps the ≥threshold ones, folds the rest to
    `misc`, edge matrix counts cross-prefix calls correctly.
  - Unit: `kg_grounded=false` path returns Cargo/README-only facts, no fabricated counts.
  - Unit: PII sweep runs — a fixture prose anchor containing a private IP is placeholdered.
  - Verify no hardcoded infra values / no `std::env::var` for secrets in new code.

  ## EDGE CASES
  - Repo with no KG entry (`found:false`) — degrade, never panic.
  - A subsystem with 0 `calls` edges — appears in rollup, absent from edge matrix.
  - TS/mixed-language trees (TERM's `constellation-web`) — prefix grouping handles `<pkg>::src`.
  - Empty/missing old README — `old_readme_sections` empty, not an error.

- **Acceptance criteria:**
  - [ ] `RepoFacts` builds from a native `crate::scribe::graph` handle (no MCP/HTTP hop)
  - [ ] Subsystem selection matches the §1.2 rule (≥max(30,1%), cap 16, remainder→misc)
  - [ ] Edge matrix aggregates `calls` edges into cross-subsystem weights
  - [ ] `kg_grounded:false` degradation path fabricates no numbers
  - [ ] All RepoFacts content passes through the PII sweep before slice serialization
  - [ ] Builder is pure + unit-tested with an injected fixture graph
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

---

### DGRICH-02: The three grounding prompts + parsers + lints
- **Priority:** High
- **Labels:** terminus, docgen, prompts
- **Agent:** claude
- **Estimate:** 3h
- **Description:** New module `src/tools/docgen/prompts.rs` with the three prompt builders
  (§3.1–3.3), their output parsers (strict JSON for identity; `=== FILE: <path> ===` splitter
  for guides), and the deterministic validation lints. `build_docs_prompt` in
  `src/review/prompt.rs` is kept but gains a deprecation note; docgen stops calling it for
  repo-level runs.

  ## FILES
  - `src/tools/docgen/prompts.rs` — **new**: prompt builders, parsers, lints.
  - `src/review/prompt.rs` — add deprecation note on `build_docs_prompt`.
  - `src/tools/docgen/mod.rs` — register module.

  ## APPROACH
  1. `build_repo_identity_prompt(repo_name, git_ref, facts_json)` verbatim per §3.1 (JSON output:
     tagline ≤120 chars, what_is 2–3 paras, audience, subsystems[], feature_rows 5–12,
     guide_topics 2–6; HARD RULES incl. anti-latch #1 and the banned-adjective list).
  2. `build_subsystem_page_prompt(repo_name, subsystem, identity_json, slice_json)` per §3.2
     (markdown page 60–200 lines).
  3. `build_guides_prompt(repo_name, identity_json, entrypoints_json, legacy_usage)` per §3.3.
  4. Parsers: `parse_repo_identity(&str) -> Result<RepoIdentity>` (strict serde, tolerant of a
     stray code fence); `parse_file_blocks(&str) -> Vec<(PathBuf, String)>` splitting on the
     exact `=== FILE: <path> ===` marker.
  5. Lints (deterministic, each returns a violation reason): **anti-latch** — tagline/what_is must
     not be dominated by any single subsystem's vocabulary AND must share no distinctive n-grams
     with the feat context (belt-and-suspenders; identity pass never sees the diff);
     **symbol-existence** — every symbol named in identity/pages exists in RepoFacts;
     **honest-command** — every command in guides names a real `[[bin]]`/tool from RepoFacts.
  6. `RepoIdentity` type lives here (or a shared `types` module) so DGRICH-03/05/06 consume it.

  ## TEST PLAN
  - Build via compiler tool test mode.
  - Unit: parse a well-formed identity JSON; reject one with a missing subsystem one-liner.
  - Unit: `=== FILE:` splitter yields the right paths+bodies incl. a 2-file guide output.
  - Unit: anti-latch lint FAILS a tagline that repeats one subsystem's vocabulary; PASSES a
    balanced hub tagline.
  - Unit: symbol-existence lint fails on an invented API name.
  - Verify no hardcoded infra values.

  ## EDGE CASES
  - LLM wraps JSON in a ```json fence — parser tolerates.
  - Guide output missing the FILE marker — parser returns empty, caller retries/flags.
  - A lint input with empty feat context — anti-latch n-gram check is a no-op, not a crash.

- **Acceptance criteria:**
  - [ ] Three prompt builders match §3.1–3.3 (rules + banned adjectives present)
  - [ ] Strict identity JSON parser + `=== FILE:` splitter, both unit-tested
  - [ ] Anti-latch, symbol-existence, honest-command lints implemented + tested
  - [ ] `build_docs_prompt` retained with a deprecation note; not called on the repo-level path
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

---

### DGRICH-03: generate_repo_docs orchestration (Passes 1–3)
- **Priority:** High
- **Labels:** terminus, docgen
- **Agent:** claude
- **Estimate:** 3h
- **Blocked by:** DGRICH-01, DGRICH-02
- **Description:** Add `generate_repo_docs(generator, facts, …) -> RepoDocsOutcome` to
  `src/tools/docgen/generate.rs`, orchestrating Pass 1 (identity), Pass 2 (per-subsystem pages,
  parallelizable, N≤16), Pass 3 (guides + getting-started) over the EXISTING `DocGenerator`
  Chord seam (unchanged). Per-pass retry-once-then-`Flagged`; partial success is usable.

  ## FILES
  - `src/tools/docgen/generate.rs` — add `generate_repo_docs` + `RepoDocsOutcome`.

  ## APPROACH
  1. `RepoDocsOutcome { identity: RepoIdentity, subsystem_pages: Vec<(String, String)>,
     guides: Vec<(PathBuf, String)>, getting_started: String, missing: Vec<String>,
     pass_ledger: Vec<PassRecord> }`.
  2. Pass 1: build identity prompt from `facts.identity_slice()`, call the generator, parse +
     lint; on lint/parse failure retry ONCE with the violation quoted; second failure →
     `Flagged` (return with `missing` populated, do not abort).
  3. Pass 2: for each kept subsystem, build the page prompt from `facts.subsystem_slice(name)` +
     identity; run concurrently (bounded); each page validated (symbol-existence). A failed page
     is listed in `missing`, others still returned — identity ok + 12/15 pages = usable.
  4. Pass 3: guides + getting-started from identity + entry points + config surface + legacy
     usage; honest-command lint; retry-once-then-flag.
  5. Keep the existing `generate_docs` (legacy per-module path) intact.
  6. Every LLM input already PII-swept in RepoFacts; do not re-sweep, do not bypass.

  ## TEST PLAN
  - Build via compiler tool test mode.
  - Unit with a stub `DocGenerator` returning canned outputs: full success populates identity +
    N pages + guides, empty `missing`.
  - Unit: a stub that returns invalid identity JSON twice → outcome `Flagged`, `missing` names
    the pass, no panic.
  - Unit: one subsystem page fails validation twice → that page in `missing`, others present.
  - Verify no hardcoded infra values.

  ## EDGE CASES
  - Generator (Chord) unreachable — surfaced as a pass failure/flag, never an `Err` that fails the feat.
  - Zero kept subsystems (tiny repo) — identity + guides only, no pages, not an error.
  - N=16 pages — concurrency bounded, all attempted.

- **Acceptance criteria:**
  - [ ] `generate_repo_docs` orchestrates Passes 1–3 over the unchanged `DocGenerator` seam
  - [ ] Per-pass retry-once-then-`Flagged`; partial success returns usable output + `missing`
  - [ ] Pass 2 runs per-subsystem, bounded-concurrent, each validated
  - [ ] `pass_ledger` records each pass outcome for operator visibility
  - [ ] Legacy `generate_docs` unchanged
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

---

### DGRICH-04: Derived architecture diagram (Pass 4) + generic lint
- **Priority:** High
- **Labels:** terminus, docgen, diagram
- **Agent:** claude
- **Estimate:** 2h
- **Blocked by:** DGRICH-01
- **Description:** Deterministic mermaid architecture diagram synthesized from the RepoFacts
  edge matrix (§2 Pass 4) in `src/tools/docgen/diagram.rs`, plus an `is_generic_placeholder`
  lint. `default_architecture_mermaid_source` is demoted to the no-KG last resort.

  ## FILES
  - `src/tools/docgen/diagram.rs` — add `subsystem_architecture_mermaid_source` + `is_generic_placeholder`.

  ## APPROACH
  1. `subsystem_architecture_mermaid_source(&SubsystemGraph) -> String`: nodes = top ≤10
     subsystems by weight (rest folded into a `…` node); edges where cross-call weight ≥
     max(5, p75 of nonzero weights); edge label = call count; node label = `name (n symbols)`;
     entry-point subsystems (reached from `bin`/server mains) placed leftmost; emit
     `flowchart LR`. A fuller ≤16-node variant for `docs/architecture.md`.
  2. Sweep the emitted source through the existing `SweptDiagramSource` gate.
  3. `is_generic_placeholder(&str) -> bool`: true if the diagram is the `Client`/`Core`/`Output`
     template or has <5 real subsystem nodes — the landing gate (DGRICH-09) uses it so a generic
     diagram can never ship silently.
  4. `default_architecture_mermaid_source` retained ONLY as the `kg_grounded:false` fallback.

  ## TEST PLAN
  - Build via compiler tool test mode.
  - Unit: a fixture edge matrix (TERM-shaped: intake/forge/tools/scribe/mesh) yields ≥5 nodes
    with real names, weighted edges, entry-point subsystem leftmost.
  - Unit: `is_generic_placeholder` returns true for the `Client→Core→Output` template and for a
    <5-node diagram; false for a real 6-node one.
  - Unit: diagram source is swept (a fixture with a private IP in a node label is placeholdered).
  - Verify no hardcoded infra values.

  ## EDGE CASES
  - All edge weights below threshold — keep the top edges by weight so the diagram is never empty.
  - Single-subsystem repo — falls back to the default (generic) source, flagged by the lint.
  - >16 subsystems — folded into `…`, node cap respected.

- **Acceptance criteria:**
  - [ ] `subsystem_architecture_mermaid_source` derives nodes/edges from the edge matrix per §2 Pass 4
  - [ ] Entry-point subsystems rendered leftmost; edge labels are real call counts
  - [ ] Diagram source passes through `SweptDiagramSource`
  - [ ] `is_generic_placeholder` detects the template + sub-5-node diagrams, unit-tested
  - [ ] `default_architecture_mermaid_source` demoted to no-KG fallback only
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

---

### DGRICH-05: Landing assembly — kill chrome, add fact row + substance floor
- **Priority:** High
- **Labels:** terminus, docgen, readme
- **Agent:** claude
- **Estimate:** 3h
- **Blocked by:** DGRICH-01, DGRICH-02, DGRICH-04
- **Description:** Rework `src/tools/docgen/readme_layers.rs` (§1.1, §4): delete the hardcoded
  chrome, add a computed fact row, assemble the landing deterministically from `RepoIdentity` +
  `RepoFacts` + the emitted docs tree, raise the cap to 300 lines and add an ≥80 substantive-line
  floor. Deterministic assembly is what makes latching/generic-chrome/dangling-links
  structurally impossible.

  ## FILES
  - `src/tools/docgen/readme_layers.rs` — delete chrome; add `fact_row`, `build_landing_body`,
    `check_landing_substance`; retarget `documentation_nav_table`.

  ## APPROACH
  1. DELETE `shields_badges`, `why_bullets`, `architecture_glance` (the fake-badge/generic chrome).
  2. `fact_row(&RepoFacts) -> String`: one plain theme-safe line, all numbers computed — e.g.
     `Rust · 410 modules · 53 MCP tools · 11.9k KG nodes · analyzed <sha>`. Omits KG-derived
     numbers when `kg_grounded:false`.
  3. `build_landing_body(&RepoIdentity, &RepoFacts, &[DocsTreeFile]) -> String` assembling the §1.1
     8-section skeleton (hero+tagline+fact row; What is; Architecture diagram; Subsystems/Features
     table linking each row to its `reference/<subsystem>.md`; Quick Start; Documentation index
     from the ACTUAL emitted tree with each page's real first-paragraph one-liner; At a glance;
     Contributing/License). Keep the centered `<h1>` hero (renders on Gitea) but every value is
     repo-derived.
  4. `documentation_nav_table` now takes the emitted tree + per-page descriptions (replaces the
     fixed 5-row table).
  5. `LANDING_MAX_LINES = 300` (was 180); new `LANDING_MIN_SUBSTANTIVE_LINES = 80` +
     `check_landing_substance` counting non-blank/non-chrome lines (excludes hero HTML + rules) —
     a landing below the floor is a gate failure exactly like one above the cap.
  6. KEEP `parse_layers`/`deepen_layers` for round-trip preservation of operator hand-edits.

  ## TEST PLAN
  - Build via compiler tool test mode.
  - Unit: `fact_row` computes counts from a fixture RepoFacts; omits KG numbers when ungrounded.
  - Unit: `build_landing_body` emits all 8 sections; every feature row links to an emitted page;
    the doc index rows match the emitted tree.
  - Unit: `check_landing_substance` fails a <80-substantive-line landing and a >300-line one; passes ~180.
  - Verify no hardcoded infra values.

  ## EDGE CASES
  - `kg_grounded:false` — fact row + At-a-glance omit KG counts, no fabricated numbers.
  - Emitted tree with 0 reference pages (degraded) — index still renders guides/getting-started.
  - Operator hand-edited landing — `parse_layers` round-trip preserved.

- **Acceptance criteria:**
  - [ ] `shields_badges`/`why_bullets`/`architecture_glance` deleted (no shields.io URLs emitted)
  - [ ] `fact_row` computes all numbers; omits KG-derived ones when ungrounded
  - [ ] `build_landing_body` assembles the §1.1 8-section skeleton, all values repo-derived
  - [ ] `LANDING_MAX_LINES=300` + `check_landing_substance` (≥80) both enforced fail-closed
  - [ ] Doc index + feature rows link to actually-emitted pages
  - [ ] README updated to document the new landing structure (this is the doc engine's own output contract)
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

---

### DGRICH-06: Per-subsystem docs tree render
- **Priority:** High
- **Labels:** terminus, docgen, render
- **Agent:** claude
- **Estimate:** 3h
- **Blocked by:** DGRICH-03
- **Description:** Rework `src/tools/docgen/render/docs_tree.rs` (§1.2) so `build_docs_tree`
  accepts the per-subsystem pages + guides and emits the KG-derived tree: `reference/<subsystem>.md`
  set, populated `configuration.md`/`cli.md` from the facts surface (no longer stubs), `legacy/`
  passthrough, and an index with real per-page descriptions. Diátaxis spine, breadcrumbs,
  `_Sidebar.md`, and cross-links preserved.

  ## FILES
  - `src/tools/docgen/render/docs_tree.rs` — accept per-subsystem pages + guides; emit the tree.

  ## APPROACH
  1. `build_docs_tree` takes `RepoDocsOutcome` + `RepoFacts`: writes `index.md` (hub one-liner +
     full nav with per-page descriptions), `getting-started.md`, `architecture.md` (full diagram +
     per-subsystem narrative), `guides/index.md` + each guide, `reference/index.md` (subsystem
     inventory table), one `reference/<subsystem>.md` per generated page, `reference/configuration.md`
     (from `config_surface` — NAMES only), `reference/cli.md` (from `[[bin]]` targets).
  2. `legacy/` passthrough wired for DGRICH-08 (backstop pages) — links from `reference/index.md`.
  3. Preserve existing breadcrumbs, `_Sidebar.md`, and cross-link generation.
  4. Per-page description = the page's real first paragraph (used by the landing/index).

  ## TEST PLAN
  - Build via compiler tool test mode.
  - Unit: a fixture `RepoDocsOutcome` with 3 subsystem pages + 2 guides → tree contains
    `reference/{a,b,c}.md`, `guides/*`, populated `configuration.md`/`cli.md`, index with descriptions.
  - Unit: `configuration.md` lists key NAMES only, no values.
  - Unit: breadcrumbs + `_Sidebar.md` still generated.
  - Verify no hardcoded infra values.

  ## EDGE CASES
  - Zero reference pages (degraded outcome) — tree still emits index/getting-started/guides.
  - Empty config surface — `configuration.md` says so honestly, not a broken stub.
  - `legacy/` empty (ideal) — no legacy section links.

- **Acceptance criteria:**
  - [ ] `build_docs_tree` emits one `reference/<subsystem>.md` per generated page
  - [ ] `configuration.md`/`cli.md` populated from the facts surface (names only), not stubs
  - [ ] Index carries real per-page descriptions; `legacy/` passthrough wired
  - [ ] Diátaxis spine, breadcrumbs, `_Sidebar.md`, cross-links preserved
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

---

### DGRICH-07: Trigger repo-level mode
- **Priority:** High
- **Labels:** terminus, docgen, trigger
- **Agent:** claude
- **Estimate:** 2h
- **Blocked by:** DGRICH-03, DGRICH-05, DGRICH-06
- **Description:** `run_docgen_trigger` in `src/tools/docgen/trigger.rs` gains the repo-level mode:
  when the project has a KG (or `target_root` gives a full checkout), build RepoFacts →
  `generate_repo_docs` → assemble landing (DGRICH-05) → build docs tree (DGRICH-06) → existing
  place/no-loss/PII/versioning machinery; else the legacy per-module path. Still structurally
  infallible. Call-site signatures (`docgen_run` tool, capstone hook) unchanged.

  ## FILES
  - `src/tools/docgen/trigger.rs` — repo-level branch + pass ledger on `TriggerOutcome::Completed`.

  ## APPROACH
  1. Detect repo-level mode: project has a KG for `project_id`, or `target_root` is a full checkout.
  2. Repo-level flow: RepoFacts (DGRICH-01) → `generate_repo_docs` (DGRICH-03) → `build_landing_body`
     (DGRICH-05) → `build_docs_tree` (DGRICH-06) → reuse existing `place_docs` (sole writer),
     `check_preservation`, PII gate, `VersionStore`.
  3. `TriggerOutcome::Completed` gains the pass ledger (per-pass outcomes) for operator visibility.
  4. Preserve infallibility — every internal failure folds into a normal `Completed{flagged}` /
     `Failed` response value, never an `Err` that fails the feat. Signatures of `docgen_run` /
     capstone hook unchanged.
  5. Legacy path retained for projects without a KG/checkout.

  ## TEST PLAN
  - Build via compiler tool test mode.
  - Unit: repo-level mode selected when a fixture KG is present; legacy mode otherwise.
  - Unit: a forced internal failure yields a `Completed{flagged}`/`Failed` value, never a panic/`Err`.
  - Unit: pass ledger present on `Completed`.
  - Verify no hardcoded infra values.

  ## EDGE CASES
  - KG present but generation flagged — still places what succeeded, ledger shows the gap.
  - No KG + no checkout — legacy path, unchanged behavior.
  - Concurrent trigger for the same project — existing locking respected.

- **Acceptance criteria:**
  - [ ] Repo-level mode wired end-to-end (RepoFacts→generate→landing→tree→place)
  - [ ] `run_docgen_trigger` remains structurally infallible
  - [ ] `docgen_run` tool + capstone hook call-site signatures unchanged
  - [ ] Pass ledger surfaced on `TriggerOutcome::Completed`
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

---

### DGRICH-08: Invert backfill — rich pipeline primary, verbatim relocation as backstop
- **Priority:** High
- **Labels:** terminus, docgen, backfill
- **Agent:** claude
- **Estimate:** 3h
- **Blocked by:** DGRICH-07
- **Description:** Rework `src/tools/docgen/backfill.rs` (§5): `backfill_readme` runs the FULL rich
  pipeline with the old README wired in as Pass-0 input #7. Then `check_preservation(old, landing +
  docs tree union)`: any old section whose substance is NOT covered is relocated VERBATIM to
  `docs/legacy/<slug>.md` and linked from the index. No-loss stays true by construction — but by
  exception (backstop), not as the entire output.

  ## FILES
  - `src/tools/docgen/backfill.rs` — invert relocation to backstop-only; report new counts.

  ## APPROACH
  1. `backfill_readme` calls the repo-level trigger path (rich pipeline), old README fed as
     RepoFacts input #7 (legacy claims to verify).
  2. After generation, run `check_preservation(old, union(landing, docs_tree))`. For each old
     section whose substance is uncovered, relocate it VERBATIM to `docs/legacy/<slug>.md`
     (reuse existing `old_readme_parts`/slugging) and link from `reference/index.md`.
  3. `BackfillReport` gains `covered_by_generation` vs `relocated_to_legacy` counts so the
     operator sees exactly what the model absorbed vs. what fell back.
  4. Coverage must remain 1.0 (no-loss) — every old section is either covered or relocated verbatim.

  ## TEST PLAN
  - Build via compiler tool test mode.
  - Unit: an old README whose sections are all covered by generation → `relocated_to_legacy=0`,
    coverage 1.0, no `legacy/` pages.
  - Unit: an old section with unique substance not regenerated → relocated verbatim to
    `legacy/<slug>.md`, linked from index, coverage stays 1.0.
  - Unit: report surfaces both counts.
  - Verify no hardcoded infra values.

  ## EDGE CASES
  - Old README empty — no legacy pages, coverage trivially 1.0.
  - Generation flagged/partial — uncovered sections all relocate (safe fallback → today's behavior).
  - A section partially covered — treat as uncovered (relocate) to guarantee no-loss.

- **Acceptance criteria:**
  - [ ] `backfill_readme` runs the rich pipeline primary; old README is Pass-0 input, not the product
  - [ ] Uncovered old sections relocated VERBATIM to `docs/legacy/<slug>.md`, linked from index
  - [ ] `check_preservation` coverage stays 1.0 for backfilled repos
  - [ ] Report surfaces `covered_by_generation` vs `relocated_to_legacy`
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

---

### DGRICH-09: Config options, docgen_facts preview tool, gate wiring
- **Priority:** Medium
- **Labels:** terminus, docgen, config
- **Agent:** claude
- **Estimate:** 2h
- **Blocked by:** DGRICH-04, DGRICH-05, DGRICH-07
- **Description:** Per-project options in `src/tools/docgen/config.rs`; a new read-only
  `docgen_facts` preview tool registered in `mod.rs`; the new gates (generic-diagram, substance
  floor, identity lint) wired into `quality.rs` + `place_docs`.

  ## FILES
  - `src/tools/docgen/config.rs` — `subsystem_page_cap`, `landing_budget`, optional `identity_hint`.
  - `src/tools/docgen/mod.rs` — register `docgen_facts`; updated `docgen_run`/`docgen_backfill` schemas.
  - `src/tools/docgen/quality.rs` — wire generic-diagram + substance-floor + identity lints.

  ## APPROACH
  1. `ProjectDocConfig` gains `subsystem_page_cap` (default 16), `landing_budget` (default 300),
     optional operator `identity_hint` (wins over Pass 1 tagline when present).
  2. New read-only `docgen_facts` tool: dry-run RepoFacts for a project so an operator can
     sanity-check grounding (subsystem rollup, counts, entry points) BEFORE a backfill. No writes.
  3. Wire gates into `quality.rs` and the `place_docs` fail-closed set: `is_generic_placeholder`
     (DGRICH-04), `check_landing_substance` (DGRICH-05), identity lint (DGRICH-02).
  4. Update `docgen_run`/`docgen_backfill` MCP schemas for any new optional params.

  ## TEST PLAN
  - Build via compiler tool test mode.
  - Unit: `identity_hint` overrides the tagline when set.
  - Unit: `docgen_facts` returns a facts summary and performs no writes.
  - Unit: a generic diagram / sub-floor landing / invented-symbol identity each fail the wired gate.
  - Verify no hardcoded infra values.

  ## EDGE CASES
  - Missing config — defaults (cap 16, budget 300) apply.
  - `docgen_facts` on a project with no KG — returns `kg_grounded:false` facts, no error.

- **Acceptance criteria:**
  - [ ] `subsystem_page_cap`/`landing_budget`/`identity_hint` config options added with defaults
  - [ ] `docgen_facts` read-only preview tool registered; performs no writes
  - [ ] Generic-diagram, substance-floor, identity lints wired into quality/place_docs (fail-closed)
  - [ ] `docgen_run`/`docgen_backfill` schemas updated
  - [ ] README updated to document the new `docgen_facts` tool
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

---

### DGRICH-10: Rollout — re-backfill TERM, CHRD, LUM, HARM, MUSE
- **Priority:** High
- **Labels:** terminus, docgen, rollout
- **Agent:** claude
- **Estimate:** 1h
- **Blocked by:** DGRICH-08, DGRICH-09
- **Type:** human-action-assisted
- **Description:** After the engine is merged + deployed, re-run `docgen_backfill` for all five
  repos with the rich generator. TERM first as the reference case (compare against the §6 worked
  example before the other four run), then CHRD, LUM, HARM, MUSE. Each repo's regenerated docs go
  through its own reviewed worktree/PR on Gitea and, for `mirror_ready` repos, the GitHub mirror.

  ## APPROACH
  1. Deploy the merged engine to terminus-primary (`constellation-update.sh --force --skip-idle terminus-primary`).
  2. `docgen_facts` each repo first to confirm grounding; then `docgen_backfill` TERM.
  3. Compare TERM landing against §6 acceptance (120–300 lines, hub tagline, ≥5-node diagram,
     ≥14 reference pages, ≥10 linked feature rows).
  4. Run CHRD, LUM, HARM, MUSE; commit each via its own PR; update Gitea + GitHub mirrors.
  5. Verify acceptance criteria §7 (1–6) hold per repo; automated TERM-vs-CHRD diff check (no
     repeated sentences).

  ## TEST PLAN
  - Per repo: landing line count in [120,300]; diagram ≥5 real nodes, no Client/Core/Output;
    `reference/` page count matches subsystem count; `check_preservation` coverage 1.0.
  - TERM vs CHRD: taglines/diagrams/feature tables share no repeated sentences.

  ## EDGE CASES
  - A repo with no KG — degrades gracefully (`kg_grounded:false`), still improved over the DLAND output.
  - Mirror withheld on residual PII — internal Gitea update still lands; surface the mirror block.

- **Acceptance criteria:**
  - [ ] Engine deployed to terminus-primary
  - [ ] All five repos re-backfilled with the rich generator, each via its own reviewed PR
  - [ ] TERM matches the §6/§7 acceptance (hub tagline, real diagram, per-subsystem pages)
  - [ ] TERM-vs-CHRD distinctness check passes (no repeated sentences)
  - [ ] Gitea + GitHub mirrors updated for `mirror_ready` repos
  - [ ] No-loss (`check_preservation` 1.0) holds for every repo
