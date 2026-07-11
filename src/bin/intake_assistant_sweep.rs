//! ASMT-10 live entrypoint: run the consolidated S84 assistant profiling sweep.
//!
//! MINT2-04: this binary is now a thin entrypoint into the UNIFIED MINT
//! harness — `MintHarness::run(RunKind::Assistant)`. The harness (in
//! `terminus_rs::intake`) owns the common run lifecycle (resolve config,
//! confirm the shared intake DB via `config::intake_database_url()`, stamp a
//! run-identity, dispatch to the assistant sub-runner). The assistant
//! sub-runner (`intake::assistant::runner::AssistantSweepRunner`) drives the
//! consolidated `run()` — which already wires the production collaborators
//! (intake Postgres `PgScoreSink`/`PgFleetStore`, the `LiveSuiteDriver` judge
//! panel + live backend passes, `ShellAcquirer`, and the reboot-survivable
//! `FileCheckpoint`) — and owns the end-of-run summary that used to live in
//! this `main`. The orchestration logic lives in the library and is
//! exhaustively tested under mocks.
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

use terminus_rs::intake::{MintHarness, RunKind};

#[tokio::main]
async fn main() -> std::process::ExitCode {
    terminus_rs::intake::init_tracing();
    MintHarness::run(RunKind::Assistant).await
}
