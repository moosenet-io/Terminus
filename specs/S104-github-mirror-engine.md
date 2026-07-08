# GitHub Mirror Engine — clean work-dir derivative, Rust PII gate, doc-ordered, mandatory
plane_project: TERM
module: Terminus
prefix: GHMR
spec_id: S104-github-mirror-engine

## Metadata
- **Author:** <operator> (Moose) + Claude
- **Session:** S104
- **Date:** 2026-07-08
- **Module version:** Terminus (terminus-rs) v1.2.0+
- **Estimated total:** ~18h autonomous agent work + operator bootstrap/decisions
- **Context:** The public `moosenet-io/*` GitHub mirrors are stale. Root cause (confirmed by
  PII pre-scan of tracked non-test files: Terminus 14, Chord 0/+8 token-shaped, Lumina 215
  private-IP hits): **internal main is not PII-clean**, so the current Stage 7b `git push
  github main` would be hard-blocked by the PII gate — and the public repos have **no common
  git ancestor** with internal main (curated exports), so a normal push can't fast-forward and
  the standing rule is "operator curation, never force." This spec replaces the naive push
  with a durable engine: a **per-repo clean mirror work dir** — a PII-swept *derivative* of
  internal main that keeps its own linear history — managed by a github **core-tool** subtool,
  with the PII gate ported from Python (`pii_gate.py`) to Rust (building on the existing
  `src/github/pii.rs`), subagent-based cleaning operationalized for non-mechanical violations,
  and the mirror stage sequenced to run **after the documentation engine (Scribe)** so the
  public repos ship with current docs. Codified as the build skill's next revision.

## Pre-flight
- Repository: `moosenet/Terminus` (the github tool + Rust PII gate live here); the skill change
  targets `<path>/.claude/skills/moosenet-spec/SKILL.md`.
- github is a **CORE tool** (primary/core registry), per the operator's tool taxonomy — these
  subtools register on the core registry alongside the other `github_*` tools.
- The mirror push is git transport → **dev-box-only** (S9's documented git-remote exception).
  Reconciliation (see GHMR-04): the engine's logic lives in the terminus-rs github module, but
  its git operations RUN ON THE DEV BOX (the sanctioned git-transport host), invoked as the
  terminus github mirror tool locally — logic-in-terminus, transport-on-dev-box. No other host
  gets a GitHub credential.
- `plane_prefix_check GHMR` at ingest to confirm the prefix is free (S101 registry is the source
  of truth); `plane_prefix_register GHMR` to claim it.

### OPEN DECISIONS (operator — resolve before/at execution; each is called out again in items)
1. **GitHub push credential:** the dev box currently has NO GitHub push path (no gh, no
   GITHUB_TOKEN, no remote, no key). Provide a `GITHUB_TOKEN` with **Contents:write** (materialized
   into terminus's SecretManager/vault for the github tool; the dev-box mirror invocation reads it
   the same way). Operator action.
2. **One-time re-baseline blessing:** the initial bootstrap force-inits each GitHub mirror to the
   swept snapshot (the single sanctioned `--force`, overriding "never force" for this deliberate
   re-curation). Operator must bless per repo.
3. **Placeholder scheme:** the canonical mechanical map (real infra value → placeholder token,
   e.g. a `192.168.0.<n>` host → `${…_HOST}` / `<REDACTED_LAN_IP>`). GHMR-02 proposes it;
   operator confirms.
4. **Harmony:** not `mirror_ready` (no `.moosenet-pipeline.yaml`). Flag + mirror it (needs a
   `moosenet-io/Harmony` target), or leave out. Operator decides.
5. **Skill version number:** the loaded skill is already labeled v3.8 (Scribe). This mechanism was
   proposed as "v3.8"; since v3.8 is taken, GHMR-06 bumps to **v3.9** unless the operator wants a
   renumber.

## Pre-Enriched Items

### GHMR-01: Rust PII sweep engine (authoritative; retire pii_gate.py)
- **Priority:** High
- **Labels:** terminus, github, pii, security
- **Agent:** claude
- **Estimate:** 3h
- **Description:** Grow the existing `src/github/pii.rs` into the authoritative tree-sweep engine
  and retire the Python `.githooks/pii_gate.py`. A reusable function scans a directory tree (not
  just staged files) and returns structured violations `{file, line, pattern_kind, matched_span}`
  for: private IPs (`10.x`, `172.16-31.x`, `192.168.x`), API-key prefixes (`ghp_`/`github_pat_`/
  `gto_`/`glpat-`/`sk-…`), configured PII terms/infra-service names, emails, and username paths —
  honoring the `// pii-test-fixture` exemption (per the structural-import precedent). Provide both a
  library API (for the mirror engine, GHMR-03/04) and a thin Rust pre-push hook binary that replaces
  the Python hook across repos.

  ## FILES
  - `src/github/pii.rs` — extend into the sweep engine (tree walker + rule set + violation report)
  - `src/github/mod.rs` — expose the sweep API; register any `github_pii_scan` tool
  - `src/bin/pii_gate.rs` — thin pre-push hook binary (reads refs, sweeps changed tree, blocks on violations)
  - README.md — document the Rust gate + hook install

  ## APPROACH
  1. Factor the existing date/phone-regex-corrected pii.rs into a `PiiRuleSet` + `scan_tree(path) -> Vec<Violation>`.
  2. Load configurable PII terms/placeholder map from a repo-root config (no hardcoded infra values in code — the rule *patterns* are generic; specific terms come from config), via config/SecretManager for any sensitive term list.
  3. Honor `// pii-test-fixture` line/structural-import exemptions exactly as the current gate + precedent.
  4. Emit a stable machine-readable report (JSON) + human summary.
  5. Ship `pii_gate` Rust binary as the pre-push hook; document swapping the Python hook out.

  ## TEST PLAN
  - Unit: fixtures with each pattern kind → correct violations; a `// pii-test-fixture` line → not flagged; a clean tree → 0.
  - Parity: run against a known-dirty tree and assert it matches the Python gate's findings (no regression in coverage).
  - `cargo test --workspace`; `no_pii_in_own_source_tree` GREEN.
  - Verify no hardcoded infrastructure values in new/modified files.

  ## EDGE CASES
  - Binary files / large files — skip or byte-scan without UTF-8 panic.
  - `pii-test-fixture` must not become a blanket bypass — only exact tagged lines / precedented structural imports.
  - Emails vs. placeholder emails (`@example.com`) — allowlist placeholders.

- **Acceptance criteria:**
  - [ ] `scan_tree()` returns structured violations for all documented pattern kinds
  - [ ] `// pii-test-fixture` exemption honored; not a blanket bypass (negative test)
  - [ ] Rust pre-push hook binary replaces `pii_gate.py`; parity with prior coverage proven
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] README updated (Rust gate + hook install)
  - [ ] All existing tests pass; `no_pii_in_own_source_tree` green

### GHMR-02: Mechanical sweep/transform (real values → placeholders)
- **Priority:** High
- **Labels:** terminus, github, pii
- **Agent:** codex
- **Estimate:** 3h
- **Description:** Given a source tree + a canonical placeholder map, produce a *candidate clean
  tree* by mechanically rewriting deterministically-fixable PII (private IPs, known infra hosts/
  URLs, org/email) to placeholder tokens, and return the *residual* violations that need human/
  agent judgment (GHMR-05). The placeholder map is config-driven (proposed default in the spec's
  Open Decision #3), never hardcoded in code.

  ## FILES
  - `src/github/mirror/sweep.rs` — the transform (map-driven rewrite + residual report)
  - `src/github/mirror/mod.rs` — module wiring
  - `.env.example` / a `mirror-placeholders.toml` schema — document the map format

  ## APPROACH
  1. Load the placeholder map (pattern → token) from config; validate it's total over the mechanical rule kinds.
  2. Walk the tree; for each mechanical violation, apply the mapped placeholder; leave non-mechanical (prose that needs restructuring, ambiguous strings) untouched and record as residual.
  3. Return `{files_rewritten, replacements, residual_violations[]}`; write into the work-dir copy (GHMR-03), never the source repo.
  4. Idempotent: re-running on an already-swept tree yields 0 further changes.

  ## TEST PLAN
  - A tree with 192.168.x + an org name → swept to placeholders; a residual prose case → reported not rewritten.
  - Idempotency test (second run = no-op).
  - `cargo test --workspace`; no hardcoded infra values (the map lives in config/fixtures, tagged if needed).

  ## EDGE CASES
  - A real IP inside a code string that must stay functional (none should, but flag rather than silently break).
  - Overlapping patterns — deterministic precedence.
  - Placeholder collisions (two real values → same token) — keep distinct tokens.

- **Acceptance criteria:**
  - [ ] Mechanical PII rewritten to placeholders from a config-driven map
  - [ ] Residual (non-mechanical) violations reported, not rewritten
  - [ ] Idempotent on an already-swept tree
  - [ ] Writes only to the work-dir copy, never the source repo
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] Tests pass

### GHMR-03: Clean mirror work-dir manager + mirror-approved tag
- **Priority:** High
- **Labels:** terminus, github, mirror
- **Agent:** claude
- **Estimate:** 3h
- **Description:** Per `mirror_ready` repo, maintain a dedicated **clean work dir** that holds the
  PII-swept derivative of internal main and keeps its OWN linear git history. On each run:
  sync the latest internal main content in, run GHMR-02's sweep + GHMR-01's gate, commit the swept
  state to the work-dir history, and — iff the gate reports **0 residual violations** — create a
  `mirror-approved/<internal-sha>` tag marking that swept commit as vetted for public push. This
  work-dir history is what shares ancestry with the GitHub mirror (the lineage bridge).

  ## FILES
  - `src/github/mirror/workdir.rs` — work-dir lifecycle (init/sync/commit/tag)
  - `src/github/mirror/mod.rs` — wiring

  ## APPROACH
  1. Work-dir location from config (a mirror-staging path, per repo). Init once (GHMR-07 bootstrap).
  2. Sync: import internal main's tree content into the work dir (content sync, NOT a merge of the divergent histories — the work dir keeps its own linear history).
  3. Run sweep (GHMR-02) → apply into work dir; run gate (GHMR-01).
  4. Commit swept state with a message referencing the internal sha. If gate clean → tag `mirror-approved/<internal-sha>`.
  5. If residual violations → do NOT tag; return them for GHMR-05 (subagent cleaning).

  ## TEST PLAN
  - From a dirty internal snapshot → work dir ends swept+committed; clean → tagged; dirty-residual → not tagged, residual returned.
  - Work-dir history stays linear across two syncs (no divergent-ancestor merge).
  - `cargo test --workspace`; no hardcoded infra values.

  ## EDGE CASES
  - First run (empty work dir) vs. incremental sync.
  - Internal main unchanged since last approved → no-op, keep existing tag.
  - Deleted files in internal main reflected in the work dir.

- **Acceptance criteria:**
  - [ ] Per-repo clean work dir holds the swept derivative with its own linear history
  - [ ] Swept state committed each run; `mirror-approved/<sha>` tag created ONLY when gate is clean
  - [ ] Residual violations block the tag and are returned for cleaning
  - [ ] No hardcoded infrastructure values
  - [ ] Tests pass

### GHMR-04: github mirror subtools (core registry) + dev-box transport
- **Priority:** High
- **Labels:** terminus, github, mirror, tooling
- **Agent:** claude
- **Estimate:** 3h
- **Description:** Expose the engine as github **core-tool** subtools: `github_mirror_status`
  (divergence + last-approved), `github_mirror_prepare` (sync→sweep→gate→commit, GHMR-02/03),
  `github_mirror_approve` (tag when clean), `github_mirror_push` (fast-forward push the
  `mirror-approved` work-dir state to the repo's `github_remote` using `GITHUB_TOKEN` via
  SecretManager). Reconcile with dev-box-only git transport: the push (and the work-dir git ops)
  execute ON THE DEV BOX — the tool logic lives in terminus-rs, invoked locally on the dev box for
  the transport steps; no other host holds a GitHub credential. `github_mirror_push` NEVER force-
  pushes (bootstrap force is GHMR-07's one-time operator-blessed step); it refuses a non-ff push.

  ## FILES
  - `src/github/mirror/tools.rs` — the 4 tools
  - `src/github/mod.rs` — register on the CORE registry (github is a core tool)
  - README.md + SKILL.md hooks

  ## APPROACH
  1. Tools call GHMR-03's workdir + GHMR-01/02 engine.
  2. `github_mirror_push`: verify the target commit is a ff-descendant of the mirror's current tip; if not, refuse with a clear error pointing at GHMR-07 (bootstrap) — never force.
  3. `GITHUB_TOKEN` via `vault::manager().get()` / SecretManager — never raw env, never logged; the push uses it as the git credential (GIT_ASKPASS runtime injection), executed on the dev box.
  4. Guarded-tool posture for the write tools (approve/push) matching the github module's destructive-tool convention.

  ## TEST PLAN
  - Unit: status/prepare/approve state machine; push refuses non-ff (asserted); token never in output.
  - Integration (against a throwaway local bare repo standing in for the mirror): ff push succeeds; non-ff refused.
  - `cargo test --workspace`; no hardcoded infra values; README updated.

  ## EDGE CASES
  - Token missing/read-only → clear error, no partial push (matches current Stage 7b failure protocol).
  - Mirror ahead of work dir (someone pushed) → refuse, surface for operator.
  - Approve called with residual violations pending → refuse.

- **Acceptance criteria:**
  - [ ] `github_mirror_{status,prepare,approve,push}` on the core registry
  - [ ] `push` fast-forward-only; refuses non-ff (never force); token via SecretManager, never logged
  - [ ] git transport executes on the dev box only (no GitHub credential on other hosts)
  - [ ] README updated to document the subtools
  - [ ] No hardcoded infrastructure values; tests pass

### GHMR-05: Operationalize subagent cleaning of residual violations
- **Priority:** Medium
- **Labels:** terminus, github, mirror, harness
- **Agent:** claude
- **Estimate:** 2h
- **Description:** When GHMR-02/03 leave residual (non-mechanical) violations, operationalize a
  cleaning pass: dispatch a scoped cleaning subagent that remediates the flagged spots in the clean
  work dir (judgment placeholdering / prose restructuring — never altering the source repo),
  re-sweeps to 0, and either yields a clean tag-able state or escalates specific spots to the
  operator. Define this as a repeatable harness step invoked by `github_mirror_prepare` when
  residuals remain, not an ad hoc one-off.

  ## FILES
  - `src/github/mirror/clean.rs` — the residual-cleaning orchestration hook (dispatch + re-sweep loop)
  - docs — the cleaning-subagent contract (inputs: residual list + work dir; output: cleaned tree or escalation)

  ## APPROACH
  1. `prepare` returns residuals → invoke the cleaning step (bounded rounds, e.g. ≤3) that edits only the work dir.
  2. Each round: agent remediates the specific flagged spans → re-run GHMR-01 gate → repeat until 0 or max rounds.
  3. On max-rounds-with-residuals: escalate the exact spots (file:line) to the operator; do NOT tag/push.
  4. Every remediation is confined to the work dir; the source repo is never touched.

  ## TEST PLAN
  - Simulated residual → cleaning loop drives gate to 0 (mock the agent step) → tag-able.
  - Unresolvable residual → escalation payload with exact spots; no tag.
  - `cargo test --workspace`; no hardcoded infra values.

  ## EDGE CASES
  - Cleaning that would break a doc's meaning → escalate rather than mangle.
  - Infinite-loop guard (bounded rounds).

- **Acceptance criteria:**
  - [ ] Residual violations trigger a bounded, repeatable cleaning pass on the work dir only
  - [ ] Drives the gate to 0 or escalates exact spots to the operator (no silent pass)
  - [ ] Source repo never modified by cleaning
  - [ ] No hardcoded infrastructure values; tests pass

### GHMR-06: Build-skill revision — mirror AFTER docs, mandatory, new engine (v3.9)
- **Priority:** High
- **Labels:** terminus, docs, pipeline
- **Agent:** claude
- **Estimate:** 2h
- **Type:** documentation
- **Description:** Revise `moosenet-spec/SKILL.md` to (a) redefine the mirror stage as this engine
  (clean work dir + Rust gate + subtools) instead of a bare `git push github main`; (b) sequence
  the mirror stage to run **AFTER the documentation engine (Scribe/README) stage** so the mirror
  ships current docs; (c) make mirroring **mandatory** for `mirror_ready` repos (was opt-in), the
  gate still an unconditional hard block; (d) document the dev-box-transport reconciliation and the
  bootstrap requirement; (e) bump the version (v3.9 unless operator renumbers — see Open Decision #5).

  ## AUDIENCE
  Executing agents + operator.

  ## OUTLINE
  - Stage ordering: … Verify (7) → Docs/Scribe (7c) → **Mirror engine (7d)** → Cleanup (8) (~150 words)
  - Mirror engine: prepare→(clean)→approve→push via the github subtools; gate unconditional (~250 words)
  - Mandatory-for-mirror_ready + the "internal main need not be clean; the work dir is the swept derivative" model (~200 words)
  - Bootstrap + dev-box transport + credential notes (~150 words)

  ## SOURCES
  - This spec; current Stage 7b + Stage 7 in SKILL.md; the Scribe hook (v3.8)

  ## TONE
  Authoritative process reference; env-var/placeholder only, no hardcoded infra values.

- **Acceptance criteria:** (documentation)
  - [ ] Mirror stage redefined as the engine and sequenced AFTER the docs/Scribe stage
  - [ ] Mandatory for `mirror_ready` repos; PII gate remains an unconditional hard block
  - [ ] Bootstrap + dev-box-transport + credential documented; version bumped
  - [ ] No hardcoded infrastructure values

### GHMR-07: One-time bootstrap + re-baseline per mirror_ready repo
- **Priority:** High
- **Labels:** terminus, github, mirror, ops
- **Agent:** <operator>
- **Estimate:** 1h + agent-assisted
- **Type:** human-action
- **Description:** For each `mirror_ready` repo (Terminus, Chord, lumina-constellation; + Harmony
  per Open Decision #4): initialize its clean work dir from current internal main (swept via the
  engine), and — with operator blessing — **force-init the GitHub mirror to that swept snapshot**
  (the single sanctioned `--force`, establishing the shared lineage so all future pushes ff). This
  is the one time "never force" is deliberately overridden, per repo, by the operator. Requires the
  `GITHUB_TOKEN` (Contents:write) from Open Decision #1.
- **Steps:**
  1. Operator confirms the `GITHUB_TOKEN` (Contents:write) is in terminus's SecretManager/vault and reachable to the dev-box mirror invocation.
  2. Agent runs `github_mirror_prepare` per repo → swept, gate-clean work dir (cleaning pass as needed; escalate residuals).
  3. Operator reviews the swept snapshot (spot-check no leaks) and blesses the re-baseline.
  4. Force-init each GitHub mirror to the approved swept snapshot (recorded, per repo).
  5. Thereafter `github_mirror_push` is ff-only and automatic per the pipeline.
- **Acceptance criteria:**
  - [ ] Each mirror_ready repo has an initialized clean work dir + gate-clean swept snapshot
  - [ ] Operator-blessed force re-baseline done per repo (shared lineage established)
  - [ ] Future pushes are ff-only (verified on the next merge)

### GHMR-08: Operator decisions bundle
- **Priority:** High
- **Labels:** ops
- **Agent:** <operator>
- **Estimate:** 20m
- **Type:** human-action
- **Description:** Resolve the Open Decisions so execution can proceed: (1) provide `GITHUB_TOKEN`
  Contents:write into the vault; (2) bless the force re-baseline approach; (3) confirm the GHMR-02
  placeholder map; (4) decide Harmony's mirror_ready status (+ create `moosenet-io/Harmony` if
  including); (5) confirm the skill version number (v3.9 vs renumber).
- **Acceptance criteria:**
  - [ ] All five Open Decisions answered so GHMR-01..07 can execute
