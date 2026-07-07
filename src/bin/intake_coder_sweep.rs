//! S86 live entrypoint: run the S83/MINT v2 **code** profiling suite across a
//! fleet of models (the builder/coder counterpart to `intake_assistant_sweep`).
//!
//! MINT Phase 1: the fleet driver itself now lives in the library
//! (`terminus_rs::intake::coder_sweep::run`), extracted so both this binary
//! AND the new `mint sweep coder` CLI subcommand share one code path. This
//! binary is now a thin wrapper: read the env-sourced config (unchanged
//! behavior/var names) and hand it to the library entry point.
//!
//! ## Runtime configuration (all env-sourced, no literals — set before launch)
//! - `INTAKE_DATABASE_URL` (or `DATABASE_URL`) — the intake Postgres (rows land
//!   in `model_profiles` + `code_profile_runs`). Read by `storage::get_pool`.
//! - `INTAKE_STAGING_DIR`  — the reliable NAS staging dir. Holds the fleet file
//!   (`coder-nominations.json`, falling back to `nominations.json`) AND the
//!   resume checkpoint (`coder-sweep-checkpoint.json`).
//! - `MODEL_REGISTRY_PATH` — chord model→backend registry (read by `infer` to
//!   route each backend pass). Set so the GPU/CPU override resolves.
//! - `OLLAMA_URL` (or `_BASE_URL` / `_CPU_URL`) — the unified inference base.
//! - `INTAKE_CORPUS_V2_DIR` — the v2 code corpus (defaults to the deployed path).
//! - `INTAKE_CODE_LANGS` — optional comma list to NARROW the languages (corpus
//!   tags: rust,typescript,python,bash,htmlcss,cpp,sql,config). Empty ⇒ all.
//! - `INTAKE_CODE_CASE_LIMIT` — optional cap on cases per model (smoke/debug).
//! - `INTAKE_VRAM_CEILING_GB` — over-ceiling models skip the GPU pass cleanly.
//!
//! ## Resume / skip-with-reason (the load-bearing 24h-run property)
//! A reboot/disconnect RESUMES, never restarts: each completed `(model, backend)`
//! is appended to the file checkpoint AFTER its rows are persisted, and a resume
//! skips any `(model, backend)` already in the checkpoint. A model that hangs,
//! is unavailable, over-VRAM, or errors is recorded as a skip-with-reason and the
//! sweep CONTINUES — one bad model never wedges the fleet.

use terminus_rs::intake::coder_sweep;

// Multi-threaded runtime: the suite mixes async IO with libraries that expect a
// multi-thread scheduler (the same reason the assistant sweep uses it); a
// current-thread runtime risks deadlocking the inner inference futures.
#[tokio::main(flavor = "multi_thread")]
async fn main() -> std::process::ExitCode {
    terminus_rs::intake::init_tracing();
    let langs = coder_sweep::langs_from_env();
    let case_limit = coder_sweep::case_limit_from_env();
    let mem_config = coder_sweep::mem_config_from_env();

    coder_sweep::run(&langs, case_limit, mem_config.as_deref()).await
}
