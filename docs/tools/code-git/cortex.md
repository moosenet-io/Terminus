[← docs index](../../README.md)

# Cortex — Atlas-backed code-intelligence gate (CXEG-01/02/03/04/06/08/09/11/12)

Cortex is a 14-tool-name module (`src/cortex/mod.rs`, `src/cortex/scope.rs`,
`src/cortex/metrics.rs`, `src/cortex/review.rs`, `src/cortex/house_style.rs`,
`src/cortex/waiver.rs`, `src/cortex/crystallize.rs`, `src/cortex/debt.rs`,
`src/cortex/deprecated.rs`, `src/cortex/audit.rs`), but as of **CXEG-01** only
3 of those names were "real" tools — the rest are structured deprecation
aliases. Those 3 are now fully live, Atlas-backed implementations rather than
pending-pointer stubs: `cortex_scope` (**CXEG-02**), `cortex_review`
(**CXEG-04**, built on `cortex_scope`'s blast radius plus **CXEG-03**'s
standalone structural-elegance signal library), and `cortex_audit`
(**CXEG-11**). As of **CXEG-06**, a NEW 4th real tool, `cortex_house_style`,
was added; **CXEG-08** added `cortex_waive`; **CXEG-09** added
`cortex_crystallize`; **CXEG-12** added `cortex_consistency_debt` — each an
intentional, additive MCP-listing change (the module's registered tool-name
count went 10 → 11 → 12 → 13 → 14). This page describes the current shape for
`cortex_scope`/`cortex_review`/`cortex_audit`/`cortex_house_style` in depth;
see `README.md`'s "Cortex" section for `cortex_waive`/`cortex_crystallize`/
`cortex_consistency_debt` and `docs/cortex-elegance-gate.md` for the
end-to-end operator/contributor reference across all three tiers. See the
"History" section at the bottom for what changed and why.

## The single most important fact about this module: **the SSH-relay era is retired**

Every previous revision of this module was a thin SSH-exec relay to a script
on a since-**retired** external fleet host — the same transport pattern
`crucible`/`sentinel`/`vigil` still use. That host is gone, and CXEG-01
deleted the entire SSH transport (no SSH client library, no `TcpStream`, no
remote-script invocation) from this module. Cortex's successor is the
in-process **Atlas code graph** (`crate::scribe::graph`, the `kg_*` tool
family), which builds, persists, and queries a real graph locally — no SSH,
no remote script, no "relay whatever the other end says" response shape.

## What's here now

| Tool | Status | What it does |
| --- | --- | --- |
| `cortex_scope` | **live (CXEG-02)** | Resolves `project_id` + `changed_files`/`diff` against the project's Atlas graph and returns the blast radius: touched symbols, their 1-hop callers/callees, affected communities, `blast_count`, `token_reduction_pct`. Degrades to `configured:false` (no error) when the project has no stored graph. |
| `cortex_house_style` | **live (CXEG-06)** | Resolves `project_id` (+ optional `community`) against the project's Atlas graph and returns, per Leiden community, deterministic modal `facts` plus per-kind `exemplars_by_kind` node refs. Degrades to `configured:false` (no error) when the project has no stored graph; degrades to `profile:"unstable"`/`sparse:true`/`degraded:true` per-community/per-bucket rather than misrepresenting a thin sample. |
| `cortex_review` | **live (CXEG-04)** | Resolves `project_id` + `changed_files`/`diff` against the Atlas graph, computes CXEG-03's structural-elegance signals over the touched nodes plus KGFIND recurrence for the same scopes, and returns a `risk_score` (0-10), `band`, `risk_signals`, and fully-explainable `contributions`. Degrades to `configured:false`/`band:"unknown"` (no graph) or a structural-only score labeled `findings:"unavailable"` (no findings store) — never an error. |
| `cortex_audit` | **live (CXEG-11)** | Runs its existing SSRF-hardened `url` validation (unchanged, see below), clones `url` into an isolated scratch dir, builds a transient Atlas graph, runs the CXEG-03 structural detectors, and returns a report — see below. |
| `cortex_waive` | **live (CXEG-08)** | Records a tracked, mandatory-reason waiver against `review_run`'s Stage-5b risk-gate escalation. See `README.md`'s "Stage-5b risk-gate escalation + waivers" section. |
| `cortex_crystallize` | **live (CXEG-09)** | The rule-crystallization loop: recurring `consistency`/`elegance` findings that survive an adversarial `review_run` panel graduate to a lint stub or a `docs/house-style.md` prose rule. See `README.md`'s "Rule crystallization loop" section. |
| `cortex_consistency_debt` | **live (CXEG-12)** | Read-only per-community, per-category rollup of `consistency`/`elegance`/`waiver` findings from the same KGFIND corpus — no new store, no writes. See `src/cortex/debt.rs` and `docs/cortex-elegance-gate.md`'s "Consistency-debt trend" section. |
| `cortex_stats` | **deprecated alias** | Returns `{"deprecated":true,"use":"kg_stats",...}`. Call `kg_stats` instead. |
| `cortex_build` | **deprecated alias** | Returns `{"deprecated":true,"use":"scribe_kg_build",...}`. Call `scribe_kg_build` instead. |
| `cortex_deps` | **deprecated alias** | Returns `{"deprecated":true,"use":"kg_neighbors",...}`. Call `kg_neighbors` instead. |
| `cortex_recent` | **deprecated alias** | Returns `{"deprecated":true,"use":"kg_query",...}`. Call `kg_query` instead. |
| `cortex_community` | **deprecated alias** | Returns `{"deprecated":true,"use":"kg_communities",...}`. Call `kg_communities` instead. |
| `cortex_architecture` | **deprecated alias** | Returns `{"deprecated":true,"use":"kg_communities",...}`. Call `kg_communities` instead. |
| `cortex_flows` | **deprecated alias** | Returns `{"deprecated":true,"use":"kg_path",...}`. Call `kg_path` instead. |

The original 10 tool NAMES from before CXEG-01 all stay registered (no
MCP-listing churn for a caller that enumerates tools), and CXEG-06 adds one
NEW name (`cortex_house_style`) on top — 11 total. 7 of the original 10 do
**zero I/O** — no network, no SSH, no filesystem, no database — they only
build and return a small JSON pointer object (`src/cortex/deprecated.rs`).

## `project_id`, not `repo`

The old fixed two-repo-name allowlist (`"lumina-fleet"` / `"lumina-terminus"`)
named two repos on the retired fleet-host layout. `cortex_scope` and
`cortex_review` are now keyed by `project_id` instead, validated against
`crate::cortex::PROJECT_IDS` (`src/cortex/mod.rs`):

```
TERM, LUM, HARM, CHRD, RAIL
```

This is the same `project_id` vocabulary the Atlas KG (`kg_*`) tools already
use, and matches the current Plane-project-prefix convention (Terminus, Lumina,
Harmony, Chord, Civic-Rail). Any other value is rejected with
`ToolError::InvalidArgument` before any graph/scoring work happens.

## `cortex_scope` — live, Atlas-backed blast radius (CXEG-02)

The pipeline's pre-dispatch scoping call: "if I touch these files, what else
might I break, and how much of the project can I safely ignore?"

**Input schema**: `project_id` (enum, required, one of `PROJECT_IDS`), plus
EITHER `changed_files` (a comma-separated string OR a JSON array of file-path
strings — the comma-separated form is kept for backward compatibility with the
CXEG-01 stub's original schema) OR `diff` (a unified diff; changed files are
parsed from its `+++ b/<path>` headers). At least one of `changed_files`/`diff`
must yield a non-empty file list.

**Oversized input truncates, it does not error** (CXEG-02 reconciliation of
count-cap vs abuse-reject): an input with *more files* than the file-count cap
(`MAX_CHANGED_FILES`, 200) — a long CSV/array, or a many-file diff — is
truncated to the cap and flagged with `truncated:true` + a `tracing::warn!`, so
an ordinary big change degrades gracefully. `InvalidArgument` is reserved for
genuinely abusive/malformed input only:
- a **single** path element longer than `MAX_TEXT_LEN` (2000) chars — one
  absurd path/blob, not "too many files";
- a **DoS-scale** raw `changed_files` string or `diff` exceeding `MAX_DIFF_LEN`
  (5,000,000) chars. For a `diff` this ceiling is checked ONLY when the parse
  did not already truncate by file count — so a big *many-file* diff truncates
  rather than being rejected; rejection is reserved for a pathologically huge
  *single blob* (few files, enormous content);
- a `changed_files` array with more than `MAX_CHANGED_FILES_ARG` (5000)
  elements — a DoS ceiling set far above the file-count cap, so arrays between
  the cap and this ceiling truncate rather than reject.

**Reuse**: both the CSV/array/diff parsing and the graph queries are shared
with `review_run`'s KGREV-01 grounding and the `kg_*` tools, not reimplemented:
- `crate::review::kg_context::derive_changed_files_counted` does the actual
  `diff`/array parsing (`src/cortex/scope.rs`'s `changed_files_from_args` only
  adapts `cortex_scope`'s own CSV-string/array argument shapes into the
  `{"changed_files"|"diff": ...}` value it expects, and surfaces its
  `input_truncated` signal). `derive_changed_files` is the thin `Vec`-only
  wrapper KGREV-01 callers still use, unchanged.
- The 1-hop caller/callee walk uses `crate::scribe::graph::query::one_hop_neighbors`
  — the SAME single-source helper `kg_neighbors` (`src/scribe/graph/tools.rs`)
  now calls, so there is exactly one place a node's incident edges are
  iterated. Graph load + touched-node resolution use the same
  `scribe::graph::store::GraphStore` / `KnowledgeGraph` API
  `review::kg_context::build_kg_block` and the other `kg_*` tools use.
- Node resolution (touched nodes AND neighbors) is filtered to the **current**
  bi-temporal view (`valid_to.is_none()`, via `current_nodes()` / an explicit
  filter), matching the other live-view tools — a since-removed (invalidated)
  symbol never appears in a live blast radius.

**Behavior**:
1. Validates `project_id` (`InvalidArgument` if not one of `PROJECT_IDS`).
2. Derives `changed_files` from the input (`InvalidArgument` if both
   `changed_files` and `diff` are absent/empty).
3. Loads the project's Atlas graph. If none is stored yet (`scribe_kg_build`
   hasn't run for this `project_id`, or the store itself failed to load),
   returns a `"configured": false` response with each entry of
   `changed_files` echoed back into `blast_radius` as an unresolved literal
   entry — **never an error**, so a dispatch caller always gets a usable
   answer even against an unindexed project.
4. Otherwise, resolves each changed file to the current graph nodes it
   defines (`role: "touched"`), any changed file with no matching node is
   ALSO echoed back as an unresolved literal entry (e.g. a brand-new file),
   then walks the 1-hop callers/callees of every touched node
   (`role: "caller"`/`"callee"`), collecting each resolved node's community
   (`cluster`) into `affected_communities`.
5. Computes `token_reduction_pct` as `1 - (blast-radius node-card bytes /
   total-project node-card bytes) * 100`, clamped to `[0, 100]` — the same
   `node_card` text `scribe_kg_build`'s embedding pipeline embeds
   (`crate::scribe::graph::vec_embed::node_card`), used here as a proxy for
   "how much smaller is the context a model needs to read than the whole
   project."
6. Sets `"truncated": true` (plus a distinct `tracing::warn!`) for EITHER of
   two independent caps — never a silent drop:
   - **input-file cap**: the raw `changed_files`/`diff` input exceeded
     `MAX_CHANGED_FILES` (the shared `review::kg_context` limit) and files
     were dropped before scoping;
   - **blast-node cap**: the walk would enumerate more than
     `CORTEX_MAX_BLAST_NODES` nodes (see "Configuration" below) and stopped.

**Response shape** (live graph):

```json
{
  "configured": true,
  "project_id": "TERM",
  "changed_files": ["src/cortex/mod.rs"],
  "blast_radius": [
    { "id": "crate::cortex::validate_project_id", "path": "src/cortex/mod.rs", "kind": "function", "resolved": true, "role": "touched" },
    { "id": "crate::cortex::CortexScope::execute", "path": "src/cortex/mod.rs", "kind": "function", "resolved": true, "role": "caller" }
  ],
  "affected_communities": [1],
  "blast_count": 2,
  "token_reduction_pct": 92.5
}
```

**Response shape** (no stored graph — degrade), also showing `truncated`:

```json
{
  "configured": false,
  "project_id": "TERM",
  "changed_files": ["src/cortex/mod.rs"],
  "blast_radius": [
    { "id": "src/cortex/mod.rs", "path": "src/cortex/mod.rs", "kind": "file", "resolved": false, "role": "touched" }
  ],
  "affected_communities": [],
  "blast_count": 1,
  "token_reduction_pct": 0.0,
  "truncated": true
}
```

**Every response field:**

| Field | Type | Meaning |
| --- | --- | --- |
| `configured` | bool | `true` when a stored Atlas graph was loaded and walked; `false` on the degrade path (no graph stored for the project, or the store failed to load) — see the degrade contract in step 3. Not an error either way. |
| `project_id` | string | Echo of the validated input `project_id` (one of `PROJECT_IDS`). |
| `changed_files` | array of strings | Echo of the derived changed-file list actually scoped (post-parse, post-`MAX_CHANGED_FILES` cap). |
| `blast_radius` | array of objects | The affected nodes (see the per-entry fields below). On the degrade path, one unresolved entry per `changed_files` item. |
| `blast_radius[].id` | string | The graph node id (fully-qualified symbol) when `resolved:true`; the literal file path when `resolved:false`. |
| `blast_radius[].path` | string | The node's repo-relative source path (`resolved:true`), or the file path itself (`resolved:false`). |
| `blast_radius[].kind` | string | The node kind (`function`/`struct`/`enum`/`trait`/`class`/`module`/`doc_section`) when `resolved:true`; the literal `"file"` when `resolved:false`. |
| `blast_radius[].resolved` | bool | `true` if the entry resolved to a current graph node; `false` for a literal changed file with no matching (current) node — e.g. a brand-new/unindexed file, or the whole degrade path. |
| `blast_radius[].role` | string | `"touched"` (a changed file / a symbol defined in one), `"callee"` (a 1-hop outgoing neighbor of a touched symbol), or `"caller"` (a 1-hop incoming neighbor). A node reachable as both is labeled `"caller"`. |
| `affected_communities` | array of ints | The distinct Leiden community/cluster ids (KGRAPH-05) of every resolved node in the blast radius, sorted ascending. Empty on the degrade path (no resolved nodes). |
| `blast_count` | int | Count of distinct affected nodes = `blast_radius.len()` (each `id` is unique within the array). |
| `token_reduction_pct` | float | `1 − (blast-radius node-card bytes / whole-project node-card bytes)`, ×100, clamped `[0,100]`, rounded to 2 dp. `0.0` when there is no resolved blast radius to compare against (empty graph, or an all-unresolved radius — a wholly-unresolved radius must not read as "100% reduction"). |
| `truncated` | bool (present only when `true`) | Emitted (and logged via a distinct `tracing::warn!`) when EITHER cap fired: the **input-file cap** (the input file list/diff exceeded `MAX_CHANGED_FILES` and files were dropped before scoping — a long CSV/array or a many-file diff; fires on the live AND degrade paths) or the **blast-node cap** (`CORTEX_MAX_BLAST_NODES`, the walk stopped enumerating). Oversized-by-count input truncates here rather than erroring. Absent when neither cap fired — never a silent cap. |

**Error/edge cases**: `InvalidArgument` is reserved for abusive/malformed
input (see "Oversized input truncates" above) — an unknown `project_id`; a
**single** `changed_files` path element (or CSV token) over `MAX_TEXT_LEN`
(2000) chars; a DoS-scale raw `changed_files` string or `diff` over
`MAX_DIFF_LEN` (5,000,000) chars (the `diff` ceiling only when the parse did
not already truncate by file count); a `changed_files` array over
`MAX_CHANGED_FILES_ARG` (5000) elements; or neither `changed_files` nor `diff`
yielding any file. Note an oversized-*by-file-count* list/diff is NOT here — it
truncates with `truncated:true` (above). A missing/unloadable Atlas graph is
also NOT an error (see step 3 above) — that is the one deliberate exception to
"validate first, then act" in this tool, since blast-radius unavailability is a
data-availability fact, not a caller mistake.

## `cortex_house_style` — live, Atlas-derived house-style exemplars (`src/cortex/house_style.rs`, CXEG-06)

"How does THIS codebase do X" instead of generic opinion, for a future Tier-C
reviewer (CXEG-07) — derives, per project and per **Leiden community**
(KGRAPH-05, i.e. one of the graph's `cluster` ids, never the whole project),
the community's MODAL patterns plus a few representative EXEMPLARS.
House-style is deliberately **community-scoped**: a `pg/` subsystem and a
`cortex/` subsystem can legitimately favor different idioms, so this tool
never computes a single project-wide style.

**Input schema**: `project_id` (enum, required, one of `PROJECT_IDS`),
`community` (integer, optional — when omitted, up to 25 communities are
returned, ascending cluster-id order; `MAX_HOUSE_STYLE_COMMUNITIES` in
`src/cortex/mod.rs`). A non-integer `community` is `InvalidArgument`.

**No source-text inspection, no LLM**: every [`ModalFacts`] signal is derived
purely from `KgNode` METADATA already in the graph (`kind`, `name`, `path`) —
matching every other Atlas-consuming module in this crate (`cortex::scope`,
`cortex::metrics`, `review::kg_context`), none of which re-reads source off
disk:
- `dominant_kind` — the community's most common `NodeKind` (ties broken
  toward the declaration-order-earlier kind, deterministic).
- `dominant_error_type` — a struct/enum member whose name contains `"Error"`
  (lowest node id wins a tie), or `None`.
- `config_read_idiom` — `"from_env"` when the community has a function member
  named exactly `from_env` (this crate's own config-constructor convention —
  see `CortexConfig::from_env`, `EmbedClient::from_env`,
  `AtlasVecStore::from_env`), else `"none_detected"`.
- `rust_tool_shape_present` — `true` when some single file among the
  community's members defines all 4 `RustTool` method names
  (`name`/`description`/`parameters`/`execute`).

**Exemplar selection**: for each `(community, kind)` bucket, every member's
`node_card` (`crate::scribe::graph::vec_embed::node_card` — the SAME text
builder `metrics`'s semantic-duplication detector and `scribe_kg_build`'s
embedding pipeline use, built from the member's current 1-hop callers/callees
via the shared `one_hop_neighbors` walk) is embedded (`EmbedClient`), the
batch is averaged into a centroid vector, and members are ranked by cosine
similarity to that centroid (ties: `rank` desc, then `id` asc) — nearest to
the bucket's own MODAL shape, not an arbitrary/extreme example. The top `K`
(`CortexConfig::house_style_exemplars_k`, see "Configuration" below) become
that kind's `exemplars_by_kind` entries.

**Degrade contract — no silent misrepresentation**:
- A community with fewer than `house_style::MIN_COMMUNITY_SIZE` (2) CURRENT
  members is too small a sample to trust at all: `"profile": "unstable"`, no
  exemplars, `facts` is the all-`None`/`"none_detected"`/`false` default.
- A `(community, kind)` bucket with fewer than `house_style::MIN_BUCKET_SIZE`
  (3) members still returns what exists, but the profile is flagged
  `"sparse": true`.
- When the embeddings endpoint/vector batch call fails or is unusable for a
  bucket, that bucket's selection falls back to centrality-only ranking
  (`rank` desc, then `degree` desc, then `id` asc) and the profile is flagged
  `"degraded": true` — every OTHER bucket in the same profile is unaffected.
- Every distribution is filtered to CURRENT nodes only
  (`graph.current_nodes()`, KGRAPH-15 bi-temporal view) — an invalidated
  symbol never appears as an exemplar or skews a modal fact.
- A missing/unloadable Atlas graph degrades to `"configured": false` (never
  an error), same posture as `cortex_scope`.

**Caching**: profiles are memoized in-process per `(project_id, community)`
(`house_style::HouseStyleCache`, one shared `Arc` built once in `register()`),
keyed additionally by the graph's `build_seq` — KGRAPH-15's monotonic
per-project refresh counter, the closest thing the model exposes to a build
"generation." A cache hit requires the stored generation to match the
CURRENT graph's `build_seq`; any other generation recomputes and overwrites
the entry, so a `scribe_kg_build` rebuild transparently invalidates every
profile computed against the prior graph — no explicit eviction pass needed.

**Response shape** (live graph):

```json
{
  "configured": true,
  "project_id": "TERM",
  "generation": 3,
  "profiles": [
    {
      "project_id": "TERM",
      "community": 1,
      "generation": 3,
      "member_count": 8,
      "profile": "ready",
      "sparse": true,
      "degraded": false,
      "facts": {
        "dominant_kind": "function",
        "dominant_error_type": "PgError",
        "config_read_idiom": "from_env",
        "rust_tool_shape_present": true
      },
      "exemplars_by_kind": {
        "function": [
          { "node_id": "crate::pg::Config::from_env", "kind": "function", "path": "src/pg/config.rs", "span": [10, 22], "rank": 0.42, "score": 0.9123 }
        ],
        "enum": [
          { "node_id": "crate::pg::PgError", "kind": "enum", "path": "src/pg/error.rs", "span": [1, 15], "rank": 0.31, "score": 0.31 }
        ]
      }
    }
  ]
}
```

**Response shape** (no stored graph — degrade):

```json
{
  "configured": false,
  "project_id": "TERM",
  "message": "no stored Atlas graph for this project yet -- run scribe_kg_build first"
}
```

**Error/edge cases**: `InvalidArgument` for an unknown `project_id` or a
non-integer `community`. A missing/unloadable Atlas graph is NOT an error
(see the degrade contract above).

## Tier-B structural-elegance signals (`src/cortex/metrics.rs`, CXEG-03)

`metrics::compute_signals` is a standalone, PURE (no LLM) scoring library
that turns a `cortex_scope` blast radius into named structural-elegance
findings from the Atlas graph — "does this change quietly make the codebase
worse-shaped," independent of correctness. As of **CXEG-04** it is the
structural half of `cortex_review`'s `risk_score` (see the `cortex_review`
section below for how its `EleganceSignal`s are weighted into the score); it
remains fully unit-tested and independently importable as
`crate::cortex::metrics::compute_signals`.

**Entry points**:
- `compute_signals(touched_node_ids, graph, project_id, config) -> Vec<EleganceSignal>`
  (async) — the full pipeline, including the one I/O-bound detector
  (`semantic_duplication`).
- `compute_structural_signals(touched_node_ids, graph, config) -> Vec<EleganceSignal>`
  (sync) — the four non-I/O detectors only, for callers/tests that don't want
  an async runtime or a vector-store dependency.

`touched_node_ids` are the blast radius's `role == "touched"` node ids
(`cortex_scope`'s output); ids that don't resolve to a CURRENT graph node
(unindexed file, or a since-invalidated symbol) are silently skipped.

**Signal catalog** — every `EleganceSignal` carries `kind`, `severity`
(relative "how far past the trigger," rounded to 4 decimals), `anchor_node`
(always a touched node, never a bystander neighbor), `anchor_file`, a
non-empty deterministic `why`, and signal-specific `evidence`:

| `kind` | Fires when | Notes |
| --- | --- | --- |
| `centrality_spike` | A touched node's PageRank **and** degree both exceed the project's own `tier_b_percentile`-th percentile cut-point (god-object drift). | Both metrics must exceed their own cut-point independently — a node that's merely high-rank OR merely high-degree doesn't fire. |
| `community_boundary_crossing` | A touched node has a 1-hop edge into a different Leiden community, and that community pair has no OTHER edge crossing it elsewhere in the graph (i.e. this change is the sole/first carrier of that coupling). | Baseline is computed from the WHOLE current graph, so an already-established (≥2 independent crossing edges) coupling between two communities is never re-flagged. |
| `semantic_duplication` | A touched node's card (`vec_embed::node_card` — same builder `scribe_kg_build`'s embedding pipeline uses) has an existing, DIFFERENT node whose embedding cosine similarity is `>= config.dup_cosine` (default 0.85). | The only signal that does I/O: embeds via `EmbedClient` and queries `AtlasVecStore::query_topk` — the exact path `kg_semantic_search` uses. Silently absent (not an error) when the vector store/embeddings endpoint is unconfigured or unreachable; every other signal still computes. |
| `complexity_spike` | A touched node's line-span size (`end - start + 1`) exceeds the project's own percentile cut-point. | `KgNode` has no dedicated complexity metric yet — span size is documented as an explicit proxy. A node with no `span` is skipped (nothing to measure), never treated as zero. |
| `fan_out_explosion` | A touched node's out-degree (via the shared `one_hop_neighbors(.., NeighborFilter::Out)` walk) exceeds the project's own percentile cut-point. | Out-degree specifically, not total (in+out) `degree` — a node with many callees but few callers reads differently from `centrality_spike`. |

**Self-calibrating thresholds**: `centrality_spike`/`complexity_spike`/
`fan_out_explosion` all compare against a cut-point computed from the
PROJECT'S OWN current-node distribution (`percentile_cutoff`, nearest-rank
method, at `config.tier_b_percentile`), never a hardcoded absolute — the
same absolute PageRank value fires in a repo where it's an outlier and does
NOT fire in a repo where it's the median. The comparison is strict
greater-than (not `>=`): a value that merely EQUALS the cut-point (e.g. a
uniform distribution where every node shares the same value) never fires,
since there is no outlier there by construction.

**Bi-temporal filtering**: every distribution (`graph.current_nodes()`) and
every anchor/neighbor lookup (`get_node(..).filter(|n| n.valid_to.is_none())`)
is restricted to CURRENT nodes — an invalidated symbol never appears in a
signal or skews a cut-point (a CXEG-02 review finding, front-loaded here).

**Determinism**: signals are sorted by `(kind, anchor_node)` before being
returned, and every numeric score is rounded to 4 decimal places — the same
graph + blast radius + config always produces byte-identical output.

**Configuration**: reuses `CortexConfig` (`src/cortex/mod.rs`) —
`dup_cosine` for `semantic_duplication`, plus a new `tier_b_percentile`
field (`CORTEX_TIER_B_PERCENTILE`, default `90.0`) shared by the three
percentile-based detectors. See the "Configuration" section below.

## `cortex_review` — live, Atlas-backed risk scoring (CXEG-04)

The pipeline's post-change risk gate: "given what I just changed, how much
review scrutiny does it deserve?" `cortex_review` combines CXEG-03's
structural-elegance signals with KGFIND-01 recurrence into one transparent,
deterministic `risk_score`.

**Input schema**: identical shape to `cortex_scope` (`src/cortex/mod.rs`'s
`validate_and_parse_changed_files` is the SAME helper both tools call, S9
single-source — extracted from `cortex_scope`'s own CXEG-02 validation so the
two tools can never silently diverge in what they accept) — `project_id`
(enum, required, one of `PROJECT_IDS`), plus EITHER `changed_files` (a
comma-separated string OR a JSON array) OR `diff` (a unified diff; changed
files parsed from `+++ b/<path>` headers). Same DoS-ceiling/truncation
reconciliation as `cortex_scope` (see that section above) — an
oversized-*by-file-count* input truncates with `truncated:true`; only
genuinely abusive/malformed input (`MAX_TEXT_LEN`/`MAX_DIFF_LEN`/
`MAX_CHANGED_FILES_ARG`) is rejected.

**Behavior**:
1. Validates `project_id` and derives `changed_files` exactly like
   `cortex_scope` (steps 1-2 there).
2. Loads the project's Atlas graph (`GraphStore`). If none is stored yet, or
   the store fails to load, returns the degrade response below — **never an
   error**.
3. Resolves `changed_files` to their CURRENT touched Atlas node ids (same
   resolution `cortex_scope` uses for `role: "touched"` entries).
4. Computes CXEG-03's full structural-signal pipeline over those touched
   nodes: `metrics::compute_signals(touched_node_ids, graph, project_id,
   config)` — all five signal kinds, including the I/O-bound
   `semantic_duplication` (silently absent if the vector store/embeddings
   endpoint is unavailable — see the Tier-B section above).
5. Looks up KGFIND-01 recurrence for the SAME touched scopes via
   `scribe::graph::findings_store::FindingsStore` — the identical store the
   `kg_findings` query tool reads (S9: no second findings access path):
   `scope_kind = "node"` rows whose `scope_ref` is a touched node id,
   `"path"` rows whose `scope_ref` is a changed file, and `"community"` rows
   whose `scope_ref` is an affected community id (from the touched nodes'
   `cluster`s). `FindingsStore::list` has no server-side `scope_ref` filter,
   so each scope kind's bucket is listed once and matched client-side
   against the touched sets; matches are summed into a `(category,
   total_occurrences)` map.
6. Combines both into a `RiskScore` via the pure `review::score` function
   (see "Scoring rubric" below), and returns the full response shape below.

**Scoring rubric** (`review::score(signals, recurrence, config) -> RiskScore`,
`src/cortex/review.rs` — pure, sync, fully unit-tested with synthetic
inputs, no I/O):

- **Structural contribution**: every `EleganceSignal` contributes
  `weight(kind) * severity` points, where `weight(kind)` is one of
  `CortexConfig`'s `risk_weight_*` fields (see "Configuration" below) and
  `severity` is the signal's own CXEG-03 relative-severity value.
- **Recurrence contribution**: every `(category, total_occurrences)` bucket
  from step 5 above contributes `risk_weight_recurrence *
  log2(1 + total_occurrences)` points — **log-scaled, not linear**, so one
  pathologically-recurring finding bucket (e.g. 1000 occurrences) cannot
  alone pin the score at the ceiling the way a linear sum would (log2(1001)
  ≈ 9.97 vs. log2(2) = 1 — roughly 10x growth for 1000x more occurrences).
- **Raw score**: the sum of every contribution's `points` (unclamped). Every
  contribution is returned in `contributions` (`{source, weight, points}`),
  so a caller can always reconstruct this raw value exactly by summing
  `points` — nothing about the score is hidden or lossy, even past the
  ceiling.
- **`risk_score`**: the raw score clamped to `[0, 10]`, rounded to 4
  decimals.
- **`band`**: `"low"` if `risk_score < risk_band_elevated_cut` (default
  `4.0`); `"elevated"` if `< risk_score_threshold` (default `7.0`);
  otherwise `"high"`. Both cut-points are inclusive at their lower bound
  (`>=`), so a value exactly AT a cut-point always resolves to the HIGHER
  band, deterministically — never ambiguous.
- **`recommendation`**: a fixed, non-empty string per band. `"high"`
  reads *"escalate review rigor: request an additional reviewer and a
  closer read of the flagged risk_signals before merge — this is a signal
  to escalate scrutiny, never an automatic rejection."* **No band ever
  recommends rejecting/blocking a change** — auto-reject is explicitly out
  of this item's scope.
- **Determinism**: `signals` arrives pre-sorted by `(kind, anchor_node)`
  (CXEG-03's `sort_signals`); `recurrence` is sorted by `category` inside
  `score` regardless of caller order. The same signals+recurrence+config
  always produce byte-identical `contributions`/`risk_score`/`band`.

**Response shape** (live graph, structural signals fired, findings store
reachable with a recurring match):

```json
{
  "configured": true,
  "project_id": "TERM",
  "changed_files": ["src/hub.rs"],
  "risk_score": 6.7342,
  "band": "elevated",
  "risk_signals": [
    {
      "kind": "centrality_spike",
      "severity": 1.6122,
      "anchor_node": "crate::hub::Hub",
      "anchor_file": "src/hub.rs",
      "why": "crate::hub::Hub has PageRank 0.9000 (above the project's 90th-percentile cut-point 0.0345) and degree 15 (above the 90th-percentile cut-point 1) — a touched god-object-shaped hub, not a typical leaf/utility node.",
      "evidence": { "rank": 0.9, "rank_cutoff": 0.0345, "degree": 15, "degree_cutoff": 1, "percentile": 90.0 }
    }
  ],
  "contributions": [
    { "source": "centrality_spike", "weight": 2.0, "points": 3.2244 },
    { "source": "recurrence:complexity_debt", "weight": 1.0, "points": 3.5098 }
  ],
  "findings": "ok",
  "recommendation": "apply standard review rigor with attention to the flagged risk_signals; no escalation required."
}
```

**Response shape** (no stored Atlas graph — degrade):

```json
{
  "configured": false,
  "project_id": "TERM",
  "changed_files": ["src/hub.rs"],
  "risk_score": 0.0,
  "band": "unknown",
  "risk_signals": [],
  "contributions": [],
  "findings": "unavailable",
  "recommendation": "insufficient data to assess risk for this change."
}
```

`findings` also appears as `"unavailable"` (structural signals still
computed and returned in full — just no recurrence term) when the Atlas
graph loads fine but the KGFIND store is unconfigured/unreachable/erroring,
and as `"empty"` (distinct from `"unavailable"`) when the store is reachable
but nothing in the touched scopes has a recorded finding — a caller must not
conflate "recurrence wasn't checked" with "recurrence was checked and found
nothing."

**Every response field:**

| Field | Type | Meaning |
| --- | --- | --- |
| `configured` | bool | `true` when a stored Atlas graph was loaded; `false` on the degrade path (no graph stored/loadable for the project) — mirrors `cortex_scope`'s own `configured` semantics. |
| `project_id` | string | Echo of the validated input `project_id`. |
| `changed_files` | array of strings | Echo of the derived changed-file list actually scored. |
| `risk_score` | float | `0.0`-`10.0`, clamped, rounded to 4 decimals. `0.0` on the degrade path (not a real "zero risk" assessment — see `band:"unknown"`). |
| `band` | string | `"low"` / `"elevated"` / `"high"`, or `"unknown"` on the degrade path. See the scoring rubric above for the cut-points. |
| `risk_signals` | array of `EleganceSignal` | The full CXEG-03 structural signals fired for this change (see the Tier-B signal catalog above for each field). Empty on the degrade path, or when nothing fired. |
| `contributions` | array of `{source, weight, points}` | Every scoring term: one entry per fired structural signal (`source` = its `kind`) plus one per recurring finding category (`source` = `"recurrence:<category>"`). Summing every `points` reconstructs the raw pre-clamp score exactly. |
| `findings` | string | `"ok"` (recurrence looked up, at least one match), `"empty"` (looked up, no match), or `"unavailable"` (KGFIND store unconfigured/unreachable/erroring, OR the whole response is on the graph-unavailable degrade path). |
| `recommendation` | string | Always non-empty. Only ever escalates review rigor for `"high"`; never recommends rejection/blocking. |
| `truncated` | bool (present only when `true`) | Same input-file-cap semantics as `cortex_scope` — present when the raw `changed_files`/`diff` input exceeded `MAX_CHANGED_FILES` and was truncated before scoring. |

**Error/edge cases**: same `InvalidArgument` conditions as `cortex_scope`
(unknown `project_id`; an over-`MAX_TEXT_LEN` single element; a DoS-scale
`changed_files`/`diff`; an over-`MAX_CHANGED_FILES_ARG` array; or neither
`changed_files` nor `diff` yielding any file). A missing/unloadable Atlas
graph and an unavailable/erroring KGFIND findings store are explicitly NOT
errors — both degrade to a labeled response (see above).

**Reuse (S9 single-source)**: argument validation/parsing
(`validate_and_parse_changed_files`), the structural signal engine
(`metrics::compute_signals`), and the findings store (`FindingsStore`, the
same one `kg_findings` reads) are all reused verbatim, not reimplemented —
`cortex_review` adds only the scoring/combination logic
(`src/cortex/review.rs`'s `score`/`touched_recurrence`/`compute_review`).

**Internal caller**: `crate::scribe::graph::cortex_bridge` (KGRULE-05) already
looks for a top-level `risk_score` field (documented 0-10 scale, rescaled to
`[0,1]`) in `cortex_review`'s response to attach a best-effort risk signal to
KG rule crystallization — see its own module doc. `cortex_review`'s response
shape as of CXEG-04 satisfies that contract with no code change needed there.

## `review_run`'s Stage-5b risk-gate escalation + `cortex_waive` (CXEG-08)

CXEG-04 deliberately scoped "what happens on a `high` band" out of
`cortex_review` itself (`recommendation_for` only ever suggests escalating
rigor). CXEG-08 is that governance layer: `review_run` widens its dispatched
provider panel when a change's `cortex_review` band is `"high"`, unless an
active waiver says otherwise — **no new risk scoring**, purely governance
around the existing `risk_score`/`band`.

**Where it runs.** `review::mod::maybe_escalate` runs BEFORE the provider
`JoinSet` is spawned (unlike CXEG-07's consistency lens, which runs strictly
AFTER `aggregate()`). Its only effect is appending a provider name to the
`providers` list that dispatch is about to use — it never touches
`aggregate_verdict`/`complete`. This is why risk cannot flip a verdict: the
escalation logic isn't an input to `aggregate()` at all, only to which
providers get asked.

**Escalation decision** (`CortexConfig.escalation_enabled` /
`CORTEX_ESCALATION_ENABLED`, default `true`; `escalation_add_provider` /
`CORTEX_ESCALATION_ADD_PROVIDER`, default `"agy"`):

1. Disabled, no `context.project_id`, or no derivable `changed_files` → no
   escalation.
2. Calls `cortex_review::compute_review` for the change. A `band` other than
   `"high"` (including `"unknown"` on an ungraphed project — `cortex_review`
   itself never errors) → no escalation. **This is the fail-open contract**:
   `cortex_review` being unavailable/unconfigured is indistinguishable, from
   this gate's perspective, from "not risky" — the correctness gate always
   proceeds on the panel's own verdict alone.
3. Looks up an active waiver via `crate::cortex::waiver::active_waiver`
   for rule `"cortex_review_high_band"` (`waiver::HIGH_RISK_BAND_RULE`) and
   this change's `changed_files` (joined, as the requested scope). A waiver
   lookup failure (store unconfigured/unreachable) is treated as "no active
   waiver found" — logged, never propagated as an error; the gate does not
   distinguish "confirmed unwaived" from "couldn't check," by design (see
   `src/cortex/waiver.rs`'s module doc, "Fail-open, always").
4. An active, matching, non-expired waiver → no escalation;
   `"waived": true` plus the waiver's `{finding_id, rule, scope, reason,
   author, expiry, broad}` in the result. An EXPIRED waiver does not
   suppress (checked against the waiver's own recorded `expiry`, not
   `review_run`'s call time in any other sense).
5. `structure == "adversarial_pair"` → `"escalated": true`,
   `"escalation_degraded": true`, panel untouched (a fixed 2-provider
   defend/attack shape can't be widened without misassigning `Role::Defend`/
   `Role::Attack`).
6. `escalation_add_provider` invalid, or the panel already at
   `MAX_PROVIDERS` (5) without already containing it → `"escalated": true`,
   `"escalation_degraded": true`, panel untouched. Escalation degrading can
   never deadlock dispatch.
7. Otherwise: `providers` gains exactly one entry (a no-op if the
   add-provider is already present); `"escalated": true`.

After dispatch, `review::mod::finalize_escalation` additionally sets
`"escalation_degraded": true` if the ADDED provider's own `ProviderResult`
came back with an `error` (same per-provider degrade path every other
provider already goes through, e.g. no `REVIEW_DAEMON_TOKEN` configured) —
so a caller can distinguish "we tried to widen the panel but that reviewer
didn't answer" from "we didn't try."

**`review_run` result shape** (new, additive `"escalation"` field):

```json
"escalation": {
  "escalated": true,
  "band": "high",
  "risk_score": 8.2,
  "waived": false,
  "escalation_degraded": false,
  "reason": "high band; panel widened by one provider",
  "advisory_only": true,
  "added_provider": "agy"
}
```

`"waiver": {...}` replaces `"added_provider"` when an active waiver
suppressed escalation. `"advisory_only": true` is always present, mirroring
CXEG-07's `consistency` block — a reminder this field never altered
`aggregate_verdict`/`complete`.

### `cortex_waive` — record a risk-gate waiver

**Input schema**: `project_id` (enum, required, one of `PROJECT_IDS`), `rule`
(string, required — the gate rule id, e.g. `"cortex_review_high_band"`),
`reason` (string, required, **MUST be non-blank** — `InvalidArgument` if
empty/whitespace-only), `scope` (string, optional, default `"*"` — `"*"` for
project-wide or a comma-separated file-path set), `author` (string, optional,
default `"unknown"`), `expiry` (string, optional — an RFC3339 timestamp;
`InvalidArgument` if present but not valid RFC3339).

**Storage (S9, no new database)**: recorded as a `category:"waiver"` finding
on the SAME KGFIND-01 `FindingsStore` every other `review_run` finding uses —
`scope_kind: Global`, `scope_ref: project_id`, `description:
"waiver[<rule>]: <reason>"` (the `rule` is folded into the description, not
just `provenance`, so two waivers with the same reason text but different
rules never collide in the store's exact-description dedup bucket). The
`(rule, scope, reason, author, expiry, waived_at)` tuple lives in the
finding's `provenance` JSON; repeating the identical `(rule, reason)` bumps
`occurrences` and merges provenance (the store's existing exact-description
dedup, unmodified) rather than duplicating a row — so over-waiving the same
thing repeatedly is visible as a high-`occurrences` finding, not silently
free.

**Scope coverage** (`waiver::scope_covers`, pure/unit-tested,
`src/cortex/waiver.rs`): a waiver's `scope` "covers" a requested scope when
`scope == "*"`, or when every path in the requested (comma-separated) set is
also in the waiver's set. Coverage is allowed to be broader than what's asked
— a project-wide `"*"` waiver covers a single-file change — but
`active_waiver` flags `"broad": true` on the match when the waiver's scope is
strictly larger than the requested one, so over-broad waivers stay visible
rather than silently accepted as "just right."

**Response shape**:

```json
{
  "recorded": true,
  "created": true,
  "waiver_id": "3f9c2e1a-...",
  "occurrences": 1,
  "project_id": "TERM",
  "rule": "cortex_review_high_band",
  "scope": "*",
  "reason": "accepted risk for the S115 sprint, revisit after CXEG-10 calibration",
  "author": "<operator>",
  "expiry": "2026-08-01T00:00:00Z"
}
```

`"created": false` (with `"occurrences" > 1`) when the identical `(rule,
reason)` waiver was already recorded and this call just bumped it.

**Error cases**: unknown `project_id`; blank `rule`; blank/missing `reason`;
a present-but-invalid `expiry`. All `InvalidArgument`, checked before any
store I/O.

## `cortex_audit` — external-repo structural-elegance audit (CXEG-11)

**Input schema**: `url` (string, required) — a public git repository URL,
e.g. `"https://github.com/owner/repo"`.

**Clone-feasibility decision (CXEG-11)**: no sanctioned "clone an arbitrary
public URL" tool exists in this crate — `crate::forge`'s `git_public`/
`git_private` tools speak a fixed, credentialed, per-provider REST API
surface (repos/issues/PRs/...) against a configured pool member, never a raw
`git clone <arbitrary-url>`. This tool's designed operation (audit an
operator-supplied external repo) is exactly what a scoped, isolated clone is
*for*, so CXEG-11 rebuilds it on a `std::process::Command`-driven `git clone`
into a scratch directory with guaranteed cleanup, rather than retiring the
tool. This is a narrower, more contained blast radius than the retired
SSH-relay era ever had — that implementation didn't even clone locally, it
shipped the URL to a remote fleet-host script and trusted whatever came back.

**Behavior**: `url` passes through the **unchanged**, SSRF-hardened
`validate_repo_url()` front-gate (`src/cortex/audit.rs` — this function was
not touched by CXEG-01 or CXEG-11; it has no dependency on the deleted SSH
transport or the new clone backend). Only `http`/`https` URLs to public,
non-private/loopback/link-local/metadata hosts are accepted — see `audit.rs`'s
own doc comments for the full numeric-host SSRF-hardening rationale
(decimal-integer, hex, octal-leading-zero, shorthand dotted-quad, and
IPv4-mapped-IPv6 encodings of loopback/private addresses are all rejected,
fail-closed).

Once `url` passes validation, `execute` runs the CXEG-11 pipeline
(`audit::run_audit`):

1. **Isolated scratch clone** (`ScratchClone`): `git clone --depth 1
   --single-branch --no-tags --no-recurse-submodules --config
   core.hooksPath=/dev/null <url> <scratch>/repo`, run with an isolated
   `$HOME`, `GIT_CONFIG_NOSYSTEM=1`, `GIT_CONFIG_GLOBAL=/dev/null`,
   `GIT_TERMINAL_PROMPT=0`, and a no-op `GIT_ASKPASS` (no operator credential
   helper, gitconfig, or stored token is ever reachable from the subprocess;
   an auth-walled URL fails fast instead of hanging). Bounded by
   `CORTEX_AUDIT_CLONE_TIMEOUT_SECS` (default 60s) — past that the subprocess
   is killed. The scratch directory is removed on **every** exit path —
   success, an early error return, or an unwinding panic — via `Drop`.
2. **Size ceiling**: the clone's on-disk size is measured (without following
   symlinks) and checked against `CORTEX_AUDIT_MAX_CLONE_BYTES` (default
   200MB, `InvalidArgument` if exceeded) *before* any graph build is
   attempted.
3. **Static extraction only, no repo code ever executes**: the same
   `walk_rs` (now `pub(crate)`, shared with `scribe_kg_build`) +
   `build_rust_graph` path scans allowlisted-extension files with
   `fs::read_to_string` and parses them with tree-sitter — no build scripts,
   no `cargo`/`npm`/interpreter invocation, no import resolution that would
   need to load foreign code. The resulting graph is clustered (`cluster`)
   and ranked (`pagerank`) exactly like a real `scribe_kg_build`, but is
   **never** passed to `GraphStore::save` and never given a real
   `project_id` — it's transient, in-process, and gone when the function
   returns.
4. **CXEG-03 structural detectors**: every (capped at
   `CORTEX_MAX_BLAST_NODES`) current node is treated as "touched" — a
   whole-repo audit has no diff to scope to — and passed to
   `metrics::compute_structural_signals` (the sync, no-vector-store subset of
   the CXEG-03 engine: `centrality_spike`, `complexity_spike`,
   `fan_out_explosion`, `community_boundary_crossing`).
   `semantic_duplication` is deliberately **not** run here — it compares a
   node's embedding against the PROJECT's own persisted vector-store rows,
   and this graph is intentionally never persisted or embedded (embedding +
   storing vectors for an arbitrary external repo would leak its content
   into local infrastructure state, exactly what "transient" is meant to
   avoid).

Successful response shape:

```json
{
  "status": "complete",
  "tool": "cortex_audit",
  "url": "https://github.com/owner/repo",
  "stats": {
    "nodes": 128,
    "edges": 340,
    "clusters": 6,
    "files_scanned": 41,
    "file_scan_cap_hit": false,
    "signal_scope_cap_hit": false,
    "clone_bytes": 934112
  },
  "signals": [ /* EleganceSignal[] — see metrics.rs / the Tier-B section above */ ],
  "signal_count": 3
}
```

**Error/edge cases**: `InvalidArgument` for any URL rejected by
`validate_repo_url` (empty, oversized, wrong scheme, embedded credentials,
shell metacharacters, whitespace/control chars, or a disallowed host) — all
caught before the clone is even attempted. Also `InvalidArgument` for a clone
that exceeds `CORTEX_AUDIT_MAX_CLONE_BYTES`, or a repo with no
allowlisted-extension source files at all. `Execution` for a clone that fails
outright or exceeds `CORTEX_AUDIT_CLONE_TIMEOUT_SECS`. In every case the
scratch directory is still removed.

## Configuration

`CortexConfig::from_env()` (`src/cortex/mod.rs`) builds one shared
`Arc<CortexConfig>` for all 5 real tools. No SSH/remote-script fields remain.

| Env var | Type | Default | Notes |
| --- | --- | --- | --- |
| `CORTEX_RISK_SCORE_THRESHOLD` | f64 | `7.0` | `cortex_review`'s (CXEG-04) `"elevated"` -> `"high"` band cut-point (a `risk_score` at or above this reads `"high"`). |
| `CORTEX_ENABLE_TIER_B` | bool | `false` | Feature flag for a not-yet-built Tier B analysis pass. Not consumed by `cortex_scope`/`cortex_review`. |
| `CORTEX_ENABLE_TIER_C` | bool | `false` | Feature flag for a not-yet-built Tier C analysis pass. |
| `CORTEX_ELEGANCE_ADVISORY_ONLY` | bool | `true` | Whether elegance/style findings are advisory-only. Not yet consumed by `cortex_review`'s scoring (its `recommendation` is already advisory-only by construction — see the scoring rubric above). |
| `CORTEX_DUP_COSINE_THRESHOLD` | f64 | `0.85` | Cosine-similarity threshold for `metrics::compute_signals`'s `semantic_duplication` detector. Not used by `cortex_audit` (CXEG-11) — that pipeline runs `compute_structural_signals`, the subset with no vector-store dependency. |
| `CORTEX_MAX_BLAST_NODES` | usize | `200` | `cortex_scope`'s (CXEG-02) cap on the number of nodes enumerated into `blast_radius` before it sets `truncated:true` and stops walking. Also reused by `cortex_audit` (CXEG-11) as the cap on how many of a cloned repo's nodes are scored (`signal_scope_cap_hit`). A zero/unparseable value falls back to the default rather than dropping every node. |
| `CORTEX_TIER_B_PERCENTILE` | f64 | `90.0` | `metrics::compute_signals`'s (CXEG-03) percentile cut-point (0-100) for `centrality_spike`/`complexity_spike`/`fan_out_explosion`, computed against the project's own current-node distribution — self-calibrating, never a hardcoded absolute. |
| `CORTEX_HOUSE_STYLE_K` | usize | `3` (`house_style::DEFAULT_EXEMPLARS_K`) | `cortex_house_style`'s (CXEG-06) `K` — exemplars selected per `(community, kind)` bucket. A zero/unparseable value falls back to the default (same "never silently drop everything" reasoning as `CORTEX_MAX_BLAST_NODES`). |
| `CORTEX_RISK_WEIGHT_CENTRALITY_SPIKE` | f64 | `2.0` | `cortex_review`'s (CXEG-04) per-point weight for a `centrality_spike` structural signal (`points = weight * severity`). |
| `CORTEX_RISK_WEIGHT_COMPLEXITY_SPIKE` | f64 | `1.5` | Weight for a `complexity_spike` structural signal. |
| `CORTEX_RISK_WEIGHT_FAN_OUT_EXPLOSION` | f64 | `1.5` | Weight for a `fan_out_explosion` structural signal. |
| `CORTEX_RISK_WEIGHT_COMMUNITY_BOUNDARY_CROSSING` | f64 | `2.5` | Weight for a `community_boundary_crossing` structural signal (severity is always `1.0` for this kind, so this is effectively its flat per-instance point value). |
| `CORTEX_RISK_WEIGHT_SEMANTIC_DUPLICATION` | f64 | `10.0` | Weight for a `semantic_duplication` structural signal — set much higher than the others because this signal's severity (`cosine - dup_cosine`) is bounded to a small `[0, ~0.15]` range by construction, unlike the percentile-relative signals' unbounded-above severities. |
| `CORTEX_RISK_WEIGHT_RECURRENCE` | f64 | `1.0` | Weight applied to each KGFIND recurrence category's log-scaled magnitude (`log2(1 + total_occurrences)`). |
| `CORTEX_RISK_BAND_ELEVATED_CUT` | f64 | `4.0` | `cortex_review`'s `"low"` -> `"elevated"` band cut-point. |
| `CORTEX_AUDIT_CLONE_TIMEOUT_SECS` | u64 | `60` | `cortex_audit`'s (CXEG-11) wall-clock ceiling on the isolated `git clone` of an external audit target; past this the subprocess is killed and the scratch dir is still cleaned up. A zero/unparseable value falls back to the default. |
| `CORTEX_AUDIT_MAX_CLONE_BYTES` | u64 | `200000000` (200MB) | `cortex_audit`'s (CXEG-11) byte ceiling on a cloned external repo, measured after clone and before any graph build; exceeding it is an `InvalidArgument`, not a silent truncation. A zero/unparseable value falls back to the default. |
| `CORTEX_ESCALATION_ENABLED` | bool | `true` | CXEG-08: master switch for `review_run`'s Stage-5b risk-gate escalation (`review::mod::maybe_escalate`). `false` -> byte-for-byte the pre-CXEG-08 dispatch path plus one additive `"escalation": {"escalated": false, "reason": "disabled"}` field. Never gates `cortex_review`/`cortex_waive` themselves. |
| `CORTEX_ESCALATION_ADD_PROVIDER` | string | `"agy"` | CXEG-08: the provider `review_run` appends to the panel on a `"high"` `cortex_review` band (unwaived, room in the panel, not `adversarial_pair`). Must be one of `review::ALLOWED_PROVIDERS`; an invalid value degrades the escalation attempt (`escalation_degraded:true`) rather than erroring the whole call. Blank/unset falls back to the default. |
| `ATLAS_DATABASE_URL` | secret-shaped | none | Read exclusively through `crate::config::atlas_database_url()` — this crate has no separate `SecretManager`/`vault::manager()` API of its own; the runtime secret store is materialized into the process environment at deploy time (same convention as `crate::pki` and `scribe::graph::vec_embed`). `None` means the Atlas KG store is not configured (`cortex_scope`/`cortex_review` still degrade cleanly in this case, via `GraphStore`/`ScribeConfig`'s own `SCRIBE_KG_STORE_DIR`, which is independent of the Postgres DSN; `cortex_review`'s `FindingsStore` connection is a SEPARATE consumer of this same DSN, degrading independently to `findings:"unavailable"`; `cortex_audit`'s transient graph never touches this DSN at all — it is never saved to `GraphStore`). |
| `EMBEDDINGS_URL` / `EMBEDDINGS_MODEL` / `EMBEDDINGS_TIMEOUT_MS` / `EMBEDDINGS_API_KEY` | see `crate::config` / `scribe::graph::vec_embed` | see `crate::config` | Not read by this module directly — `cortex_house_style`'s (CXEG-06) exemplar selection goes through the SAME `EmbedClient::from_env()` `metrics`'s semantic-duplication detector and `scribe_kg_build` already use. An unreachable/misconfigured endpoint degrades that bucket to centrality-only ranking (`degraded:true`), never an error. |

All 7 `risk_weight_*`/band-cut fields are "sane conservative defaults, tunable
in CXEG-10 calibration" — none is derived from a live calibration run yet.

Boolean flags accept `"1"`/`"true"`/`"yes"` (case-insensitive) as truthy;
anything else (including unset) falls back to the default.

## `cortex_stats` / `cortex_build` / `cortex_deps` / `cortex_recent` / `cortex_community` / `cortex_architecture` / `cortex_flows` — deprecation aliases

Each of these 7 tool names is registered (`src/cortex/deprecated.rs`) purely
so a caller using the old name doesn't get a bare "tool not found" — its
`execute` performs **no I/O of any kind** and always returns:

```json
{
  "deprecated": true,
  "use": "<replacement tool name>",
  "message": "'<old name>' was retired in CXEG-01 along with the rest of Cortex's SSH-relay-era transport to the now-retired fleet host. Call '<replacement>' against the in-process Atlas KG instead."
}
```

Replacement map:

| Retired tool | Replacement |
| --- | --- |
| `cortex_stats` | `kg_stats` |
| `cortex_build` | `scribe_kg_build` |
| `cortex_deps` | `kg_neighbors` |
| `cortex_recent` | `kg_query` |
| `cortex_community` | `kg_communities` |
| `cortex_architecture` | `kg_communities` |
| `cortex_flows` | `kg_path` |

These accept any argument shape (their `parameters()` schema is deliberately
permissive, `additionalProperties: true`) since they never inspect their
arguments — the pointer is returned unconditionally.

## Registration

`register()` (`src/cortex/mod.rs`) builds one shared `Arc<CortexConfig>` and
one shared `Arc<house_style::HouseStyleCache>` (CXEG-06 — a single cache
instance across every `cortex_house_style` call, so its per-generation
memoization actually accumulates hits rather than starting empty each call),
registers the 5 real tools against them (`cortex_scope` live as of CXEG-02;
`cortex_house_style` live as of CXEG-06; `cortex_review` live as of CXEG-04;
`cortex_audit` live as of CXEG-11; `cortex_waive` live as of CXEG-08 --
stateless, no shared config needed), then delegates to
`crate::cortex::deprecated::register()` for the 7 aliases. Cortex is wired
into **both** top-level registries in `src/registry.rs`: `register_all` (the
core registry, served by `terminus-primary`/Chord) and `register_personal`
(the personal registry) — unchanged from before CXEG-01.

## `crate::scribe::graph::cortex_bridge` — the one internal caller

`src/scribe/graph/cortex_bridge.rs` (KGRULE-05) calls `cortex_review`
internally to attach a best-effort risk signal to KG findings, via
`extract_risk`'s pure JSON lookup for a top-level (or one-level-nested under
`result`) numeric `risk_score` field, rescaled `0-10 -> 0-1`. As of **CXEG-04**
`cortex_review`'s live response carries exactly that field
(`response.risk_score`, `0.0`-`10.0`), so `cortex_bridge` now returns a real
signal instead of always `None` — with **no code change** in `cortex_bridge.rs`
itself, exactly as its own module doc predicted ("once CXEG-04 lands a real
`risk_score`, this bridge starts returning real signal with no code change
here"). `cortex_bridge`'s degrade contract (returns `None` on any tool error,
or when the response carries no numeric `risk`/`score`/`risk_score` field) is
otherwise unchanged — a `cortex_review` degrade response (`configured:false`
or a findings-unavailable structural-only score) still carries a numeric
`risk_score` (`0.0` on the graph-unavailable path, a real structural-only
value otherwise), so `cortex_bridge` treats both as a legitimate (if low-
confidence) signal rather than `None`.

## Testing notes for this module

`src/cortex/mod.rs`'s test module covers, without any network access:
`project_id` validation (accepts `TERM`/`LUM`/`HARM`/`CHRD`/`RAIL`, rejects
unknowns and the old legacy repo names), free-text length capping,
`cortex_audit`'s SSRF-front-gate `InvalidArgument` rejection paths and
missing-`url` rejection (its real clone→build→report pipeline is covered
hermetically against local git fixtures in `audit::tests`),
`cortex_scope`'s AND `cortex_review`'s shared argument-validation/
wiring via `validate_and_parse_changed_files` (`project_id` rejection; the
count-vs-abuse reconciliation — a long CSV of short paths AND a many-file
diff both TRUNCATE with `truncated:true` rather than erroring, while a single
over-`MAX_TEXT_LEN` element, an over-`MAX_CHANGED_FILES_ARG` array, and a
pathologically huge single-blob `diff` are each rejected; "neither
changed_files nor diff" rejection; array-form and diff-only-form acceptance
for both tools), `cortex_review`'s `configured:false`/`band:"unknown"`
degrade smoke test against an empty store dir, `cortex_scope`'s own
`configured:false` degrade smoke test, `cortex_audit`'s unchanged SSRF-guard
rejections, `cortex_house_style`'s argument validation (unknown `project_id`,
non-integer `community`) and a `configured:false` degrade smoke test against
an empty store dir, and full registration (`register()` yields exactly 14
tool names, all `cortex_*`).
`src/cortex/debt.rs`'s own test module covers `resolve_bucket`'s pure
scope-to-community resolution (global → project-wide, a parseable/
unparseable `community` scope_ref, node/path scopes without a loaded graph
→ unmapped, an unknown scope_kind → unmapped, and deterministic bucket
ordering), `Rollup::absorb`'s pure aggregation math, `compute_debt`'s
`configured:false` degrade without a configured findings store, and the
`cortex_consistency_debt` tool's argument validation (unknown/missing
`project_id`) plus its own degrade smoke test.
`src/cortex/house_style.rs`'s own test module covers the deeper behavior
against a two-community fixture graph (one community with a `PgError` enum, a
`from_env` function, and a full `RustTool`-shape file; one plain community
with neither): `modal_facts` detects the error type/`from_env`
idiom/`RustTool` shape for the first community and their absence for the
second (proving house-style is community-scoped, not global); dominant-kind
tie-breaking is deterministic; `select_exemplars` falls back to
centrality-only ranking (`degraded:true`) when the embeddings endpoint is
forced unreachable (`EMBEDDINGS_URL` pointed at a refused port —
`#[serial_test::serial]`, since a real Chord embeddings proxy IS reachable on
the <host> build host this suite runs on, and a unit test must never silently
round-trip to it) and respects the `K` cap; `compute_profile` returns
`profile:"unstable"` below `MIN_COMMUNITY_SIZE` (and doesn't panic on a
wholly-empty community), flags `sparse:true` for a thin bucket, filters
bi-temporally invalidated nodes out of both facts and exemplars, and is
deterministic; `HouseStyleCache` returns a STALE (proven via mutating the
underlying graph's content between two same-generation calls and asserting
the second call returns the FIRST result verbatim) cached profile when the
generation is unchanged, recomputes when `build_seq` bumps, and keys
independently per `(project_id, community)`.
`src/cortex/scope.rs`'s test module covers the full blast-radius derivation
against a small fixture graph (2 files, a `calls` edge and a `references`
edge, 2 distinct clusters): `changed_files_from_args`'s array/CSV/diff
parsing agree on the same file set; a touched node's documented caller AND
callee both appear in `blast_radius`; a changed file with no matching graph
node is echoed back as an unresolved literal entry alongside resolved
symbols; a bi-temporally invalidated neighbor (its file removed) is excluded
from a live blast radius; `compute_scope` against an unconfigured/empty store
degrades to `configured:false` with every `changed_files` entry unresolved; an
artificially low `max_blast_nodes` sets `truncated:true` and caps the
returned `blast_radius`; an input file list over `MAX_CHANGED_FILES` sets
`truncated:true` via the input-file cap (distinct from the node cap, and
surfaced even on the `configured:false` degrade path); and
`token_reduction_pct` is `0.0` for an empty graph and high when only a small
fraction of a larger graph is touched. `src/review/kg_context.rs`'s tests add
coverage for `derive_changed_files_counted`'s `(files, input_truncated)`
signal (array- and diff-path caps flag truncation; deduped paths at the cap
do not), while the existing `derive_changed_files` tests are unchanged.
`src/scribe/graph/query.rs`'s tests add coverage for the shared
`one_hop_neighbors` walk (incoming/outgoing split, direction filter), and
`src/scribe/graph/tools.rs`'s existing `kg_neighbors` tests are unchanged —
its output is byte-identical after being refactored onto that helper.
`src/cortex/deprecated.rs`'s test module covers: all 7 aliases register,
each returns a `{"deprecated":true,"use":...}` pointer regardless of input
shape (including empty args), and no alias's `execute` does any I/O.
`src/cortex/audit.rs`'s test module is unchanged — it separately covers every
branch of `validate_repo_url()`, including the SSRF bypass-encoding
regression tests.
`src/cortex/review.rs`'s test module (CXEG-04) covers `score` as a PURE
function against synthetic `EleganceSignal`/recurrence inputs — no live
Postgres needed: bands (`"low"` for no signals/recurrence, `"high"` for
severe structural + heavy recurrence), `contributions` reconstructing the
raw pre-clamp score exactly (a fixture deliberately exceeding the `10.0`
ceiling), determinism (repeat calls, and recurrence-input order not
affecting the result), band-boundary determinism (exactly-at-cut-point
values resolve to the HIGHER band), the log-scaled (not linear) growth of
`recurrence_magnitude`, and `recommendation_for` never containing "reject"
for any band. `touched_recurrence` and `compute_review`'s async,
I/O-touching paths are tested against a fixture `GraphStore` (seeded via
`SCRIBE_KG_STORE_DIR`, `#[serial_test::serial]`) with `ATLAS_DATABASE_URL`
absent (skipping gracefully, mirroring `metrics.rs`'s own
`compute_signals_degrades_when_vector_store_unconfigured` pattern, if a real
DSN happens to be live in the test process): a hub-touching change fires
structural signals and scores `> 0`; a small uniform-distribution change
scores `"low"`/`0.0`; the graph-unavailable path degrades to
`configured:false`/`band:"unknown"`/`findings:"unavailable"` (never an
error) and still propagates `truncated`; `touched_recurrence` itself degrades
to `(vec![], "unavailable")` without a configured DSN; and repeated
`compute_review` calls over the same fixture are byte-identical.

## History

Before CXEG-01, this module was a 10-tool SSH-exec relay to a script (`ops.py`)
on the fleet host, ported from a legacy Python source, mirroring
`crucible`/`sentinel`/`vigil`'s SSH-exec mechanics exactly (same SSH client
library usage, same non-infra-leaking generic error messages, same
`CORTEX_SSH_HOST`/`CORTEX_SSH_USER`/`CORTEX_SSH_KEY_PATH`/`CORTEX_SCRIPT`-env
config surface). That fleet host is now retired, and this whole transport
(including 7 of the original 10 tools' entire reason for existing — querying
a graph that only ever lived on the remote host) no longer has anywhere to
connect to. CXEG-01 deleted the transport and the 7 pure graph-relay tools,
kept `cortex_scope`/`cortex_review`/`cortex_audit`'s names/parameter surfaces
as principled stubs pending their Atlas-backed rebuilds (CXEG-02/CXEG-04/
CXEG-11), and added 7 zero-I/O deprecation aliases pointing at the in-process
Atlas KG's `kg_*` tool family, which is the actual successor to "a code graph
Cortex can query." **CXEG-02** then replaced `cortex_scope`'s pending-pointer
stub with the real Atlas-backed blast-radius implementation described above
(`src/cortex/scope.rs`), reusing `review::kg_context::derive_changed_files`
and the same `GraphStore`/`KnowledgeGraph` query API `kg_neighbors`/
`build_kg_block` already use rather than standing up a second graph-walk.
**CXEG-03** added the standalone Tier-B structural-elegance signal library
(`src/cortex/metrics.rs`) without yet wiring it into `cortex_review`. **CXEG-04**
then replaced `cortex_review`'s pending-pointer stub with the real
Atlas-backed risk-scoring implementation described above (`src/cortex/review.rs`):
CXEG-03's structural signals plus KGFIND-01 recurrence, combined via a pure,
fully-explainable, log-scaled scoring function into a `risk_score`/`band`/
`recommendation` that only ever escalates review rigor, never auto-rejects.
It also factored `cortex_scope`'s argument-validation logic out into the
shared `validate_and_parse_changed_files` helper (`src/cortex/mod.rs`) so
`cortex_review` reuses the identical validation rather than a second copy.
Once CXEG-04's real `risk_score` landed, `crate::scribe::graph::cortex_bridge`
(KGRULE-05) began returning a real signal (with no code change to its
`extract_risk` path — just a guard so a `configured:false` degrade response
maps to `None` rather than a misleading `Some(0.0)`).

**CXEG-11** (which merged to `main` around the same time as CXEG-04) replaced
`cortex_audit`'s pending-pointer stub with a real Atlas-backed external-repo
audit: an isolated, always-cleaned-up `ScratchClone` (a scoped
`std::process::Command` git clone — no sanctioned "clone an arbitrary public
URL" tool exists in this crate, so this is the sanctioned fallback for exactly
this tool's designed operation) feeds the same `walk_rs`/`build_rust_graph`
extraction `scribe_kg_build` uses to build a transient, never-persisted graph,
which CXEG-03's `metrics::compute_structural_signals` scores.
`validate_repo_url` — already the strongest piece of this module — is
untouched. With CXEG-04 and CXEG-11 both merged, all 3 of Cortex's real tools
(`cortex_scope`, `cortex_review`, `cortex_audit`) are now live Atlas-backed
implementations; no `cortex_*` tool remains a pending stub.

**CXEG-06** added the module's first entirely NEW tool name since CXEG-01,
`cortex_house_style` (`src/cortex/house_style.rs`) — house-style exemplar
extraction from Atlas, scoped per Leiden community, built for a future Tier-C
reviewer (CXEG-07) to cite concrete in-repo precedent rather than generic
opinion. It reuses the same `vec_embed::node_card`/`EmbedClient` card+embed
path `metrics` (CXEG-03) and `scribe_kg_build` already use, adds a small
in-process generation-keyed cache (`house_style::HouseStyleCache`), and takes
the module's registered tool-name count from 10 to 11 (an intentional,
additive MCP-listing change).

**CXEG-08** added `cortex_waive` and `review_run`'s Stage-5b risk-gate
escalation (governance around CXEG-04's `risk_score`, never a new scoring
signal — see `README.md`). **CXEG-09** added `cortex_crystallize`, the rule
crystallization loop that promotes recurring `consistency`/`elegance`
findings into a lint stub or a `docs/house-style.md` prose rule via an
adversarial `review_run` panel. **CXEG-12** added the final tool,
`cortex_consistency_debt` (`src/cortex/debt.rs`) — a read-only, per-community
rollup of the same `consistency`/`elegance`/`waiver` findings CXEG-07/CXEG-08
already capture, so the fleet can see whether house-style debt is trending up
or down without standing up a second store. This took the module's
registered tool-name count from 11 to 14 across CXEG-08/09/12 (11 → 12 → 13 →
14). See `docs/cortex-elegance-gate.md` for the full end-to-end operator
reference tying all of CXEG-01 through CXEG-12 together.

---

[← docs index](../../README.md)
