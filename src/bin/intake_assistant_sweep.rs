//! ASMT-10 live entrypoint: run the consolidated S84 assistant profiling sweep.
//!
//! Thin `main` around [`terminus_rs::intake::assistant::runner::run`] — the
//! library `run()` already wires the production collaborators (intake Postgres
//! `PgScoreSink`/`PgFleetStore`, the `LiveSuiteDriver` judge panel + live backend
//! passes, `ShellAcquirer`, and the reboot-survivable `FileCheckpoint`). This
//! binary exists only so the sweep is launchable as a process; the orchestration
//! logic lives in the library and is exhaustively tested under mocks.
//!
//! ## Runtime configuration (all env-sourced, no literals — set before launch)
//! - `INTAKE_DATABASE_URL` (or `DATABASE_URL`) — the intake/assistant Postgres.
//! - `INTAKE_STAGING_DIR`  — NAS staging: nominations.json + the resume checkpoint.
//! - `MODEL_REGISTRY_PATH` — chord model→backend registry (inference routing).
//! - `OLLAMA_URL` / proxy base — the unified inference path (the live backends).
//! - `JUDGE_<CLAUDE|GEMINI|CODEX>_CLI` / `_MODEL` — judge panel (default to the
//!   bare `claude`/`gemini`/`codex` CLIs on PATH; a missing/unauthed judge
//!   abstains and the panel still scores).
//!
//! Resume-safe: a reboot/disconnect re-runs only the dimensions not yet
//! checkpointed (see the runner's `FileCheckpoint`).

use terminus_rs::intake::assistant::runner;

#[tokio::main]
async fn main() -> std::process::ExitCode {
    terminus_rs::intake::init_tracing();
    match runner::run().await {
        Ok(report) => {
            let total = report.models.len();
            let profiled = report
                .models
                .iter()
                .filter(|m| {
                    m.acquisition_skip.is_none() && m.backends.iter().any(|b| b.survived)
                })
                .count();
            let skipped = report
                .models
                .iter()
                .filter(|m| m.acquisition_skip.is_some())
                .count();
            eprintln!(
                "assistant sweep complete: {profiled}/{total} models profiled, \
                 {skipped} acquisition-skipped (scores persisted to the intake DB)"
            );
            std::process::ExitCode::SUCCESS
        }
        Err(e) => {
            // Genericized — the error type already avoids infra leakage (S77).
            eprintln!("assistant sweep did not complete: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}
