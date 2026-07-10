# Documentation Engine — Sovereign Post-Feat Doc Generation (replaces Mintlify)
plane_project: TERM
module: Terminus
prefix: DOCGEN
spec_id: S95-documentation-engine

## Metadata
- **Author:** the operator (Moose)
- **Session:** S95
- **Date:** (current)
- **Lumina version:** 1.0.0
- **Module version:** Terminus (doc engine tool) + Chord (SLM router capability) + build-skill trigger
- **Estimated total:** ~59h (10 items: 9 code + 1 doc)
- **Context:** A sovereign, in-house documentation/knowledge-base engine that **replaces Mintlify**. Triggered
  after every feat by the build skill, it reads WHAT WAS ACTUALLY BUILT (the merged diff + spec), deepens
  the project's documentation, and renders **variable output artifacts per project** (README, wiki, PDF,
  Notion/Obsidian notes, dev blog) as declared in that project's config. All inference routes through Chord,
  whose **SLM router** decides the destination (local high-context / OpenRouter frontier-free / other) — and
  a **test panel sweep** evaluates which SLM routes best. A **mandatory PII sweep gates the input before any
  inference**, so nothing unsanitized ever reaches a model (local or cloud); this also keeps published docs
  clean of infrastructure detail. **Version control is a core feature** — every generated artifact is
  versioned, diffable, and rollback-able, so regenerating after each feat never clobbers good docs.

  **This is one combined spec** (doc engine + Chord SLM router), per the operator's choice, accepting the layer
  coupling. Clean-ish seam preserved where possible: the doc engine ASKS Chord to route; Chord OWNS the
  routing decision (SLM router). The doc engine never picks a model itself.

  **Key boundaries:**
  - **Artifacts only, no hosted site.** The engine produces rendered artifacts; the CALLING HARNESS places
    them where they belong. The engine does not know/assume repo layout or hosting.
  - **Config-driven output.** Each project declares its doc targets; the engine renders exactly those.
  - **PII sweep is unconditional and gates the INPUT** — before content can reach Chord's router at all
    (because once the router picks "cloud," it's too late to sweep).

## Pre-flight
- Repositories: `terminus` (doc engine tool) + `chord` (SLM router capability). Build order below sequences them.
- Vault secrets (all via vault): `OPENROUTER_API_KEY` (frontier-free tier), Notion/Obsidian creds if used
  (`NOTION_TOKEN` etc.), any blog-target creds. Chord already holds local-model + cloud routing creds.
- Depends on: the existing PII gate/sweep implementation (reused, unconditional here); Chord's serving
  layer (S92) for local-model routing.
- Plane via the Terminus Plane tool; ingest into project TERM.
- Baseline tests: record terminus-rs + chord current counts — never regress.

---

## Design Overview (read before executing)

**Flow, end to end:**
1. **Trigger (build skill):** after a feat merges + verifies, the build skill invokes the doc engine with the
   feat's context (spec_id, merged diff, repo, project config).
2. **PII sweep (unconditional gate):** the diff/spec/code is swept BEFORE anything else. Content that fails
   is redacted or blocked; unsanitized content NEVER proceeds to inference. This gates the input regardless
   of where it will route.
3. **Doc generation request → Chord:** the (swept) context + the project's declared doc targets go to Chord.
4. **Chord SLM router:** a small router model decides the inference destination (local high-context /
   OpenRouter frontier-free / other) per Chord's policy. Chord executes the generation on the chosen model.
5. **Render per target:** the engine renders the returned content into each declared artifact format (README
   / wiki / PDF / Notion / Obsidian / blog).
6. **Version the artifacts:** each rendered artifact is versioned (tied to the triggering feat/commit),
   diffable against its prior version, rollback-able.
7. **Return artifacts to the caller:** the calling harness places them where they go. The engine's job ends
   at "here are the versioned artifacts."

**Router evaluation sweep (Part of this spec):** a test panel that measures candidate SLMs on routing
quality — did the router pick a destination that produced good docs at acceptable cost/latency? Same
measure-don't-assume discipline, a new profiling dimension, results tagged for comparison.

---

### DOCGEN-01: Doc engine scaffold + per-project doc-target config
- **Priority:** High
- **Labels:** terminus, docgen, scaffold, config
- **Agent:** claude
- **Estimate:** 6h
- **Description:** The doc engine tool skeleton in terminus-rs + the per-project config schema declaring each
  project's doc targets (which artifacts to produce, their formats/paths-relative-hints). No generation yet.

  ## FILES
  - `src/tools/docgen/mod.rs` — engine registration + core types
  - `src/tools/docgen/config.rs` — per-project doc-target config (targets: readme|wiki|pdf|notion|obsidian|blog, per-target options)
  - `.env.example` — doc-target + creds var NAMES

  ## APPROACH
  1. Define a per-project doc-target config (declared per repo/project): a list of targets, each with format
     + rendering options. Default: minimal (README only) if a project declares nothing.
  2. The engine reads a project's config to know what to produce — never guesses formats.
  3. Creds (OpenRouter, Notion, etc.) via vault; no literals.

  ## TEST PLAN
  - `cargo test --workspace`
  - Unit: config parses target lists; a project with no config → README-only default
  - Unit: unknown target type → clear error, not a crash (negative test)
  - Verify secrets via vault; no hardcoded infra

  ## EDGE CASES
  - Project declares a target needing missing creds (Notion, no token) → that target disabled with a hint, others proceed
  - Empty/malformed config → safe default (README), warn

  ## ACCEPTANCE CRITERIA
  - [ ] Per-project doc-target config schema (readme/wiki/pdf/notion/obsidian/blog + options)
  - [ ] No-config project → README-only default; unknown target → clear error (negative test)
  - [ ] No hardcoded infrastructure values; secrets via vault
  - [ ] All existing tests still pass

---

### DOCGEN-02: PII sweep gate on the input (unconditional, pre-inference)
- **Priority:** Critical
- **Labels:** terminus, docgen, pii, security
- **Agent:** claude
- **Estimate:** 6h
- **Description:** The load-bearing safety: sweep the feat's diff/spec/code BEFORE it can reach Chord/any
  inference. Unconditional — cloud or local, the input is swept first. Content failing the sweep is
  redacted or blocked; unsanitized content never proceeds. Doubles as keeping published docs infra-clean.

  ## FILES
  - `src/tools/docgen/pii_gate.rs` — the pre-inference sweep (reuses the fleet PII gate)
  - tests

  ## APPROACH
  1. Before ANY inference request is built, run the full PII sweep on the input (diff/spec/code): private
     IPs, `\bCT\d{3}\b`, internal hostnames, service names, etc.
  2. On a hit: REDACT the offending detail (so docs can still be generated about the feature without the
     infra specifics) OR block if redaction can't preserve meaning. Never pass unsanitized content onward.
  3. This gate runs regardless of routing destination — it gates the INPUT, before Chord's router even sees
     it (because once the router picks cloud, it's too late).
  4. Log what was redacted (sanitized meta, not the secret itself).

  ## TEST PLAN
  - `cargo test --workspace`
  - Unit: a diff containing CT###/IP/hostname → redacted before the inference request is built (negative
    test: unsanitized content NEVER reaches the request builder)
  - Unit: content that can't be safely redacted → blocked, not passed (negative test)
  - Unit: the gate runs for BOTH local and cloud routing (it's input-gating, destination-agnostic)
  - Verify redaction logged sanitized; no infra literals

  ## EDGE CASES
  - Infra detail intrinsic to the meaning → redact + note the doc will be generic there, don't leak
  - False-positive test placeholder (like the scanner.rs whitelisted case) → respect the established whitelist
  - Huge diff → sweep completely, don't sample (a missed chunk is a leak)

  ## ACCEPTANCE CRITERIA
  - [ ] Input PII sweep runs UNCONDITIONALLY before any inference request is built (negative test: nothing unsanitized reaches inference)
  - [ ] Hits are redacted (preferred) or blocked; never passed onward
  - [ ] Gate is destination-agnostic (runs for local AND cloud routing)
  - [ ] Redaction logged, sanitized
  - [ ] No hardcoded infrastructure values; secrets via vault
  - [ ] All existing tests still pass

---

### DOCGEN-03: Chord SLM router capability
- **Priority:** Critical
- **Labels:** chord, docgen, router, slm, inference
- **Agent:** claude
- **Estimate:** 7h
- **Description:** In CHORD: an SLM-router that, given a generation request, decides the inference
  destination (local high-context model / OpenRouter frontier-free / other) per policy, and executes on the
  chosen model. This is the "all inference through Chord, Chord decides" mechanism. Doc engine consumes it;
  Chord owns the decision.

  ## FILES
  - `chord/src/router/slm_router.rs` — the SLM-router: request → destination decision → execute
  - `chord/src/router/policy.rs` — routing policy (what's allowed local vs cloud, cost/context thresholds)
  - chord config for OpenRouter frontier-free + local high-context model refs (via vault)

  ## APPROACH
  1. A small router model classifies the request → destination (needs high context → local high-ctx or a
     frontier model; simple → cheap local; etc.), guided by an explicit policy (thresholds, allow/deny).
  2. Execute the generation on the chosen model, return the result to the caller (the doc engine).
  3. **Assume input is already PII-swept** — Chord's router is NOT the PII gate (DOCGEN-02 gated the input
     upstream). But Chord still respects ISO egress isolation for any cloud call (the allowlist).
  4. Router decision is logged (which destination, why) for the evaluation sweep (DOCGEN-04).
  5. Graceful fallback: chosen destination unavailable → policy-defined fallback (e.g. local if cloud down),
     never silently fail the doc generation.

  ## TEST PLAN
  - `cargo test --workspace` (chord)
  - Unit: router maps high-context request → high-ctx destination; simple → cheap; per policy
  - Unit: chosen destination down → policy fallback (negative test: no silent failure)
  - Unit: cloud call respects egress isolation/allowlist (negative test: no unisolated cloud egress)
  - Unit: routing decision logged for evaluation
  - Verify secrets via vault; no infra literals

  ## EDGE CASES
  - Frontier-free tier rate-limited → fallback to local, note it
  - Request too big for any single model's context → chunk or route to highest-context option, don't truncate silently
  - Router model itself unavailable → conservative default route + warning

  ## ACCEPTANCE CRITERIA
  - [ ] Chord SLM-router: request → destination decision (local/cloud/frontier) per explicit policy → execute
  - [ ] Chosen-destination-down → policy fallback, no silent failure (negative test)
  - [ ] Cloud calls respect ISO egress isolation/allowlist (negative test)
  - [ ] Routing decisions logged for the evaluation sweep
  - [ ] No hardcoded infrastructure values; secrets via vault
  - [ ] All existing tests still pass

---

### DOCGEN-04: SLM router evaluation sweep (test panel)
- **Priority:** Medium
- **Labels:** chord, docgen, router, sweep, eval
- **Agent:** claude
- **Estimate:** 6h
- **Description:** A test panel that measures candidate SLMs on ROUTING quality — did the router pick a
  destination that produced good docs at acceptable cost/latency? New profiling dimension, measure-don't-
  assume, results tagged for comparison (and `dynamic_gtt` for the current mem config).

  ## FILES
  - the sweep harness extension for router eval (in terminus/chord sweep code)
  - Postgres table/dimension for router-eval results
  - tests

  ## APPROACH
  1. Define routing-quality metrics: decision appropriateness (did a high-context task go to a high-context
     model?), resulting doc quality (via a grader), cost, latency.
  2. Run candidate SLM routers over a fixed set of representative doc-gen requests; score each.
  3. Persist to a router-eval table (same model_id normalization as other sweeps; tag mem_config).
  4. Sanity-check the grader against one known-good case (H4 lesson — a bad grader invalidates the sweep).

  ## TEST PLAN
  - `cargo test --workspace`
  - Unit: routing-quality scorer against a known routing decision → expected score
  - Unit: results persist with correct model_id + mem_config tag
  - Integration (mocked): a candidate router scored end-to-end
  - Verify no infra literals; table PII-clean

  ## EDGE CASES
  - A router that always picks cloud (ignores local) → scored down on cost, flagged
  - A router that picks local for an over-context task → scored down on quality (output truncated/poor)
  - Grader disagreement → sanity-check before trusting the panel

  ## ACCEPTANCE CRITERIA
  - [ ] Router-eval panel scores candidate SLMs on decision-appropriateness + doc-quality + cost + latency
  - [ ] Results persist with model_id + mem_config tag
  - [ ] Grader sanity-checked against a known case before trusting scores
  - [ ] No hardcoded infrastructure values; results PII-clean
  - [ ] All existing tests still pass

---

### DOCGEN-05: Doc generation orchestration (read feat → deepen docs)
- **Priority:** High
- **Labels:** terminus, docgen, generation
- **Agent:** claude
- **Estimate:** 6h
- **Description:** The core generation flow: read what was built (swept diff/spec), request generation via
  Chord's router, and produce deepened documentation content — revising/extending existing docs based on the
  actual change, not regenerating from scratch each time.

  ## FILES
  - `src/tools/docgen/generate.rs` — orchestration (swept-input → Chord router → content)
  - tests

  ## APPROACH
  1. Take the (PII-swept, DOCGEN-02) feat context + existing docs for the project.
  2. Request generation via Chord (DOCGEN-03) — deepen/revise: incorporate what the feat changed into the
     existing docs, preserving good prior content rather than overwriting wholesale.
  3. Return structured content ready for per-target rendering (DOCGEN-06).
  4. Never send un-swept content (DOCGEN-02 is the gate; assert the ordering).

  ## TEST PLAN
  - `cargo test --workspace`
  - Unit: generation revises existing docs (deepens) rather than replacing (asserted on a before/after)
  - Unit: only swept content reaches the Chord request (negative test: ordering enforced)
  - Integration (mocked Chord): feat context → deepened content
  - Verify no infra literals; secrets via vault

  ## EDGE CASES
  - First-ever doc for a project (no existing docs) → generate fresh, not "deepen nothing"
  - Feat with no doc-relevant change → minimal/no update, don't fabricate
  - Generation returns poor/empty → don't write an empty doc version; flag

  ## ACCEPTANCE CRITERIA
  - [ ] Generation deepens/revises existing docs from the feat (not wholesale overwrite) — asserted
  - [ ] Only PII-swept content reaches inference (ordering enforced, negative test)
  - [ ] First-doc and no-op-change cases handled sensibly
  - [ ] No hardcoded infrastructure values; secrets via vault
  - [ ] All existing tests still pass

---

### DOCGEN-06: Multi-format rendering (README / wiki / PDF / Notion / Obsidian / blog)
- **Priority:** High
- **Labels:** terminus, docgen, render, output
- **Agent:** claude
- **Estimate:** 6h
- **Description:** Render the generated content into each declared target format. Artifacts only — the engine
  produces the files/entries; it does NOT place them (the calling harness does).

  ## FILES
  - `src/tools/docgen/render/{markdown,wiki,pdf,notion,obsidian,blog}.rs` — per-format renderers
  - tests

  ## APPROACH
  1. For each declared target in the project config, render the content to that format (markdown README,
     wiki markup, PDF, Notion API entry, Obsidian note, blog post).
  2. Return the rendered artifact(s) to the caller — do not write to repos/hosting; the harness places them.
  3. Notion/Obsidian/blog via their APIs/creds (vault); PDF via a renderer; README/wiki as files.
  4. A target whose creds/renderer is unavailable → skip with a clear note, render the others.

  ## TEST PLAN
  - `cargo test --workspace`
  - Unit: each renderer produces valid output for its format from sample content
  - Unit: engine returns artifacts, does NOT write to a repo/hosting (negative test: no placement)
  - Unit: unavailable target skipped, others render (negative test)
  - Verify secrets via vault; no infra literals

  ## EDGE CASES
  - PDF of very long content → paginates, doesn't truncate
  - Notion/Obsidian API failure → that target skipped + noted, others succeed
  - Format needing assets (images) → handle or note the limitation

  ## ACCEPTANCE CRITERIA
  - [ ] Renders each declared format (readme/wiki/pdf/notion/obsidian/blog) from generated content
  - [ ] Returns artifacts to caller; does NOT place them (negative test)
  - [ ] Unavailable target skipped with note; others render (negative test)
  - [ ] No hardcoded infrastructure values; secrets via vault
  - [ ] All existing tests still pass

---

### DOCGEN-07: Artifact version control (versioned, diffable, rollback-able)
- **Priority:** High
- **Labels:** terminus, docgen, versioning, core
- **Agent:** claude
- **Estimate:** 6h
- **Description:** Core feature: every generated artifact is versioned — tied to the triggering feat/commit,
  diffable against its prior version, and rollback-able. Regenerating after each feat never clobbers good
  docs; a bad auto-generation is a new version you can compare and revert. The engine keeps its OWN versioned
  record independent of where the caller places the artifact.

  ## FILES
  - `src/tools/docgen/versioning.rs` — version store, diff, rollback
  - tests

  ## APPROACH
  1. On each generation, store the artifact as a new version keyed to (project, target, triggering feat/
     commit, timestamp). Never overwrite the prior version.
  2. Diff: produce a diff between any two versions of an artifact (what the feat changed in the docs).
  3. Rollback: restore a prior version as the current (e.g. a bad generation).
  4. This version store is the engine's own — independent of the caller's downstream placement, so diff/
     rollback work regardless of what the harness did with the artifact.
  5. Store is PII-clean (it holds already-swept content) and vault/config-driven for any backend.

  ## TEST PLAN
  - `cargo test --workspace`
  - Unit: each generation creates a new version, prior preserved (negative test: no overwrite)
  - Unit: diff between two versions is correct
  - Unit: rollback restores a prior version as current
  - Unit: versioning independent of caller placement (asserted)
  - Verify store PII-clean; no infra literals

  ## EDGE CASES
  - First version (nothing to diff against) → handled, diff is "all new"
  - Rollback to a version that referenced now-removed content → restores the doc as-was, notes the staleness
  - Concurrent generations for the same artifact → serialize, last-wins with both versioned

  ## ACCEPTANCE CRITERIA
  - [ ] Every generation is a new version tied to the feat/commit; prior never overwritten (negative test)
  - [ ] Diff between versions works; rollback restores a prior version
  - [ ] Versioning independent of caller placement (asserted)
  - [ ] Version store PII-clean
  - [ ] No hardcoded infrastructure values; secrets via vault
  - [ ] All existing tests still pass

---

### DOCGEN-08: Build-skill trigger (post-feat doc stage)
- **Priority:** High
- **Labels:** docgen, build-skill, trigger, pipeline
- **Agent:** claude
- **Estimate:** 5h
- **Description:** Wire the doc engine into the build pipeline as a post-feat stage: after a feat merges +
  verifies, invoke the doc engine with the feat context. This is the "next step in the build skill." It runs
  after Stage 7 (verify) / alongside the mirror stage — deepening docs is part of completing a feat.

  ## FILES
  - build-skill / pipeline integration point (the stage that fires the doc engine post-verify)
  - the v3.5 spec skill doc update (new stage described — coordinate with the skill)
  - tests

  ## APPROACH
  1. After merge + verify (and per project config), invoke the doc engine with: spec_id, merged diff, repo,
     project doc-target config.
  2. The engine runs its flow (PII sweep → Chord router → generate → render → version) and returns artifacts.
  3. The pipeline/harness places the artifacts per the project's placement rules (the engine doesn't place).
  4. Non-blocking to the build's success: a doc-gen failure logs + flags but does NOT fail the merged feat
     (docs are important but shouldn't un-merge working code). Report doc-gen outcome per feat.
  5. Opt-in per project (like mirror-ready): a project without doc-targets configured → stage skips.

  ## TEST PLAN
  - `cargo test --workspace`
  - Unit: post-verify, the trigger invokes the engine with correct feat context
  - Unit: doc-gen failure does NOT fail the feat (negative test: build stays green, doc flagged)
  - Unit: project with no doc-targets → stage skips
  - Integration (mocked engine): full trigger → artifacts returned → placement invoked
  - Verify no infra literals; Plane via Terminus Plane tool

  ## EDGE CASES
  - Doc engine unavailable → feat still succeeds, doc-gen flagged for retry
  - Feat with no doc-relevant change → engine returns minimal/no update, stage completes clean
  - Multiple feats in a batch → each triggers its own doc-gen, versioned per feat

  ## ACCEPTANCE CRITERIA
  - [ ] Post-verify trigger invokes the doc engine with feat context (spec/diff/repo/config)
  - [ ] Doc-gen failure does NOT fail the merged feat (negative test)
  - [ ] Opt-in per project; no-config project skips the stage
  - [ ] Engine returns artifacts; placement is the harness's job (not the engine's)
  - [ ] No hardcoded infrastructure values; Plane via the Terminus Plane tool
  - [ ] All existing tests still pass

---

### DOCGEN-09: Documentation
- **Priority:** Medium
- **Labels:** docgen, docs
- **Agent:** gemini
- **Estimate:** 4h
- **Type:** documentation
- **Description:** Document the doc engine: the flow, per-project config, the PII gate, Chord SLM routing +
  the eval sweep, versioning, and the build-skill trigger. (Meta: the doc engine should eventually document
  itself — but this bootstrap doc is written normally.)

  ## OUTLINE
  - Overview (~150w): sovereign doc engine replacing Mintlify; post-feat deepening; artifacts-only.
  - Flow (~200w): trigger → PII sweep → Chord router → generate → render → version → return.
  - Per-project config (~150w): declaring doc targets; placement is the caller's job.
  - PII gate (~120w): unconditional input sweep; keeps docs infra-clean.
  - Chord SLM routing + eval sweep (~150w): Chord decides destination; how routers are measured.
  - Versioning (~120w): versioned/diffable/rollback-able artifacts; independent of placement.

  ## TONE
  Technical reference; plain-language operator sections; no infra literals.

---

### DOCGEN-10: Behavior-contract mismatch detector (panel-adjudicated → Plane issue)
- **Priority:** Medium
- **Labels:** terminus, docgen, review, feedback-loop, plane
- **Agent:** claude
- **Estimate:** 7h
- **Description:** A feedback loop that surfaces **code-vs-behavior-contract mismatches** a code reviewer
  might miss. At doc-generation time the engine already holds two independent descriptions of the system:
  the **actual behavior** (extracted from the merged code) and the **intended behavior** (spec acceptance
  criteria / behavior contract / prior docs). When the actual behavior *contradicts* a binding contract, the
  detector dispatches the **Terminus 5-agent review panel** to adjudicate which side is authoritative and
  what the resolution is, then files a **Plane issue** (via the Terminus Plane tool) with the panel's
  consensus resolution — or escalates to human if the panel can't converge.

  **Why this catches what code review misses:** a code reviewer sees code-vs-code; it lacks an independent
  notion of intended behavior. The mismatch lives in the GAP between the actual behavior and the stated
  contract — which the doc engine uniquely holds both sides of.

  ## FILES
  - `src/tools/docgen/mismatch.rs` — the detector + panel dispatch + Plane-issue filing
  - tests

  ## APPROACH
  1. **Tiered sensitivity (conservative overall, keyed to contract strength):**
     - Acceptance criteria / explicit behavior contracts → high sensitivity: a contradiction (contract says
       X, code does not-X) is high-confidence, always evaluated.
     - Documented prose behavior → flag only a genuine contradiction (not phrasing/omission).
     - Implementation detail / style / summary-level difference → NEVER flag (docs summarize; that's not
       drift). The bias is toward silence; every filed issue must be a broken *promise* about behavior.
  2. **Panel adjudication (the direction decision):** on a candidate mismatch, dispatch the Terminus 5-agent
     review panel with BOTH artifacts laid out (actual code behavior + the stated contract) and the EXPLICIT
     question: *"These disagree — which is authoritative (is the code wrong, or is the contract/spec stale),
     and what's the resolution?"* This is an AUTHORITY/DIRECTION judgment, NOT a code-quality review — the
     panel must be prompted for that specific question, or it will review the code and miss that the CONTRACT
     might be the stale side.
  3. **Consensus → Plane issue with the resolution:** file a Plane issue (via the Terminus Plane tool) with
     both sides quoted + the panel's consensus resolution as the suggested approach. The resolution direction
     can be EITHER "fix the code to match the contract" OR "update the contract/spec — the code is right and
     the spec is stale" — both are valid outcomes (this is what makes the loop safe in both directions).
     Queue state per the resolution's confidence (ready-for-build if consensus is clear code-fix; the panel
     may mark spec-update or needs-care).
  4. **No consensus → escalate to human:** if the 5 agents can't converge, file the Plane issue as
     needs-human-decision (NOT auto-queued for build) — panel disagreement IS the ambiguity signal.
  5. All Plane ops via the Terminus Plane tool. Runs at doc-gen time (it already has both sides). Non-
     blocking: a detector failure never fails the feat or the doc-gen.

  ## TEST PLAN
  - `cargo test --workspace`
  - Unit: a clear contract contradiction (AC says X, code does not-X) → candidate raised; a phrasing-only
    difference → NOT raised (negative test: no noise on summary-level diffs)
  - Unit: panel dispatched with the AUTHORITY question + both artifacts (not a code-quality prompt) — asserted
  - Unit: panel consensus "code wrong" → Plane issue with code-fix resolution; consensus "spec stale" → Plane
    issue with contract-update resolution (both directions produce valid issues)
  - Unit: panel no-consensus → issue filed needs-human, NOT auto-queued (negative test)
  - Unit: detector failure does not fail the feat/doc-gen (negative test)
  - Verify Plane ops via the Terminus Plane tool; no infra literals; both-sides quoting is PII-swept

  ## EDGE CASES
  - Contract itself ambiguous → panel likely no-consensus → human escalation (correct)
  - Code right, spec stale → resolution is spec-update, NOT a code rewrite (the loop must not "fix" correct code)
  - Many small mismatches in one feat → batch into one review issue, don't spam Plane
  - Panel unavailable → file the raw mismatch as needs-human (don't guess direction without the panel)

  ## ACCEPTANCE CRITERIA
  - [ ] Detects contradictions of binding contracts (AC/behavior contract); ignores phrasing/summary diffs (negative test)
  - [ ] Dispatches the 5-agent panel with the AUTHORITY/direction question + both artifacts (not code-quality) — asserted
  - [ ] Consensus → Plane issue with resolution (either code-fix OR spec-update direction, both valid)
  - [ ] No consensus → Plane issue as needs-human, not auto-queued (negative test)
  - [ ] All Plane ops via the Terminus Plane tool; both-sides quoting PII-swept
  - [ ] Detector failure never fails the feat/doc-gen (negative test)
  - [ ] No hardcoded infrastructure values; secrets via vault
  - [ ] All existing tests still pass

---

## Notes for the executing agent
1. **PII sweep gates the INPUT unconditionally, before Chord's router.** This is the load-bearing safety —
   the doc tool reads real diffs and could route to cloud; unsanitized content must never reach any model.
   DOCGEN-02 gates; DOCGEN-03 (Chord) assumes swept input. Assert the ordering.
2. **Chord owns routing, the doc engine consumes it.** Even in one combined spec, keep the seam: the engine
   asks Chord to route; Chord's SLM router decides. The engine never picks a model.
3. **Artifacts only — the engine never places files.** It returns versioned artifacts; the calling harness
   places them. Do not have the engine write to repos/hosting.
4. **Version control is core, not optional.** Every generation is a new version; prior never clobbered;
   diff + rollback. This is what makes post-feat regeneration safe.
5. **Config-driven output, opt-in per project.** A project declares its doc targets; no config → sensible
   default / stage skips. Don't force docs on a project that hasn't opted in.
6. **Doc-gen failure never fails the feat.** Docs matter but must not un-merge working code — log + flag,
   keep the build green.
7. **Build order:** Chord SLM router (DOCGEN-03) before the engine consumes it (05); PII gate (02) before
   generation (05); versioning (07) before/with rendering. Sequence so dependencies exist when needed.
8. **Full v3.5 pipeline; Plane via the Terminus Plane tool; ingest project TERM.** Respect ISO egress
   isolation on any cloud (OpenRouter) call.
9. **The mismatch loop (DOCGEN-10) must be safe in BOTH directions.** When code and contract disagree,
   either can be the correct one — so the 5-agent panel judges *authority/direction*, and a valid resolution
   is "update the stale spec," not only "fix the code." The loop must NEVER auto-rewrite correct code to
   match an outdated contract. Panel no-consensus → human, not a guess. This is what turns a self-correcting
   loop into a safe one rather than a self-corrupting one. Prompt the panel for the authority question
   specifically — not a code-quality review (it was built for the latter; this needs the former).
