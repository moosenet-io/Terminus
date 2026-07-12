[← docs index](../../README.md)

# Cortex — Atlas-backed code-intelligence gate (CXEG-01/02/06)

Cortex is an 11-tool-name module (`src/cortex/mod.rs`, `src/cortex/scope.rs`,
`src/cortex/house_style.rs`, `src/cortex/deprecated.rs`, `src/cortex/audit.rs`),
but as of **CXEG-01** only 3 of those names were "real" tools — the rest are
structured deprecation aliases. As of **CXEG-02**, `cortex_scope` is the first
of those 3 to be a fully live, Atlas-backed implementation rather than a
pending-pointer stub. As of **CXEG-06**, a NEW 4th real tool,
`cortex_house_style`, is added (an intentional, additive MCP-listing change —
the module's registered tool-name count goes from 10 to 11). This page
describes the current shape; see the "History" section at the bottom for what
changed and why.

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
| `cortex_review` | **pending rebuild (CXEG-04)** | Validates `project_id`/`changed_files`, returns `{"status":"pending","item":"CXEG-04",...}`. No risk scoring happens yet. |
| `cortex_audit` | **pending rebuild (CXEG-11)** | Runs its existing SSRF-hardened `url` validation (unchanged, see below), then returns `{"status":"pending","item":"CXEG-11",...}`. No clone/graph-build happens yet. |
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
`ToolError::InvalidArgument` before the stub response is built.

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
worse-shaped," independent of correctness. It is not wired into
`cortex_review`'s response yet (that's CXEG-04's job) but is fully
unit-tested and importable as `crate::cortex::metrics::compute_signals`.

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

## `cortex_review` (pending — CXEG-04)

**Input schema**: identical shape to `cortex_scope` — `project_id` (enum,
required), `changed_files` (string, required, ≤2000 chars, comma-separated
modified file paths).

**Behavior**: validates both fields, then returns:

```json
{
  "status": "pending",
  "item": "CXEG-04",
  "tool": "cortex_review",
  "project_id": "TERM",
  "message": "cortex_review's SSH-relay-era backend has been retired; an Atlas-backed risk-scoring implementation lands in CXEG-04. In the meantime, query kg_findings / kg_query directly against the Atlas KG.",
  "risk_score_threshold": 7.0,
  "elegance_advisory_only": true
}
```

`risk_score_threshold` and `elegance_advisory_only` are read from
`CortexConfig` (see "Configuration" below) and echoed here so the CXEG-04
rebuild's threshold config is already visible/testable even though nothing
consumes it yet.

**Error/edge cases**: same as `cortex_scope`.

**In the meantime**: call `kg_findings` / `kg_query` directly against the
Atlas KG.

## `cortex_audit` — the one tool that still does real validation work

**Input schema**: `url` (string, required) — a public git repository URL,
e.g. `"https://github.com/owner/repo"`.

**Behavior**: `url` passes through the **unchanged**, SSRF-hardened
`validate_repo_url()` front-gate (`src/cortex/audit.rs` — this file was not
touched by CXEG-01; it has no dependency on the deleted SSH transport). Only
`http`/`https` URLs to public, non-private/loopback/link-local/metadata hosts
are accepted — see `audit.rs`'s own doc comments for the full numeric-host
SSRF-hardening rationale (decimal-integer, hex, octal-leading-zero, shorthand
dotted-quad, and IPv4-mapped-IPv6 encodings of loopback/private addresses are
all rejected, fail-closed). Once a `url` passes validation, `execute` returns:

```json
{
  "status": "pending",
  "item": "CXEG-11",
  "tool": "cortex_audit",
  "url": "https://github.com/octocat/Hello-World",
  "message": "cortex_audit's SSH-relay-era backend has been retired; a locally-sandboxed clone + Atlas-build implementation lands in CXEG-11. The url has passed SSRF-hardened validation but no audit has been performed.",
  "dup_cosine_threshold": 0.85
}
```

**No clone, no graph build, no HTML report generation happens yet** — CXEG-11
is expected to rebuild this as a locally-sandboxed clone + Atlas KG build,
replacing the old remote-script relay entirely (the retired implementation
never actually performed the clone in this process either — it delegated
that to the remote fleet-host script, so this is not a regression in local
sandboxing, just a currently-unimplemented rebuild).

**Error/edge cases**: `InvalidArgument` for any URL rejected by
`validate_repo_url` (empty, oversized, wrong scheme, embedded credentials,
shell metacharacters, whitespace/control chars, or a disallowed host) — all
caught before the stub response is built, so a malicious/malformed URL never
gets even a pending-pointer response, only a rejection.

## Configuration

`CortexConfig::from_env()` (`src/cortex/mod.rs`) builds one shared
`Arc<CortexConfig>` for all 4 real tools. No SSH/remote-script fields remain.

| Env var | Type | Default | Notes |
| --- | --- | --- | --- |
| `CORTEX_RISK_SCORE_THRESHOLD` | f64 | `7.0` | Echoed in `cortex_review`'s pending-pointer response; will gate the CXEG-04 rebuild's escalation logic. |
| `CORTEX_ENABLE_TIER_B` | bool | `false` | Feature flag for a not-yet-built Tier B analysis pass. No longer consumed by `cortex_scope` as of CXEG-02 (the pending stub used to echo it). |
| `CORTEX_ENABLE_TIER_C` | bool | `false` | Feature flag for a not-yet-built Tier C analysis pass. |
| `CORTEX_ELEGANCE_ADVISORY_ONLY` | bool | `true` | Whether elegance/style findings are advisory-only; echoed in `cortex_review`'s response. |
| `CORTEX_DUP_COSINE_THRESHOLD` | f64 | `0.85` | Cosine-similarity threshold for a not-yet-built dup-detection pass; echoed in `cortex_audit`'s response. |
| `CORTEX_MAX_BLAST_NODES` | usize | `200` | `cortex_scope`'s (CXEG-02) cap on the number of nodes enumerated into `blast_radius` before it sets `truncated:true` and stops walking. A zero/unparseable value falls back to the default rather than dropping every node. |
| `CORTEX_TIER_B_PERCENTILE` | f64 | `90.0` | `metrics::compute_signals`'s (CXEG-03) percentile cut-point (0-100) for `centrality_spike`/`complexity_spike`/`fan_out_explosion`, computed against the project's own current-node distribution — self-calibrating, never a hardcoded absolute. |
| `CORTEX_HOUSE_STYLE_K` | usize | `3` (`house_style::DEFAULT_EXEMPLARS_K`) | `cortex_house_style`'s (CXEG-06) `K` — exemplars selected per `(community, kind)` bucket. A zero/unparseable value falls back to the default (same "never silently drop everything" reasoning as `CORTEX_MAX_BLAST_NODES`). |
| `ATLAS_DATABASE_URL` | secret-shaped | none | Read exclusively through `crate::config::atlas_database_url()` — this crate has no separate `SecretManager`/`vault::manager()` API of its own; the runtime secret store is materialized into the process environment at deploy time (same convention as `crate::pki` and `scribe::graph::vec_embed`). `None` means the Atlas KG store is not configured (`cortex_scope` still degrades cleanly in this case, via `GraphStore`/`ScribeConfig`'s own `SCRIBE_KG_STORE_DIR`, which is independent of the Postgres DSN). |
| `EMBEDDINGS_URL` / `EMBEDDINGS_MODEL` / `EMBEDDINGS_TIMEOUT_MS` / `EMBEDDINGS_API_KEY` | see `crate::config` / `scribe::graph::vec_embed` | see `crate::config` | Not read by this module directly — `cortex_house_style`'s exemplar selection goes through the SAME `EmbedClient::from_env()` `metrics`'s semantic-duplication detector and `scribe_kg_build` already use. An unreachable/misconfigured endpoint degrades that bucket to centrality-only ranking (`degraded:true`), never an error. |

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
registers the 4 real tools against them (`cortex_scope`/`cortex_house_style`
live; `cortex_review`/`cortex_audit` still pending), then delegates to
`crate::cortex::deprecated::register()` for the 7 aliases. Cortex is wired
into **both** top-level registries in `src/registry.rs`: `register_all` (the
core registry, served by `terminus-primary`/Chord) and `register_personal`
(the personal registry) — unchanged from before CXEG-01.

## `crate::scribe::graph::cortex_bridge` — the one internal caller

`src/scribe/graph/cortex_bridge.rs` (KGRULE-05) calls `cortex_review`
internally to attach a best-effort risk signal to KG findings. As of CXEG-01
it always gets `None` back — `cortex_review`'s pending-stub response carries
no `risk_score` field for `cortex_bridge::extract_risk` to find — which is
within `cortex_bridge`'s own documented degrade contract ("returns `None`...
whenever... the tool call errors for any reason... [or] carries no numeric
`risk`/`score` field"). No code change is needed there once CXEG-04 lands a
real `risk_score`; the bridge is forward-compatible as-is.

## Testing notes for this module

`src/cortex/mod.rs`'s test module covers, without any network access:
`project_id` validation (accepts `TERM`/`LUM`/`HARM`/`CHRD`/`RAIL`, rejects
unknowns and the old legacy repo names), free-text length capping,
`cortex_review`/`cortex_audit`'s `InvalidArgument` rejection paths and
pending-pointer success shape, `cortex_scope`'s argument-validation/wiring
(`project_id` rejection; the count-vs-abuse reconciliation — a long CSV of
short paths AND a many-file diff both TRUNCATE with `truncated:true` rather
than erroring, while a single over-`MAX_TEXT_LEN` element, an over-
`MAX_CHANGED_FILES_ARG` array, and a pathologically huge single-blob `diff`
are each rejected; "neither changed_files nor diff" rejection; array-form and
diff-only-form acceptance; and a `configured:false` degrade smoke test against
an empty store dir),
`cortex_audit`'s unchanged SSRF-guard rejections, `cortex_house_style`'s
argument validation (unknown `project_id`, non-integer `community`) and a
`configured:false` degrade smoke test against an empty store dir, and full
registration (`register()` yields exactly 11 tool names, all `cortex_*`).
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
`cortex_review`/`cortex_audit` remain pending CXEG-04/CXEG-11. **CXEG-06**
added the module's first entirely NEW tool name since CXEG-01,
`cortex_house_style` (`src/cortex/house_style.rs`) — house-style exemplar
extraction from Atlas, scoped per Leiden community, built for a future Tier-C
reviewer (CXEG-07) to cite concrete in-repo precedent rather than generic
opinion. It reuses the same `vec_embed::node_card`/`EmbedClient` card+embed
path `metrics` (CXEG-03) and `scribe_kg_build` already use, adds a small
in-process generation-keyed cache (`house_style::HouseStyleCache`), and takes
the module's registered tool-name count from 10 to 11.

---

[← docs index](../../README.md)
