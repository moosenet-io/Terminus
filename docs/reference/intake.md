# intake

`src/intake` — 2,059 KG symbols, the largest subsystem in the repository.

Intake is the model-intake and profiling engine (project name MINT): it
discovers candidate models, runs them through graduated profiling suites —
context (recall/throughput/TTFT/VRAM per context tier), coder (validated code
cases), and assistant (multi-dimension prompted evaluation with a 3-judge CLI
panel) — and stores a derived operational profile (safe/absolute context,
degradation point, recommended timeouts) in the shared Postgres. Downstream
consumers (model routing, serving control) read those profiles instead of
guessing. The engine enforces a single-hot-model VRAM policy and a GPU-authority
lock so profiling runs never fight production serving for the GPU.

## Key types and functions

| Symbol | Kind | File | Description |
|---|---|---|---|
| `intake::code_v2::CaseV2` | struct | `src/intake/code_v2.rs` | One validated coder-suite case: prompt, validator, tier, and per-case `timeout()` policy. |
| `intake::code_v2::CaseV2Result` | struct | `src/intake/code_v2.rs` | Outcome of a coder case run (pass/fail, timing, validator output). |
| `intake::code_v2::tier_default_timeout` | fn | `src/intake/code_v2.rs` | Default per-tier timeout for coder cases. |
| `intake::assistant::ModelId` | struct | `src/intake/assistant/mod.rs` | Canonical model identifier; `from_registry_key` normalizes registry keys into it. |
| `intake::assistant::dim5_prompted::PrecheckFlags` | struct | `src/intake/assistant/dim5_prompted.rs` | Pre-check flags for the prompted dimension-5 assistant evaluation (`any()` gates whether prechecks tripped). |
| `intake::timeouts::tier_default_secs` | fn | `src/intake/timeouts.rs` | Base timeout per profiling tier. |
| `intake::timeouts::reload_adjusted_timeout_secs` | fn | `src/intake/timeouts.rs` | Scales a base timeout when a model reload is expected (cold-load penalty). |
| `intake::gpu_authority` | module | `src/intake/gpu_authority.rs` | GPU-authority lock: acquire/release exclusive GPU use for a sweep (see `mint gpu ...`). |
| `intake::discovery` | module | `src/intake/discovery.rs` | Model discovery/curation feed (the `model_discovery_refresh` candidate brochure). |
| `intake::jobs` / `intake::supervisor` | modules | `src/intake/jobs.rs`, `supervisor.rs` | Durable fleet-sweep jobs and the restart-resilient supervisor around them. |
| `intake::serving` | module | `src/intake/serving.rs` | Serving-profile foundation the `serving_*` tools (in `tools/serving_tools`) sit on. |

## Tools and binaries

Registered tools include `model_intake`, `model_intake_status`,
`model_intake_compare`, `model_intake_fleet`, `model_intake_job_status`, and
`mint_breakfix`. The same library entry points back five binaries: the unified
`mint` CLI (`mint sweep coder|assistant`, `mint case`, `mint gaps`,
`mint gpu status|acquire|release`) and the legacy `intake_coder_sweep`,
`intake_coder_case`, `intake_coder_gaps`, `intake_assistant_sweep` binaries.

## How it connects

`registry::register_all` registers the intake tools on the CORE registry only
(Chord-served; deliberately not in `register_personal`). Inference calls go
through Ollama's `/api/generate` and through Chord sessions
(`intake::chord_pull`, `intake::chord_session`); results persist via `sqlx` to
the intake Postgres (`crate::config::intake_database_url`). The assistant
suite's judge panel shells out to provider CLIs — but only from the harness
binaries, never from a registered tool. `tools::serving_tools` builds on
`intake::serving`, and `mint` idle-mode (`src/mint`) coordinates with the
`compiler` build door to release GPU/RAM during heavy builds.

## Configuration

`INTAKE_DATABASE_URL` (falls back to `DATABASE_URL`), `JUDGE_<ID>_CLI` (per-judge
CLI command override; judges are `claude`/`gemini`/`codex`), plus tier/timeout
knobs read through `crate::config`. Names only — values come from the vault.

## Notes and gaps

This page does not enumerate the individual profiling dimensions, case corpora,
or the acquisition/durability design — see the dedicated MINT docs under
[docs/tools/mint/](../tools/mint/README.md) (cli, coder-eval, assistant-eval,
gpu-authority, data-model, restart-resilience, serving-profiles). The
`vuln_scan`, `breakfix`, `newcats`, and `lifecycle` submodules are not covered
here.
