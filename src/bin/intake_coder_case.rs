//! HFIX-06: ad hoc single/multi-case rerun for the v2 code suite.
//!
//! MINT Phase 1: the rerun logic itself now lives in the library
//! (`terminus_rs::intake::coder_case::run_from_env`), extracted so both this
//! binary AND the new `mint case` CLI subcommand share one code path. This
//! binary is now a thin wrapper.
//!
//! ## Runtime configuration (env-sourced, matching `intake_coder_sweep`'s convention)
//! - `INTAKE_CASE_MODEL`   — REQUIRED. The model id (e.g. "qwen3.5:27b").
//! - `INTAKE_CASE_IDS`     — REQUIRED. Comma-separated case ids from the v2
//!   corpus manifest (`manifest.json`'s `id` field).
//! - `INTAKE_CASE_BACKEND` — optional. `gpu` (default) or `cpu` — forces the
//!   same backend override the fleet sweep uses for a GPU/CPU pass.
//! - `INTAKE_CODE_LANGS`   — optional narrowing (rarely needed alongside
//!   explicit case ids; kept for parity with the fleet sweep's env surface).
//! - `SWEEP_MEM_CONFIG`    — optional; tags rows the same way the fleet
//!   sweep does, so a gap-fill lands under the same mem_config as the run
//!   it's patching.
//! - All the shared `INTAKE_DATABASE_URL` / `OLLAMA_URL` / `MODEL_REGISTRY_PATH`
//!   / `INTAKE_STAGING_DIR` / `INTAKE_CORPUS_V2_DIR` vars the fleet sweep uses.

use terminus_rs::intake::coder_case;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> std::process::ExitCode {
    terminus_rs::intake::init_tracing();
    coder_case::run_from_env().await
}
