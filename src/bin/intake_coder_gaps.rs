//! HFIX-06: find which v2 code-suite case ids a model is MISSING valid data
//! for, under a given `mem_config` — the companion to `intake_coder_case`
//! (which reruns an explicit case-id list). Prints a ready-to-paste
//! `INTAKE_CASE_IDS` value so a gap can be closed without re-running a
//! model's entire suite.
//!
//! MINT Phase 1: the audit logic itself now lives in the library
//! (`terminus_rs::intake::coder_gaps::run_from_env`), extracted so both this
//! binary AND the new `mint gaps` CLI subcommand share one code path. This
//! binary is now a thin wrapper.
//!
//! ## Runtime configuration (env-sourced, matching the sibling tools' convention)
//! - `INTAKE_CASE_MODEL`  — REQUIRED. The model id to audit.
//! - `SWEEP_MEM_CONFIG`   — optional. The mem_config to scope the audit to
//!   (e.g. `dynamic_gtt`). Unset ⇒ audits rows with `mem_config IS NULL`
//!   (the carveout baseline convention) — pass `SWEEP_MEM_CONFIG=carveout` to
//!   instead scope to rows explicitly labeled `'carveout'` post-relabel.
//! - `INTAKE_CODE_LANGS`  — optional narrowing, same semantics as the fleet
//!   sweep (empty ⇒ every case in the corpus).
//! - `INTAKE_DATABASE_URL` / `INTAKE_CORPUS_V2_DIR` — shared with the other
//!   intake binaries.

use terminus_rs::intake::coder_gaps;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> std::process::ExitCode {
    terminus_rs::intake::init_tracing();
    coder_gaps::run_from_env().await
}
