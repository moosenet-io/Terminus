# Doc engine: concise Hermes-style landing + no-loss hierarchy cutover
plane_project: TERM
module: Terminus
prefix: DLAND
spec_id: S119-docgen-landing-hierarchy

## Metadata
- **Author:** <operator> (Moose)
- **Session:** S119
- **Date:** 2026-07-17
- **Module version:** terminus-rs (docgen S95 line, DOCGEN-01..22 + DOCGEN-21 concise-landing revision)
- **Estimated total:** ~19h autonomous agent work
- **Context:** The doc engine already KNOWS how to produce a concise, hub-and-spoke README
  (DOCGEN-21 caps the landing at `LANDING_MAX_LINES = 180`, emits a `## Documentation` nav
  table linking OUT to a `docs/` sub-page tree, and pushes all deep-dive / Diataxis bodies
  into that tree — verified in `readme_layers.rs`). The problem is NOT the generator. Two
  gaps make the repos ship 1500–2200-line hand-grown READMEs instead:
  (1) **PLACEMENT** — `docgen_run` / `run_docgen_trigger` is artifacts-only by design
  ("Placement is the harness's job", enforced by the `run_never_touches_filesystem_or_repo`
  negative test). Nothing in the pipeline actually WRITES the concise landing + `docs/` tree
  over a repo's `README.md`, so the generated concise output never lands. The live
  `Terminus/README.md` is 2183 lines of pre-revision, hand-appended per-feature sections that
  the engine has never overwritten.
  (2) **NO-LOSS CUTOVER** — the existing bloated READMEs contain real, hand-curated content
  (e.g. Terminus's ~20 per-feature `##` sections). A naive first overwrite with the concise
  renderer would DELETE that. The operator's hard requirement is: **relocate into the
  hierarchy, never lose information.**
  North-star for "concise": the public `NousResearch/hermes-agent` top-level README (~200
  lines, hub-and-spoke: a short pitch + feature table + quick start + a Documentation index
  table that links every topic OUT to a dedicated doc page). Match that shape; keep every
  fact, but move detail into linked sub-pages.

## Pre-flight
- Repository: `moosenet/Terminus` on Gitea (docgen lives at `src/tools/docgen/`)
- Build/test host: <host> (Terminus's build+serve host) via the single build door
  (`compiler_build`, `mode=test`) — never an ad-hoc `cargo test` on a shared host
- Baseline tests: current `terminus-rs` workspace green on `main`
- Baseline behavior: docgen unit tests in `readme_layers.rs` / `render/docs_tree.rs` /
  `trigger.rs` all pass
- Design reference (read-only, external): `NousResearch/hermes-agent` README shape

---

### DLAND-01: Repo placement writer — land the concise README + docs/ tree onto disk
- **Priority:** High
- **Labels:** terminus, docgen, docs
- **Agent:** claude
- **Estimate:** 4h
- **Description:** Add the missing "harness places the artifacts" step as a first-class,
  testable Terminus function: given the concise landing markdown and the rendered `docs/`
  tree that the engine already returns in-memory, write them into a target repo working copy
  — `README.md` at the root and each `docs/**` file at its path — atomically and
  repo-relative. This is the piece that closes the "artifacts-only, never placed" gap; the
  ENGINE stays pure, this writer is the explicit, single, auditable impure boundary.

  ## FILES
  - `src/tools/docgen/place.rs` — NEW: `place_docs(target_root, landing, docs_tree) ->
    PlacementReport`. Pure-ish: takes an explicit target root + the already-rendered
    artifacts; writes `README.md` + `docs/**`; returns which paths were written/changed.
  - `src/tools/docgen/mod.rs` — register/export the new module; no behavior change to existing
    tools.
  - `src/tools/docgen/render/docs_tree.rs` — reuse its `DocsTreeFile` list as the input shape
    (do NOT re-derive paths; consume the existing `DOCS_*_PATH` constants).

  ## APPROACH
  1. Define `PlacementReport { written: Vec<String>, unchanged: Vec<String>, skipped: Vec<String> }`
     (repo-relative paths only).
  2. `place_docs` takes an explicit `target_root: &Path` (caller supplies it — the function
     never guesses a repo location), the landing `String`, and the `Vec<DocsTreeFile>` the
     renderer already returns. It creates parent dirs, writes each file via
     tempfile-plus-atomic-rename (never `cp`/truncate over a live file), and skips a write when
     content is byte-identical to what's on disk (so a no-op run produces an empty diff).
  3. Refuse to write outside `target_root` (path-traversal guard on every relative path) and
     refuse an absolute or `..`-escaping doc path — return it in `skipped` with a reason.
  4. Do NOT touch git here (no add/commit/push) — placement writes the working tree only; the
     pipeline's existing git stages own commits. Keep this function free of any network/forge
     calls.
  5. No hardcoded paths, hosts, or org names; `target_root` and all inputs are arguments.

  ## TEST PLAN
  - via `compiler_build(module=terminus, ref=<branch>, mode=test)`: `cargo test -p terminus-rs`
  - Unit: placing into a temp dir writes `README.md` + every `docs/**` file at the expected
    repo-relative path; `PlacementReport.written` lists them all.
  - Unit: a second identical `place_docs` call writes nothing (`written` empty, all
    `unchanged`) — idempotent, empty-diff on no change.
  - Unit (negative): a `docs/` entry with a `../escape` or absolute path is `skipped` with a
    reason and never written outside `target_root`.
  - Unit: an existing hand-written `README.md` in the temp dir is replaced atomically (old
    inode gone, new content present) — no partial/truncated intermediate file observable.
  - Verify no hardcoded IPs or org names in new/modified files.

  ## EDGE CASES
  - `target_root` does not exist / is a file → clear error, nothing written.
  - A `docs/` path whose parent dir doesn't exist yet → created, not an error.
  - Zero-length landing (engine returned `NoChange`/`Flagged`) → write nothing, return an empty
    report; never write an empty `README.md` over a real one.
  - Read-only target → surface the io error per-file in `skipped`, do not panic.

- **Acceptance criteria:**
  - [ ] `place_docs` writes the landing to `README.md` and each `docs/**` file at its
        repo-relative path under an explicit `target_root`
  - [ ] Writes are atomic (tempfile + rename) and idempotent (byte-identical → skipped)
  - [ ] Path-traversal / absolute-path doc entries are refused and reported, never written
  - [ ] The function touches no git, network, or forge surface
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] README updated to document the new placement step (see DLAND-06 for the fuller docs)
  - [ ] All existing tests still pass

---

### DLAND-02: No-loss cutover guard — every legacy section survives into the hierarchy
- **Priority:** Critical
- **Labels:** terminus, docgen, docs, safety
- **Agent:** claude
- **Estimate:** 5h
- **Description:** Before a first cutover REPLACES a hand-grown bloated `README.md`, prove no
  information is lost. Feed the existing README as generation context (the engine already
  supports `existing_docs` and is prompted to "deepen, not regenerate") AND add an explicit
  content-preservation check: every top-level (`##`) section and every heading in the OLD
  README must have its substance present somewhere in the NEW landing + `docs/` tree. If a
  section is not accounted for, the cutover is FLAGGED (not silently shipped). This is the
  guarantee behind "I am not asking to lose information."

  ## FILES
  - `src/tools/docgen/preserve.rs` — NEW: `check_preservation(old_readme, new_landing,
    new_docs_tree) -> PreservationReport` (covered headings, missing headings, coverage ratio).
  - `src/tools/docgen/quality.rs` — reuse existing heading/section extraction helpers if
    present rather than re-implementing markdown parsing.
  - `src/tools/docgen/generate.rs` — ensure the existing-README content is passed through as
    `existing_docs` on a cutover generation (confirm the call site threads it; add a
    regression test if it doesn't).

  ## APPROACH
  1. Parse the OLD README into a list of headings + their section bodies (reuse `quality.rs`
     extraction; do not hand-roll a second markdown splitter).
  2. Parse the NEW landing and every `docs/**` file into the same shape; build one combined
     corpus of new content.
  3. For each old heading, compute coverage: is the heading (normalized) OR a strong content
     signature of its body (e.g. key identifiers/tool names it mentions) present in the new
     corpus? Emit `covered` / `missing` per section and an overall `coverage_ratio`.
  4. `PreservationReport { covered: Vec<String>, missing: Vec<Section>, coverage_ratio: f32 }`.
     A run with any `missing` section is a FLAG the caller must surface — it does NOT auto-fail
     the build (docgen is non-blocking per the pipeline), but a cutover that would drop content
     must be withheld/escalated by DLAND-04's gate, not shipped.
  5. Content-signature matching must tolerate rewording (the concise landing paraphrases) —
     key on stable tokens (tool names, symbol names, env-var names, numbers) rather than exact
     prose, so paraphrase ≠ loss but a genuinely dropped feature ≠ covered.

  ## TEST PLAN
  - via `compiler_build(..., mode=test)`: `cargo test -p terminus-rs`
  - Unit: an old README whose every section's key tokens appear across the new landing+docs →
    `coverage_ratio == 1.0`, `missing` empty.
  - Unit (negative): delete one feature section's content from the new corpus → that section
    appears in `missing`, `coverage_ratio < 1.0`.
  - Unit: a section that was PARAPHRASED (same tool names, different prose) in a `docs/` page is
    `covered` — paraphrase is not counted as loss.
  - Unit: `generate.rs` threads the existing README into `existing_docs` on a cutover (assert
    the generator received it).
  - Verify no hardcoded IPs or org names in new/modified files.

  ## EDGE CASES
  - Old README with no `##` headings (just prose) → treat the whole body as one section.
  - Duplicate heading names in the old README → track by (heading, ordinal), not just text.
  - A section that is genuinely obsolete/removed on purpose → still reported `missing`; the
    operator decides in the DLAND-05 review, the tool never silently drops it.
  - Very large old README (2000+ lines) → bounded memory, streams sections.

- **Acceptance criteria:**
  - [ ] `check_preservation` reports covered vs missing sections and a coverage ratio
  - [ ] Paraphrased content (same key tokens) counts as covered; genuinely dropped content is
        reported missing (includes a negative test)
  - [ ] The cutover generation passes the existing README through as `existing_docs`
  - [ ] The report is data the caller surfaces — no silent drop of any old section
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass (pure-analysis item; no user-facing feature → README
        exempt, noted here)

---

### DLAND-03: Landing lint gate — enforce concise + resolvable links on what ships
- **Priority:** High
- **Labels:** terminus, docgen, docs
- **Agent:** claude
- **Estimate:** 3h
- **Description:** Turn the existing `check_landing_length` cap into a shippable gate and add a
  link-resolution check, so a README that is placed always obeys the Hermes-style contract:
  at/under the concise line cap AND every `## Documentation` nav-table link (and hero nav-row
  link) resolves to a `docs/` file that was actually generated. A landing that fails either
  check is not a valid placement.

  ## FILES
  - `src/tools/docgen/readme_layers.rs` — build on the existing `check_landing_length` /
    `LANDING_MAX_LINES` / `DOCS_*_PATH` constants; add `check_landing_links(landing,
    docs_tree)`.
  - `src/tools/docgen/place.rs` — DLAND-01's writer calls both checks before writing and
    returns their result in the `PlacementReport` (a failing gate → placement is refused, not
    a broken README on disk).

  ## APPROACH
  1. `check_landing_links` extracts every relative markdown link target from the landing
     (nav-row + `## Documentation` table + inline) and asserts each `docs/…` target is present
     in the `docs_tree` file set (reusing the shared `DOCS_*_PATH` constants so landing and
     tree can't drift).
  2. `place_docs` runs `check_landing_length` + `check_landing_links` first; on failure it
     writes NOTHING and returns the failure in the report (fail-closed — a bad landing never
     lands over a good README).
  3. Keep both checks pure functions returning `Result<(), Vec<String>>` so they're unit-
     testable and reusable by the backfill (DLAND-05) and any future pipeline gate.

  ## TEST PLAN
  - via `compiler_build(..., mode=test)`: `cargo test -p terminus-rs`
  - Unit: a 181-line landing fails the length gate with a clear message; a 180-line one passes.
  - Unit (negative): a landing linking `docs/missing.md` not in the tree fails the link gate
    naming the dangling target.
  - Unit: a valid landing + matching tree passes both; `place_docs` then writes.
  - Unit: a landing that fails a gate causes `place_docs` to write nothing (working tree
    unchanged).
  - Verify no hardcoded IPs or org names in new/modified files.

  ## EDGE CASES
  - External `http(s)://` links in the landing → ignored by the link check (only local
    `docs/` targets are resolved).
  - Anchor-only links (`#section`) → ignored / resolved against the same file.
  - A landing exactly at `LANDING_MAX_LINES` → passes (boundary inclusive).

- **Acceptance criteria:**
  - [ ] `check_landing_links` fails on any local doc link with no matching generated file
  - [ ] `place_docs` runs both gates first and refuses to write on failure (fail-closed)
  - [ ] Length gate boundary is correct (≤ cap passes, cap+1 fails)
  - [ ] Both checks are pure, reusable functions with a negative test each
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass (internal gate; no user-facing surface → README exempt,
        noted here)

---

### DLAND-04: Wire placement into the pipeline — capstone-APPROVE path places the docs
- **Priority:** High
- **Labels:** terminus, docgen, pipeline
- **Agent:** claude
- **Estimate:** 3h
- **Blocked by:** DLAND-01, DLAND-03
- **Description:** Connect the writer to where the engine already runs. Per v4.1 the doc engine
  fires once per build at the end of a PASSING Epic capstone (`review_run` structure `epic`,
  APPROVE → `docgen_run` → `scribe_docs`). Today that call returns artifacts and stops. Extend
  the capstone-APPROVE doc hook so, after generation, it PLACES the concise landing + `docs/`
  tree into the repo working copy via DLAND-01 (gated by DLAND-03), producing a normal working-
  tree change that the pipeline's existing git/commit path can carry — no new forge/git door.

  ## FILES
  - `src/tools/docgen/trigger.rs` — the `docgen_run` result already carries the rendered
    artifacts; add an explicit, opt-in `place: bool` + `target_root` param path so the capstone
    hook (or a caller) can request placement of the returned artifacts. Placement stays OFF by
    default (backward compatible: existing callers still get artifacts-only).
  - `src/review/…` (the capstone doc-hook call site that fires `docgen_run` on APPROVE) — pass
    `place=true` + the repo path so an APPROVE capstone lands the docs. Keep it non-blocking
    (a placement failure is logged/flagged, never reverts the merge or fails the build), exactly
    like the existing `docgen_run`/`scribe_docs` hook contract.

  ## APPROACH
  1. Add a placement step to `docgen_run`'s tool surface guarded by an explicit `place`+
     `target_root` (absent → today's artifacts-only behavior, unchanged — preserve the
     `run_never_touches_filesystem_or_repo` guarantee for the no-place path; add a parallel
     positive test for the place path).
  2. In the capstone-APPROVE hook, after a successful generation, call the placement (DLAND-01)
     with the guards (DLAND-03); fold any placement failure into the same non-blocking
     flagged-outcome shape the hook already uses.
  3. Do not place per-merge (docgen is capstone-gated per v4.1) — placement rides the same
     once-per-build APPROVE path as generation, so it isn't token-thrashed per item.
  4. No new Plane/Gitea/GitHub access; placement is a local working-tree write only.

  ## TEST PLAN
  - via `compiler_build(..., mode=test)`: `cargo test -p terminus-rs`
  - Unit: `docgen_run` WITHOUT `place` still touches no filesystem (existing guard test still
    passes unchanged).
  - Unit: `docgen_run` WITH `place`+`target_root` writes `README.md`+`docs/**` into a temp
    root and reports the placed paths.
  - Unit: a placement failure (e.g. failing landing gate) is returned as a flagged/failed
    outcome, NOT an error that would fail the feat (the non-blocking contract holds).
  - Verify no hardcoded IPs or org names in new/modified files.

  ## EDGE CASES
  - Capstone APPROVE but generation `NoChange`/`Flagged` → nothing to place, complete cleanly.
  - `place=true` but `target_root` missing/invalid → flagged outcome, merge untouched.
  - Concurrent capstone for the same repo → placement is idempotent (DLAND-01), safe.

- **Acceptance criteria:**
  - [ ] Default (`place` absent) `docgen_run` behavior is byte-for-byte unchanged (guard test
        still green)
  - [ ] With `place`+`target_root`, an APPROVE capstone lands the concise README + docs/ tree
  - [ ] Placement failure is non-blocking (flagged outcome, never reverts merge / fails build)
  - [ ] No new Plane/Gitea/GitHub access path; placement is a local write only
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] README updated to document the capstone-places-docs behavior (see DLAND-06)
  - [ ] All existing tests still pass

---

### DLAND-05: One-shot backfill — migrate the existing bloated READMEs, operator-reviewed
- **Priority:** High
- **Labels:** terminus, docgen, docs, migration
- **Agent:** claude
- **Estimate:** 3h
- **Blocked by:** DLAND-02, DLAND-03, DLAND-04
- **Description:** A tool that migrates an already-bloated repo (Terminus, Chord, Muse,
  lumina-constellation, …) from its hand-grown mega-README to the concise landing + `docs/`
  hierarchy in ONE guarded pass: generate against the existing README as context, run the
  no-loss guard (DLAND-02) and the landing gates (DLAND-03), and PRODUCE THE CHANGE FOR REVIEW
  rather than auto-shipping. First cutover per repo is operator-blessed (like the mirror
  bootstrap) — the tool writes to a working copy + emits a summary (line count before/after,
  covered vs missing sections, the `docs/` files created), and a human approves before it lands
  on `main` through the normal pipeline.

  ## FILES
  - `src/tools/docgen/backfill.rs` — NEW: `backfill_readme(target_root, project, git_ref)`
    orchestrating generate → preserve-check → landing gates → place-into-working-copy → summary.
  - `src/tools/docgen/mod.rs` — register a `docgen_backfill` tool exposing it.

  ## APPROACH
  1. Read the repo's current `README.md` (+ any existing `docs/`) as `existing_docs`; run the
     engine to produce the concise landing + Diataxis `docs/` tree.
  2. Run DLAND-02's preservation check. If any section is `missing`, DO NOT place — return the
     summary with the missing sections listed for the operator (the engine may need another
     pass, or the operator confirms the drop is intended).
  3. If preservation is clean AND the landing gates (DLAND-03) pass, place into the working copy
     (DLAND-01) and emit a summary: before/after line count, `coverage_ratio`, list of `docs/`
     files created, and the nav links.
  4. Never commit/push — hand the working-copy change to the operator + the normal
     worktree→review→merge pipeline. First cutover per repo is explicitly human-gated.
  5. Route any Plane/Gitea calls (if the tool reports status) through the Terminus tools — no
     raw API, no new door (S9).

  ## TEST PLAN
  - via `compiler_build(..., mode=test)`: `cargo test -p terminus-rs`
  - Unit: backfill against a synthetic 400-line README with 6 feature sections → produces a
    ≤180-line landing + a `docs/` tree, preservation clean, summary shows before/after counts.
  - Unit (negative): if the generated corpus drops a section, backfill returns
    `missing`-populated and places NOTHING (working copy untouched).
  - Unit: gates failing (over-length / dangling link) → no placement, summary explains why.
  - Verify no hardcoded IPs or org names in new/modified files.

  ## EDGE CASES
  - A repo already in concise form (no bloat) → `NoChange`, no-op, summary says so.
  - A repo with no `README.md` → treated as first-doc generation (no old sections to preserve).
  - Backfill must be re-runnable safely (idempotent placement via DLAND-01).

- **Acceptance criteria:**
  - [ ] `docgen_backfill` produces a concise landing + docs/ tree in a working copy, never
        auto-commits
  - [ ] It refuses to place when the no-loss guard reports missing sections (operator decides)
  - [ ] It emits a before/after summary (line count, coverage ratio, docs files created)
  - [ ] All Plane/Gitea/GitHub interaction (if any) goes through the Terminus tools, not a new
        API client
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] README updated to document the `docgen_backfill` tool (see DLAND-06)
  - [ ] All existing tests still pass

---

### DLAND-06: Operator review + first Terminus cutover (human-gated)
- **Priority:** Medium
- **Labels:** terminus, docgen, docs, human-action
- **Agent:** <operator>
- **Estimate:** 1h
- **Type:** human-action
- **Blocked by:** DLAND-05
- **Description:** Run `docgen_backfill` against Terminus (the worst offender, 2183 lines),
  review the produced concise landing + `docs/` tree and the no-loss summary, and bless the
  first cutover so it goes through the normal pipeline onto `main`. This is the one-time,
  per-repo operator gate (mirror-bootstrap analogue) that authorizes replacing a hand-grown
  README with the generated hierarchy.
- **Steps:**
  1. Run `docgen_backfill` for Terminus (via the docgen tool) against a fresh working copy.
  2. Review the summary: confirm `coverage_ratio` is 1.0 (or consciously accept any listed
     `missing` section as intentionally retired).
  3. Eyeball the concise landing against the Hermes yardstick (pitch + feature table + quick
     start + Documentation index) and click through the `docs/` links.
  4. If good, open the worktree→review→merge PR for the cutover; repeat per repo (Chord, Muse,
     lumina-constellation) once Terminus proves the flow.

---

## Out of scope (explicitly)
- Rewriting the generator / prompt to be "more concise" — it already targets ≤180 lines; the
  fix is placement + no-loss cutover, not generation.
- The standing Harmony direct-Plane-access violation and other unrelated pipeline gaps.
- Per-merge doc placement — docgen stays capstone-gated (v4.1); placement rides the same
  once-per-build APPROVE path.
