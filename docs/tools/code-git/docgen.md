[← Tool index](../README.md) · [← Docs index](../../README.md)

# docgen — the sovereign documentation engine

**Status: shipped (S95, spec `S95-documentation-engine`, Plane TERM-143..171).**
`docgen` lives at `src/tools/docgen/` (a `mod.rs` plus ~20 sibling modules and
a `render/` sub-directory), registered via `crate::tools::register` in
`src/tools/mod.rs`, itself called from `register_all()` in
`src/registry.rs` — the same core registration path `plane`/`gitea`/`github`/
`scribe` use. It registers **5 MCP tools** (`docgen_status`,
`docgen_mismatch_detect`, `docgen_generate_changelog`, `docgen_drift_check`,
`docgen_run`); the rest of the engine's ~20 modules are library code these
tools (chiefly `docgen_run`) compose internally, not separately-invoked
tools.

`docgen` is a **separate module from `scribe`** (`src/scribe/`): Scribe is
the standing knowledge-vault documentation agent; `docgen` is the sovereign,
config-driven, multi-format engine described in `S95-documentation-engine.md`
that replaces Mintlify. Docgen reuses Scribe's read-only worktree inspection
(`crate::scribe::inspect`) and pure note-rendering primitives
(`crate::scribe::vault`) rather than duplicating them, and reuses the fleet
PII sweep engine (`crate::github::pii`) rather than shipping a second
scanner — see each module's own doc comment for its specific reuse.

## Overview

`docgen` is Terminus's in-house, in-process documentation engine. It fires
**after** a feat merges and its tests pass (never on unreviewed code), reads
what was actually built, and deepens a project's documentation — revising
and extending existing docs with the real change, rather than regenerating
from nothing each time. It replaces Mintlify: instead of an externally
hosted docs SaaS, generation, PII sweeping, multi-format rendering, and
versioning all run in-process, inside the same binary that already holds the
Gitea/Plane/GitHub tools.

The engine is **artifacts-only**: every generation and render function
*returns* content — a string, a struct, a set of files-as-bytes — and never
itself writes to a filesystem, a git repository, or a hosting API. Placing a
returned artifact (committing a README, pushing a wiki page, calling a
publish API) is always the calling harness's job, one layer up. This
write-model inversion is deliberate and tested (`render_all_never_touches_
filesystem_or_vault` in `render/mod.rs`'s test suite) — it keeps the engine
safe to call from contexts, like the build skill's Stage 7c, that must never
have a side effect on their own initiative.

Every project opts in individually, the same way a repo opts in to the
public GitHub mirror via `mirror_ready`: no doc-target config declared means
the engine treats that project as not-yet-onboarded and skips cleanly,
rather than forcing docs on it.

## Flow

The end-to-end sequence, matching the flow the build skill's Stage 7c
invokes via `docgen_run` (DOCGEN-08):

```
trigger (docgen_run)
  -> PII sweep (pii_gate::sweep_input, DOCGEN-02 -- unconditional, pre-inference)
  -> generate via Chord's SLM router (generate::generate_docs, DOCGEN-05 / DOCGEN-03)
  -> render every declared target (render::render_all, DOCGEN-06)
  -> version each rendered artifact (versioning::VersionStore, DOCGEN-07)
  -> return the versioned artifacts to the caller
```

1. **Trigger** — the caller (typically the build skill, post-verify) invokes
   `docgen_run` with the feat's `spec_id`, the merged diff/spec/code as
   `feat_context`, the target `project`, `module_path`, `git_ref`, the
   project's existing docs, and its `project_config` (doc-target
   declarations).
2. **PII sweep** — `feat_context` is swept through
   `pii_gate::sweep_input` *before* anything else touches it. This is
   unconditional: it runs whether the eventual destination is a local model
   or a cloud one, because by the time the router picks a destination it is
   already too late to sweep.
3. **Generate** — the swept context plus the project's existing docs are
   handed to `generate::generate_docs`, which requests generation through
   Chord's SLM router. Chord decides local-vs-cloud; the doc engine never
   picks a model itself.
4. **Render** — `render::render_all` renders the generated content into
   every target the project's config declares (README, wiki, PDF, Notion,
   Obsidian, blog). A target with a missing credential, or a renderer whose
   backend binary isn't installed, is skipped with a clear note; every other
   declared target still renders independently.
5. **Version** — each successfully rendered artifact is stored as a new,
   append-only version in a `versioning::VersionStore`, keyed to the
   triggering feat/commit.
6. **Return** — the caller receives the versioned artifacts. `docgen_run`
   never places them; that's the harness's job, immediately after the call.

## Per-project config

A project declares which output artifacts it wants as a list of targets in
its `project_config`, each one of `readme` | `wiki` | `pdf` | `notion` |
`obsidian` | `blog`, plus free-form per-target rendering options:

```json
{
  "targets": [
    { "type": "readme" },
    { "type": "notion", "options": { "database_id": "..." } }
  ]
}
```

- **No config declared** (absent `project_config`, or an empty/missing
  `targets` list) → for `config.rs`'s own resolver, the safe default is
  README-only. For `docgen_run` specifically, no declared config means the
  project has not opted in at all — the engine is never invoked and the
  call reports `"outcome": "skipped"`. This is a stricter gate than
  `config.rs`'s README-only default, which only applies once a project
  *has* opted in with at least one declared target.
- **An unknown target type** → a clear, typed error naming the bad value
  and the valid set, never a panic.
- **A declared target whose credential is missing** (e.g. a `notion` target
  with no `NOTION_TOKEN` available) → that target resolves as disabled with
  a human-readable hint; every other declared target still resolves
  independently. `config.rs` never reads a secret *value* to decide this —
  it takes the caller-supplied set of available credential **key names**
  and only ever names the key a target needs. Resolving that key to an
  actual value via `vault::manager().get()` is deferred to the generation/
  render code that actually calls out to a target's API.
- **Placement is always the caller's job.** Declaring a target only tells
  the engine to render and version it — landing the result in a repo, a
  wiki, or a hosting API is a separate step the harness performs after the
  engine returns.

## PII gate

Before any inference request is built — for a local model or a cloud one —
the raw feat context (diff, spec text, touched source) passes through
`pii_gate::sweep_input`. This is the load-bearing safety net for the whole
engine: it is unconditional and runs regardless of what Chord's router will
later decide, because by the time the router picks a destination it's
already too late to sweep.

Detection reuses the canonical fleet sweep engine
(`crate::github::pii::scan_and_redact`) end to end — the same pattern set
already used to gate the git-public mirror (private IPs, `CT###`-shaped
container ids, internal hostnames, emails, API keys, and the
`// pii-test-fixture` whitelist convention). Docgen adds no new detection
logic, only the redact-vs-block decision and sanitized logging layered on
top.

Policy is **redact-preferred**: a detected PII span is replaced in place
with a `[REDACTED:{category}]` placeholder so generation can still produce
meaningful documentation ("the tool connects to an internal service" rather
than leaking the literal hostname). Content is only blocked outright when
redaction can't preserve enough surrounding meaning to be worth passing on.
Two further sweep points reuse this same gate downstream, for free text a
model itself might emit: diagram node labels (`diagram.rs`) and OG-card
frontmatter substitutions (`svg_assets.rs`) are swept again before
rendering, since a model can restate an internal hostname from its own
knowledge even when the swept input didn't contain one verbatim. The net
effect, beyond safety, is that published docs stay infra-clean by
construction.

## Chord SLM routing + eval sweep

Model selection is not this engine's job — it belongs to Chord. The doc
engine only ever *asks* Chord to generate; Chord's SLM router (`moosenet/
Chord`, `src/router/slm_router.rs`, DOCGEN-03) decides the actual inference
destination per an explicit routing policy (`src/router/policy.rs`): a
local high-context model, a local cheap model, or an OpenRouter
frontier-free cloud destination, subject to the ISO cloud-egress allow-list
before any network call. A destination that fails (unreachable, non-2xx,
egress-denied) never silently produces an empty result — the router walks
a defined fallback chain until either a destination succeeds or the
fallback floor is reached, at which point it returns a hard error rather
than fabricating a success.

Because routing quality is a distinct concern from raw model quality, a
separate evaluation sweep (`src/router/eval.rs`, DOCGEN-04) measures it: a
fixed, representative panel of doc-generation requests is run through a
candidate router, each output graded for doc quality, and combined with
decision-appropriateness/cost/latency into one score per request and one
summary per candidate. The panel never makes a live inference call against
the shared GPU host (it's held by a permanent production serve) — grading
runs against a mockable executor/grader seam, with a required sanity check
that a grader can actually discriminate a known-good output from a
known-bad one before any candidate score from it is trusted.

## Versioning

Every generated artifact `docgen_run` returns is versioned by
`versioning::VersionStore`, tied to the triggering feat/commit, diffable
against its prior version, and rollback-able. Regenerating after each feat
never clobbers good docs — a bad auto-generation is just a new version that
can be compared and reverted.

`store_version` always **appends**; there is no update-in-place. A rollback
restores a prior version as current by appending a *new* version copying
that prior content, rather than mutating history, so both "restore a prior
version" and "prior versions are never overwritten" hold at once. The store
is keyed by `(project, target)` and is entirely **independent of caller
placement** — it never reads or writes wherever the harness later lands the
rendered artifact on disk or in a repo, so diff and rollback keep working
regardless of what happened to a given version's content after the engine
returned it. The store holds only content the caller already PII-swept
upstream; it performs no scanning of its own and introduces no new PII
surface.

## `docgen_run` — the build-skill trigger (DOCGEN-08, Plane TERM-150)

The post-feat doc stage: the single orchestration entry point the build
skill calls after a feat merges and verifies. It assembles every piece
described above into one flow and returns versioned artifacts — it does not
place them anywhere. Lives at `src/tools/docgen/trigger.rs`; the pure
orchestration function is `run_docgen_trigger` (`docgen_run` is its
`RustTool` wrapper, holding its own `versioning::VersionStore` instance).

- **Opt-in per project, like `mirror_ready`.** A project with no
  `project_config` (or an empty/absent `targets` list) is not considered
  onboarded to this stage: the engine is never invoked and the call reports
  `"outcome": "skipped"`.
- **Non-blocking to the feat.** The underlying `run_docgen_trigger` function
  has no `Result`/`Err` in its signature at all — a config, PII-sweep, or
  Chord-generation failure is folded into a normal `"outcome": "failed"`
  response value, never propagated as an error a caller would need to treat
  as "the feat failed." A feat merges and verifies independently of whether
  its doc-gen stage succeeded.
- **`GenerationOutcome::NoChange` / `::Flagged` complete cleanly** with no
  render or version step — matching the "don't fabricate a version" edge
  case: a feat with no doc-relevant change, or a generation that came back
  too thin to trust, is not treated as a failure.
- **Artifacts only.** Exactly like `render_all`, this tool never writes to a
  filesystem, repo, or hosting surface — placing a returned artifact is the
  calling harness's job.

```json
{
  "name": "docgen_run",
  "arguments": {
    "spec_id": "S95-documentation-engine",
    "project": "TERM",
    "module_path": "src/tools/docgen",
    "git_ref": "237b14b",
    "feat_context": "the merged diff/spec/code describing what this feat changed",
    "existing_docs": "# terminus-rs docgen module\n\n...",
    "project_config": { "targets": [{ "type": "readme" }] },
    "available_credential_keys": []
  }
}
```

The build skill (v3.15, Stage 7c) invokes this after Stage 7 verify passes
and the in-repo README currency check has run, and before Stage 7d's public
mirror — so a `mirror_ready` repo's mirror ships the deepened docs
`docgen_run` just produced, never stale ones. A `docgen_run` failure is
logged and flagged for follow-up but never reverts the merge and never
blocks the mirror.

## `docgen_status`

Read-only, config-inspection only — reports how the engine would interpret
a project's declared (or absent) doc-target config, without generating or
rendering anything:

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
disabled for a missing credential.

## Multi-format rendering (DOCGEN-06)

`render::render_all` (`src/tools/docgen/render/`) renders generated content
into every format a project's config declares: `markdown` (README) and
`obsidian` reuse `crate::scribe::vault`'s pure note-rendering primitives
rather than reimplementing them; `wiki`, `pdf`, `notion`, and `blog` are
renderers added by the doc engine itself. `wiki` converts Markdown ATX
headings to MediaWiki-style `==`-delimited headings for broad compatibility
across self-hosted wiki engines. `notion` and `blog` perform a read-only
credential-validation call before rendering (never a create/update/publish
call) so a target with a present-but-invalid credential is skipped with a
clear note rather than returning a bogus artifact — every other declared
target still renders. `pdf` always returns a clear "renderer unavailable"
skip: no PDF-generation crate is present in this workspace, so pagination
logic is implemented and tested against that future backend, but no real
PDF bytes are produced yet.

## Auto architecture/flow diagrams (DOCGEN-11, `diagram.rs`)

After a merged feat, the engine can derive an architecture/flow/sequence
diagram from the code and diff. The model emits **D2 or Mermaid source** —
deterministic, diffable, PII-inspectable text — never a binary image
straight from a model. Rendering uses D2's bundled dagre/ELK layout engines
only (`--layout dagre` is passed explicitly on every invocation); D2's
proprietary hosted TALA layout engine is never invoked. Mermaid diagrams
need a browser or a self-hosted Kroki instance, neither of which is wired
in this build, so a Mermaid source is always reported as a render skip —
its swept source is still versioned. When the `d2` CLI is not on `PATH`,
this module returns the same clear "renderer unavailable" skip pattern as
the PDF renderer: the diagram source is still produced and versioned; only
the SVG raster is conditional on the tool being present.

## Ground-truth crate/module graph (DOCGEN-12, `crate_graph.rs`)

Unlike `diagram.rs`, this module never calls a model at all. Every node and
edge in the dependency graph is extracted directly from real source: each
workspace crate's `Cargo.toml` for crate-level edges, plus a deterministic
scan of `mod`/`use` declarations for module-level containment and
dependency edges. Because it's derived straight from the code, it doubles
as a drift signal: if the graph ever contradicts prose documentation, that's
exactly the kind of mismatch DOCGEN-10 escalates. The DOT source is always
produced (the diffable, versioned ground-truth artifact); rasterizing to
SVG additionally shells out to `sfdp` (preferred) or `neato` (fallback) if
either is on `PATH` — neither present means the DOT model is returned with
no SVG and a clear note, the same skip pattern used throughout this engine.

## Layered Diátaxis README (DOCGEN-13, `readme_layers.rs`)

Builds on the merged render layer rather than replacing it: the README
target still goes through `crate::scribe::vault`'s frontmatter and body
assembly; this module decides what goes into that body — a progressive-
disclosure structure (hero → quickstart → deep-dive) following the
standard-readme section order, plus a parallel four-way Diátaxis split
(tutorial / how-to / reference / explanation) for wider docs, each tagged
with its mode in frontmatter. Templating uses plain Rust string-building
functions rather than a template-engine dependency, matching the style
already used throughout the `render/` modules.

## Wiki information architecture (DOCGEN-14, `render/wiki_graph.rs`) and search index (DOCGEN-15, `search_index.rs`)

Nav/sidebar generation, a `[[wikilink]]` backlink index, and a
force-directed graph view are inherently whole-vault concerns — they need
every page's content at once, so these modules operate over a slice of
notes rather than a single render target and don't plug into
`render::render_all`. `search_index.rs` builds a static, offline search
index over a project's rendered pages the same way: `pagefind` (preferred,
if the binary is on `PATH`) or a small dependency-free built-in inverted
index with a vanilla-JS query helper as fallback when it isn't.

## SVG explainers + OG cards (DOCGEN-16, `svg_assets.rs`)

Produces theme-aware (light and dark) explainer graphics and per-page Open
Graph social cards. No SVG-to-PNG rasterization crate (`resvg`/`usvg`/
`tiny-skia`) is present in this workspace, so the SVG **source** is always
the guaranteed, diffable artifact the engine produces; PNG rasterization is
an injectable seam that reports a clear "resvg unavailable" skip until a
real backend is wired in. Frontmatter text substituted into a card (title,
summary) is swept through the PII gate a second time before rendering, for
the same reason diagram node labels are.

## Changelog + release notes (DOCGEN-17, `changelog.rs`)

`docgen_generate_changelog` produces a Keep-a-Changelog-formatted
`CHANGELOG.md` section plus a separate release-notes document from a list
of merged commits, parsed as Conventional Commits. No `git-cliff` binary
(the tool the originating research pointed at) is available in this build
environment and it's a standalone binary rather than a library crate to
vendor, so this ships a minimal, dependency-free, in-process parser and
renderer producing the same grouped-by-type, dated shape of output instead.
Commits are grouped into fixed sections (Breaking first, then Added/
Changed/Fixed/Documentation/Performance/Tests/Build & CI/Reverted/Chore/
Other) in deterministic order; a commit that doesn't match Conventional
Commit shape is never dropped, only filed under Other with its original
subject line. Merge commits are excluded as noise but counted separately.
Like the versioning store, this module never reads the system clock — the
caller supplies `version` and `date`, so the same input always produces
byte-identical output.

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

## Quality gate (DOCGEN-18, `quality.rs`)

A two-layer gate for a generated artifact: a deterministic prose lint
(banned words, max sentence length, a passive-voice heuristic — dependency-
free; the external `vale` linter is not assumed installed on build/serving
hosts, though an operator may run it out-of-band against rendered artifacts
without this module ever shelling out to it), paired with an LLM-as-judge
rubric scoring faithfulness, completeness, and coherence through the same
Chord generation seam DOCGEN-05 already established. The deterministic lint
always runs and can fail the gate on its own; the judge is best-effort and
degrades gracefully (missing generator, no diff context, or a failed call
all fall back to "judge unavailable," with the combined score then resting
on the lint alone) — the judge can never be the sole reason an artifact
passes when the lint itself found an error-level issue. Scores are keyed by
the same `(project, target, version)` the versioning store uses, so a
stored score is unambiguously "the score for this artifact's version N."

## Staleness/drift detection (DOCGEN-19, `drift.rs`) and mismatch detection (DOCGEN-10, `mismatch.rs`)

Two complementary, independent detectors, both exposed as MCP tools:

- **`docgen_drift_check`** anchors a doc snippet to the exact code location
  it describes (`{file, symbol, line-hash}`) and re-resolves that anchor
  against a later commit's read-only worktree, catching silent staleness
  before it ships — a purely structural check (a line-hash and
  symbol-declaration re-scan), no model call, no panel.
- **`docgen_mismatch_detect`** answers a different, more expensive question:
  does the *actual* behavior extracted from merged code genuinely
  *contradict* the *intended* behavior stated in an acceptance criterion,
  behavior contract, or prior documentation? A code reviewer only ever sees
  code-vs-code and lacks an independent notion of intended behavior, so a
  real contradiction here is escalated to the Terminus 5-agent review panel
  with an explicit authority/direction question — "which side is right, and
  what's the resolution?" — never a code-quality prompt. Either side can be
  the one that's wrong: a valid resolution is "fix the code" or "the
  contract/spec is stale, update it." No panel consensus is treated as the
  ambiguity signal it is — the mismatch escalates to a human, never
  auto-queued for either direction, and the loop never auto-rewrites code
  to match a contract that might itself be stale. Both detectors reuse the
  shipped Scribe discrepancy-filing machinery (stable-signature dedup, the
  local-queue fallback when Plane is unreachable) under their own dedup tag
  prefix, so a mismatch is never silently lost and never double-filed
  against Scribe's own discrepancy issues.

The two stay separate modules rather than one growing into the other: drift
is a cheap structural re-scan; mismatch is an expensive semantic
adjudication reserved for genuine content disagreement.

## Machine-readable AI surface (DOCGEN-20, `render/llms_txt.rs`)

Alongside the human-facing render targets, a project's documentation corpus
can also be emitted as two AI-consumption artifacts following the
`llmstxt.org` convention: `llms.txt`, an index (one line per page, title,
path, and a one-line summary) an LLM client can use to cheaply decide which
pages are worth fetching in full, and `llms-full.txt`, the complete
concatenated corpus for a client that wants everything in one shot. Like
the wiki graph and search index, this is a whole-corpus concern operating
over every rendered page at once rather than a single render target.

## Skip-when-tool-absent, summarized

Several capabilities depend on an external binary this build environment
does not have installed, and all of them follow the same permanent
(not provisional) pattern established by the PDF renderer: the guaranteed,
diffable, versioned artifact is always produced; a richer rasterized/
indexed byproduct is conditional on the tool being present, and its absence
produces a clear skip note rather than a failure.

| Capability | Preferred external tool | Fallback / always-produced artifact |
|---|---|---|
| Diagram rasterization (DOCGEN-11) | `d2` CLI | D2/Mermaid source text, versioned; no SVG |
| Crate-graph rasterization (DOCGEN-12) | `sfdp` or `neato` (Graphviz family) | DOT source, versioned; no SVG |
| SVG→PNG rasterization (DOCGEN-16) | `resvg`/`usvg`/`tiny-skia` (not vendored) | SVG source, versioned; no PNG |
| Search index (DOCGEN-15) | `pagefind` binary | Built-in dependency-free inverted index + JS query helper |
| Prose lint (DOCGEN-18) | `vale` (operator-run, out-of-band) | Built-in deterministic lint (banned words, sentence length, passive voice) |
| Changelog generation (DOCGEN-17) | `git-cliff` binary | Built-in Conventional-Commit parser + Keep-a-Changelog renderer |
| PDF rendering (DOCGEN-06) | a PDF-generation crate (not vendored) | Pagination logic implemented/tested; render always skips with page-count note |

## For operators

In plain terms: once a project is opted in (a `project_config` with at
least one declared doc target committed somewhere the build skill can read
it), every merged feat to that project automatically gets its documentation
deepened — no manual "write the docs" step, and no external SaaS dependency.
The engine never guesses at infrastructure details it wasn't told; anything
that looks like a private IP, an internal hostname, or a `CT###`-style
container id gets redacted before it ever reaches a model, local or cloud.
A doc-generation failure never blocks or reverts a merge — worst case, a
feat ships without deepened docs and that's logged for follow-up, not
silently swallowed. Placing a generated artifact (into a README file, a
wiki, a hosting API) is a deliberate, separate step performed by whatever
called the engine — the engine itself never writes to your repo or pushes
anywhere on its own initiative.
