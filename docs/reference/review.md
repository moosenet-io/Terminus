# review

`src/review` — 263 KG symbols.

`review_run` is the build pipeline's review gate: it dispatches one review
prompt to 1–5 providers concurrently in one of four structures — `single`,
`adversarial_pair`, `panel_majority`, `panel_unanimous` — and aggregates the
verdicts into one answer. CLI-backed providers (`opus`, `codex`, `agy`) are
reached over loopback HTTP via the `review_daemon` binary, the only place in
the codebase permitted to spawn those processes; free-tier frontier models
(`nemotron`, `qwen_coder`) go directly to OpenRouter's chat-completions
endpoint. A single provider failing, timing out, or hitting quota degrades that
provider's entry to `"unavailable: <reason>"` instead of failing the call; the
aggregate's `complete` flag records whether every requested provider answered.

## Key types and functions

| Symbol | Kind | File | Description |
|---|---|---|---|
| `review::ReviewRun` | struct | `src/review/mod.rs` | The `review_run` tool: request parsing, structure selection, concurrent dispatch, aggregation (`new`, `execute`). |
| `review::prompt::parse_verdict` | fn | `src/review/prompt.rs` | Extracts a structured `Verdict` from a provider's raw text response. |
| `review::prompt::parse_findings_with_marker` | fn | `src/review/prompt.rs` | Parses marker-delimited structured findings out of review output. |
| `review::prompt::build_docs_prompt` | fn | `src/review/prompt.rs` | Prompt builder for documentation-generation dispatches (the legacy per-module docgen path). |
| `review::consistency::merge_and_flag_disagreement` | fn | `src/review/consistency.rs` | Merges multi-provider verdicts and flags substantive disagreement for the operator. |
| `review::kg_context::derive_changed_files_counted` | fn | `src/review/kg_context.rs` | Derives the changed-file set (with truncation detection) that keys KG context injection into review prompts. |
| `review::capacity` | module | `src/review/capacity.rs` | Provider-capacity core: two-tier cooldown/shelve state machine distinguishing rolling API quotas from fixed-cliff subscription quotas; backs `review_provider_status`. |
| `review::dispatch` | module | `src/review/dispatch.rs` | Provider dispatch: daemon-vs-OpenRouter routing, model tags, `ReviewConfig::dispatch_daemon`. |
| `review::free_pool` | module | `src/review/free_pool.rs` | Free-tier provider pool management (env-tunable via helpers like `env_u64`). |

## The review daemon

`src/bin/review_daemon/` is a standalone loopback HTTP daemon with a hard
security posture: closed provider enum validated before any spawn code, argv
arrays only (never `sh -c`), a sanitized child environment computed once at
startup (allowlist, then strip anything TOKEN/KEY/SECRET/PASSWORD-shaped),
`127.0.0.1` bind, fail-closed bearer auth (`REVIEW_DAEMON_TOKEN` required to
start), a concurrency semaphore, and an egress proxy (`egress_proxy.rs`) that
constrains child network access.

## How it connects

Registered on the core registry (tools `review_run`,
`review_provider_status`). `review::kg_context` reads `scribe::graph` to inject
a `knowledge_graph` block into prompts and records recurring findings to the
KG findings store. `scribe`'s documentation agent and `tools::docgen` both
dispatch their LLM work through this subsystem's daemon seam rather than
owning their own subprocess path. `cortex_review`'s risk output feeds the same
gate decisions.

## Configuration

`REVIEW_DAEMON_URL` (default loopback :8790), `REVIEW_DAEMON_TOKEN` (unset →
daemon providers degrade to unavailable), `OPENROUTER_API_KEY` (unset → the two
OpenRouter providers degrade), `REVIEW_DAEMON_PORT` (daemon side, operator env,
never request-controlled).

## Notes and gaps

Known operational caveat: only daemon providers receive the inline diff in some
flows — a provider that cannot see the code may return a blind REQUEST_CHANGES;
weigh dissent accordingly (daemon-side diff inlining to every provider is the
tracked fix). This page does not document the review prompt taxonomy or panel
selection policy — see [docs/tools/models-review/](../tools/models-review/README.md).
