//! MINT Phase 1: the unified `mint` CLI — one binary, clap-derived
//! subcommand tree, over the SAME library entry points the standalone
//! `intake_coder_sweep` / `intake_coder_case` / `intake_coder_gaps` /
//! `intake_assistant_sweep` binaries call (`terminus_rs::intake::{coder_sweep,
//! coder_case, coder_gaps, assistant::runner, gpu_authority}`).
//!
//! Nothing here duplicates orchestration logic: every subcommand resolves its
//! configuration (CLI flag, falling back to the SAME env var the legacy
//! binary reads) and hands the resolved values to the library's `run`
//! function. The legacy binaries remain first-class (unchanged behavior, same
//! code path) — `mint` is an additional, more discoverable front door, not a
//! replacement.
//!
//! ## Subcommands
//! - `mint sweep coder`      — fleet-level coder profiling sweep (was `intake_coder_sweep`).
//! - `mint sweep assistant`  — fleet-level assistant profiling sweep (was `intake_assistant_sweep`).
//! - `mint case`             — ad hoc single/multi-case rerun (was `intake_coder_case`).
//! - `mint gaps`             — case-id gap audit for a model (was `intake_coder_gaps`).
//! - `mint gpu status`       — point-in-time GPU-authority lock/runner-config query.
//! - `mint gpu acquire`      — proactively claim exclusive (or hand back shared) GPU use.
//! - `mint gpu release`      — release a held GPU-authority lock.

use clap::{Parser, Subcommand, ValueEnum};

use terminus_rs::intake::{
    assistant::{runner, schema},
    chord_pull, coder_case, coder_gaps, coder_sweep, gpu_authority, infer, supervisor,
};

#[derive(Parser)]
#[command(name = "mint", about = "MINT model-intake profiling CLI (sweep/case/gaps/gpu)")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Fleet-level profiling sweeps.
    Sweep {
        #[command(subcommand)]
        target: SweepTarget,
    },
    /// Ad hoc rerun of an explicit (model, backend, case_ids) slice — a
    /// gap-fill, without re-running a model's whole suite.
    Case {
        /// Model id. Falls back to INTAKE_CASE_MODEL when omitted.
        #[arg(long)]
        model: Option<String>,
        /// Comma-separated case ids. Falls back to INTAKE_CASE_IDS when omitted.
        #[arg(long)]
        ids: Option<String>,
        /// "gpu" (default) or "cpu". Falls back to INTAKE_CASE_BACKEND.
        #[arg(long)]
        backend: Option<String>,
        /// Comma-separated language narrowing. Falls back to INTAKE_CODE_LANGS.
        #[arg(long)]
        langs: Option<String>,
        /// mem_config tag. Falls back to SWEEP_MEM_CONFIG.
        #[arg(long = "mem-config")]
        mem_config: Option<String>,
        /// MINT Phase 6: redirect the default ollama backend's inference to a
        /// remote host (`host:port` or full URL). Falls back to
        /// MINT_REMOTE_OLLAMA_URL. The harness still runs (and locks the GPU)
        /// locally; only the inference target moves. Models pinned to a
        /// non-default backend keep their own routing.
        #[arg(long)]
        remote: Option<String>,
    },
    /// Find which v2 code-suite case ids a model is missing valid data for.
    Gaps {
        /// Model id to audit. Falls back to INTAKE_CASE_MODEL when omitted.
        #[arg(long)]
        model: Option<String>,
        /// mem_config to scope the audit to. Falls back to SWEEP_MEM_CONFIG.
        #[arg(long = "mem-config")]
        mem_config: Option<String>,
        /// Comma-separated language narrowing. Falls back to INTAKE_CODE_LANGS.
        #[arg(long)]
        langs: Option<String>,
    },
    /// HFIX-07 GPU-runner authority: status / acquire / release.
    Gpu {
        #[command(subcommand)]
        action: GpuAction,
    },
    /// MINT Phase 3 permanent sweep supervisor: run / install / uninstall.
    Supervisor {
        #[command(subcommand)]
        action: SupervisorAction,
    },
    /// MINT Phase 5: delegate a model re-pull/re-quantize to Chord's
    /// PullCoordinator (`POST /api/models/:name/pull`).
    FetchModel {
        /// Model id to (re-)pull. Falls back to INTAKE_CASE_MODEL when omitted
        /// (same env var `mint case`/`mint gaps` fall back to — this is an ad
        /// hoc operator invocation, not a fleet-sweep target).
        #[arg(long)]
        model: Option<String>,
    },
}

#[derive(Subcommand)]
enum SupervisorAction {
    /// Run the permanent jam-detect + auto-recover daemon (tokio loop, 90s
    /// tick, SIGTERM-graceful). This is what the systemd unit's ExecStart runs.
    Run,
    /// Write + enable the `mint-supervisor.service` systemd unit. (Not run in
    /// the Phase-3 build; deploy-time only.)
    Install,
    /// Disable + remove the `mint-supervisor.service` systemd unit.
    Uninstall,
}

#[derive(Subcommand)]
enum SweepTarget {
    /// The S86 coder fleet sweep (builder/coder profiling suite).
    Coder {
        /// Comma-separated language narrowing. Falls back to INTAKE_CODE_LANGS.
        #[arg(long)]
        langs: Option<String>,
        /// Per-model case cap (smoke/debug). Falls back to INTAKE_CODE_CASE_LIMIT.
        /// `0` means "no limit" (same convention as leaving the env var unset).
        #[arg(long = "case-limit")]
        case_limit: Option<usize>,
        /// mem_config tag. Falls back to SWEEP_MEM_CONFIG.
        #[arg(long = "mem-config")]
        mem_config: Option<String>,
        /// MINT Phase 6: redirect the default ollama backend's inference to a
        /// remote host (`host:port` or full URL). Falls back to
        /// MINT_REMOTE_OLLAMA_URL. See `mint case --help` for the composition
        /// rule with per-model backend routing.
        #[arg(long)]
        remote: Option<String>,
    },
    /// The S84 consolidated assistant profiling sweep.
    Assistant {
        /// MINT Phase 6: redirect the default ollama backend's inference to a
        /// remote host (`host:port` or full URL). Falls back to
        /// MINT_REMOTE_OLLAMA_URL.
        #[arg(long)]
        remote: Option<String>,
    },
}

#[derive(Copy, Clone, ValueEnum)]
enum GpuModeArg {
    Exclusive,
    Shared,
}

impl From<GpuModeArg> for gpu_authority::GpuMode {
    fn from(m: GpuModeArg) -> Self {
        match m {
            GpuModeArg::Exclusive => gpu_authority::GpuMode::Exclusive,
            GpuModeArg::Shared => gpu_authority::GpuMode::Shared,
        }
    }
}

#[derive(Subcommand)]
enum GpuAction {
    /// Point-in-time query — no side effects.
    Status,
    /// Proactively apply a mode's policy (stop competing services, reconcile
    /// Ollama's runner config) and take the exclusive-use lock.
    Acquire {
        #[arg(long, value_enum, default_value = "exclusive")]
        mode: GpuModeArg,
        /// Short label recorded as the lock holder (e.g. the caller's name).
        #[arg(long, default_value = "mint")]
        holder: String,
    },
    /// Release a held lock, restarting exactly the services that acquire stopped.
    Release {
        /// Must match the holder that acquired the lock.
        #[arg(long, default_value = "mint")]
        holder: String,
    },
}

/// Merge a CLI flag with its env-var fallback: `Some(cli)` wins outright
/// (even if the CLI value differs from the env var); `None` falls back to
/// whatever the env-sourced resolver (the same one the legacy binary/`_from_env`
/// helper uses) returns.
fn resolved_string(cli: Option<String>, env_fallback: impl FnOnce() -> Option<String>) -> Option<String> {
    cli.or_else(env_fallback)
}

fn env_opt(key: &str) -> Option<String> {
    std::env::var(key).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// MINT Phase 6: normalize a `--remote` value to an Ollama base URL. A bare
/// `host:port` (no scheme) becomes an `http://` URL (`pvf2:11434` ⇒
/// `http://pvf2:11434`); an explicit `http://`/`https://` value passes through.
/// A trailing slash is trimmed (matching `context::ollama_base`).
fn normalize_remote_url(raw: &str) -> String {
    let s = raw.trim().trim_end_matches('/');
    if s.starts_with("http://") || s.starts_with("https://") {
        s.to_string()
    } else {
        format!("http://{s}")
    }
}

/// MINT Phase 6: resolve the remote inference-target override. The CLI `--remote`
/// flag wins over the `MINT_REMOTE_OLLAMA_URL` env var (same flag-wins precedent
/// as [`resolved_string`]); a blank value from either source is treated as unset.
/// The resolved value is normalized via [`normalize_remote_url`]. `None` means
/// "no override" — inference targets the default `context::ollama_base()`.
fn resolve_remote_url(cli: Option<String>) -> Option<String> {
    resolved_string(cli, || env_opt("MINT_REMOTE_OLLAMA_URL"))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .map(|s| normalize_remote_url(&s))
}

/// MINT Phase 2 item 5: connect + migrate the shared intake schema ONCE,
/// explicitly, at `mint`'s own entry point — in ADDITION to (not instead of)
/// each library `run` function's own defensive `migrate()` call, so the
/// standalone legacy binaries (`intake_coder_sweep`, `intake_assistant_sweep`,
/// …) keep working unchanged when invoked directly, without going through
/// `mint` at all. `migrate()` is idempotent and lock-safe (advisory-lock
/// serialized — see `dd44eaf`), so calling it here as well as inside the
/// dispatched subcommand is always safe, never a double-migration hazard.
async fn ensure_schema_migrated() -> Result<(), String> {
    let pool = schema::get_pool().await.map_err(|e| e.to_string())?;
    schema::migrate(&pool).await.map_err(|e| e.to_string())
}

/// Item 5: does `cmd` need the shared intake schema migrated before it runs?
/// Every subcommand EXCEPT `gpu ...` and `supervisor ...` touches the shared
/// intake DB (directly, or via the library function it dispatches to). `gpu
/// status`/`acquire`/`release` manage the GPU-authority FILE lock only and have
/// no Postgres dependency at all. `supervisor` is an OBSERVER of tables the
/// sweeps own — it must never migrate the schema (it doesn't write those
/// tables), and its `run` daemon must start even when the DB is momentarily
/// unreachable (it retries per tick), so a startup migrate would wrongly make
/// the daemon refuse to start; `install`/`uninstall` have no DB dependency at
/// all. Forcing a DB connection on any of these would make them fail on a host
/// with no DB reachable, which is not this item's intent. Pure/extracted so
/// this decision is unit-testable without a live Postgres connection.
fn needs_schema_migrate(cmd: &Command) -> bool {
    !matches!(
        cmd,
        Command::Gpu { .. } | Command::Supervisor { .. } | Command::FetchModel { .. }
    )
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> std::process::ExitCode {
    terminus_rs::intake::init_tracing();
    let cli = Cli::parse();

    // Item 5: migrate once, explicitly, here — before dispatching — for
    // every subcommand `needs_schema_migrate` says needs it.
    if needs_schema_migrate(&cli.command) {
        if let Err(e) = ensure_schema_migrated().await {
            eprintln!("mint: schema migrate failed: {e}");
            return std::process::ExitCode::FAILURE;
        }
    }

    match cli.command {
        Command::Sweep { target } => match target {
            SweepTarget::Coder { langs, case_limit, mem_config, remote } => {
                let langs = match langs {
                    Some(s) => coder_sweep::parse_langs(Some(&s)),
                    None => coder_sweep::langs_from_env(),
                };
                let case_limit =
                    coder_sweep::normalize_case_limit(case_limit).or_else(coder_sweep::case_limit_from_env);
                let mem_config = resolved_string(mem_config, coder_sweep::mem_config_from_env);
                // Phase 6: install the remote inference-target override (if any)
                // before the sweep runs. Process-global; intake runs sequentially.
                infer::set_remote_ollama_url(resolve_remote_url(remote));

                // Item 7: `mint`'s dispatcher pre-acquires the GPU-authority
                // guard under the EXACT SAME holder label `coder_sweep::run`
                // acquires internally (`coder_sweep::GPU_HOLDER`). This is
                // safe (not a double-acquisition deadlock/footgun) BECAUSE
                // `gpu_authority::acquire` treats a re-acquire by the SAME
                // holder as an idempotent no-op (see
                // `gpu_authority::is_idempotent_reacquire`) — the inner
                // acquire inside `coder_sweep::run` sees the lock already
                // held by its own label and does nothing further, and the
                // inner guard's `Drop` (at the end of `coder_sweep::run`)
                // performs the actual release + service-restart; THIS outer
                // guard's later `Drop` then finds no lock left and is a
                // no-op too. A DIFFERENT holder currently holding the lock
                // still correctly fails HERE, before any sweep work starts.
                let _gpu_guard = match gpu_authority::ExclusiveGuard::acquire(
                    gpu_authority::GpuMode::Exclusive,
                    coder_sweep::GPU_HOLDER,
                ) {
                    Ok(g) => g,
                    Err(e) => {
                        eprintln!("mint: GPU acquire failed: {e}");
                        return std::process::ExitCode::FAILURE;
                    }
                };
                coder_sweep::run(&langs, case_limit, mem_config.as_deref()).await
            }
            SweepTarget::Assistant { remote } => {
                // Phase 6: install the remote inference-target override (if any).
                infer::set_remote_ollama_url(resolve_remote_url(remote));
                // Item 7: same pattern as `Sweep::Coder`, under
                // `runner::GPU_HOLDER` — the exact label `runner::run`
                // acquires internally.
                let _gpu_guard = match gpu_authority::ExclusiveGuard::acquire(
                    gpu_authority::GpuMode::Exclusive,
                    runner::GPU_HOLDER,
                ) {
                    Ok(g) => g,
                    Err(e) => {
                        eprintln!("mint: GPU acquire failed: {e}");
                        return std::process::ExitCode::FAILURE;
                    }
                };
                match runner::run().await {
                    Ok(report) => {
                        let total = report.models.len();
                        let profiled = report
                            .models
                            .iter()
                            .filter(|m| m.acquisition_skip.is_none() && m.backends.iter().any(|b| b.survived))
                            .count();
                        let skipped = report.models.iter().filter(|m| m.acquisition_skip.is_some()).count();
                        eprintln!(
                            "assistant sweep complete: {profiled}/{total} models profiled, \
                             {skipped} acquisition-skipped (scores persisted to the intake DB)"
                        );
                        std::process::ExitCode::SUCCESS
                    }
                    Err(e) => {
                        eprintln!("assistant sweep did not complete: {e}");
                        std::process::ExitCode::FAILURE
                    }
                }
            }
        },

        Command::Case { model, ids, backend, langs, mem_config, remote } => {
            // Phase 6: install the remote inference-target override (if any).
            infer::set_remote_ollama_url(resolve_remote_url(remote));
            let model = resolved_string(model, || env_opt("INTAKE_CASE_MODEL"));
            let ids_raw = resolved_string(ids, || env_opt("INTAKE_CASE_IDS"));
            let case_ids = match ids_raw {
                Some(raw) => coder_case::case_ids_from_env(&raw),
                None => Vec::new(),
            };
            let backend = resolved_string(backend, || env_opt("INTAKE_CASE_BACKEND"));
            let langs = match langs {
                Some(s) => coder_sweep::parse_langs(Some(&s)),
                None => coder_case::langs_from_env(),
            };
            let mem_config = resolved_string(mem_config, coder_case::mem_config_from_env);

            // Item 7: same pattern, under `coder_case::GPU_HOLDER`.
            let _gpu_guard = match gpu_authority::ExclusiveGuard::acquire(
                gpu_authority::GpuMode::Exclusive,
                coder_case::GPU_HOLDER,
            ) {
                Ok(g) => g,
                Err(e) => {
                    eprintln!("mint: GPU acquire failed: {e}");
                    return std::process::ExitCode::FAILURE;
                }
            };
            coder_case::run(model.as_deref(), &case_ids, backend.as_deref(), &langs, mem_config.as_deref()).await
        }

        Command::Gaps { model, mem_config, langs } => {
            // No GPU guard here: `coder_gaps::run` is a read-only audit
            // against already-persisted rows, never runs inference, and
            // never acquires the GPU-authority lock itself either.
            let model = resolved_string(model, || env_opt("INTAKE_CASE_MODEL"));
            let mem_config = resolved_string(mem_config, coder_gaps::mem_config_from_env);
            let langs = match langs {
                Some(s) => coder_sweep::parse_langs(Some(&s)),
                None => coder_gaps::langs_from_env(),
            };
            coder_gaps::run(model.as_deref(), mem_config.as_deref(), &langs).await
        }

        Command::Gpu { action } => match action {
            GpuAction::Status => {
                let status = gpu_authority::status();
                match status.lock {
                    Some((holder, mode, pid, alive)) => {
                        println!(
                            "GPU lock: holder={holder} mode={mode} pid={pid} pid_alive={alive}"
                        );
                    }
                    None => println!("GPU lock: none held"),
                }
                println!("ollama exclusive drop-in present: {}", status.ollama_dropin_present);
                std::process::ExitCode::SUCCESS
            }
            GpuAction::Acquire { mode, holder } => match gpu_authority::acquire(mode.into(), &holder) {
                Ok(()) => {
                    println!("GPU acquired: holder={holder} mode={}", gpu_authority::GpuMode::from(mode).as_str());
                    std::process::ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("GPU acquire failed: {e}");
                    std::process::ExitCode::FAILURE
                }
            },
            GpuAction::Release { holder } => match gpu_authority::release(&holder) {
                Ok(()) => {
                    println!("GPU released: holder={holder}");
                    std::process::ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("GPU release failed: {e}");
                    std::process::ExitCode::FAILURE
                }
            },
        },

        Command::Supervisor { action } => match action {
            // The long-running daemon — observes the sweeps and restart-recovers
            // a jam; it manages its OWN DB lifecycle (reconnect/retry per tick),
            // which is why `needs_schema_migrate` excludes it above.
            SupervisorAction::Run => supervisor::run().await,
            SupervisorAction::Install => supervisor::install(),
            SupervisorAction::Uninstall => supervisor::uninstall(),
        },

        Command::FetchModel { model } => {
            let model = match resolved_string(model, || env_opt("INTAKE_CASE_MODEL")) {
                Some(m) => m,
                None => {
                    eprintln!(
                        "mint fetch-model: no model specified (pass --model or set INTAKE_CASE_MODEL)"
                    );
                    return std::process::ExitCode::FAILURE;
                }
            };
            match chord_pull::fetch_model(&model).await {
                Ok(outcome) => report_fetch_model_outcome(&model, outcome),
                Err(e) => {
                    eprintln!("mint fetch-model: {e}");
                    std::process::ExitCode::FAILURE
                }
            }
        }
    }
}

/// Print `outcome` and translate it to a process exit code. Split out from
/// the `main()` match arm so a unit test can assert on the exit-code mapping
/// for every [`chord_pull::PullOutcome`] variant without invoking `main`.
fn report_fetch_model_outcome(model: &str, outcome: chord_pull::PullOutcome) -> std::process::ExitCode {
    use chord_pull::PullOutcome;
    match outcome {
        PullOutcome::Warmed { .. } => {
            println!("fetch-model: {model} is now warm (present locally)");
            std::process::ExitCode::SUCCESS
        }
        PullOutcome::NotFound { detail } => {
            eprintln!("fetch-model: {model} not found: {detail}");
            std::process::ExitCode::FAILURE
        }
        PullOutcome::InsufficientDiskSpace { detail } => {
            eprintln!("fetch-model: {model}: {detail}");
            std::process::ExitCode::FAILURE
        }
        PullOutcome::Unauthorized => {
            eprintln!(
                "fetch-model: chord rejected the pull request (401/403) — set CHORD_JWT to a \
                 valid lumina token for this harness host"
            );
            std::process::ExitCode::FAILURE
        }
        PullOutcome::Unreachable { detail } => {
            eprintln!("fetch-model: chord control endpoint unreachable: {detail}");
            std::process::ExitCode::FAILURE
        }
        PullOutcome::Failed { detail } => {
            eprintln!("fetch-model: {model}: {detail}");
            std::process::ExitCode::FAILURE
        }
    }
}

// ===========================================================================
// Tests — clap parsing (flags resolve correctly; env fallback is exercised
// via the pure `resolved_string`/`env_opt` helpers `main()`'s match arms use,
// since the arms themselves call into live library `run()` functions that
// need a DB/corpus/inference stack — integration-only).
// ===========================================================================
#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;
    use std::sync::Mutex;

    // Serializes tests that touch process-global env vars — `cargo test`
    // runs tests in the same process on multiple threads by default, and
    // env vars are process-global state.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// clap's own self-consistency check (conflicting args, bad defaults,
    /// etc.) — cheap and catches a mis-declared arg at test time instead of
    /// only at first real invocation.
    #[test]
    fn cli_definition_is_self_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn sweep_coder_parses_all_flags() {
        let cli = Cli::try_parse_from([
            "mint",
            "sweep",
            "coder",
            "--langs",
            "rust,python",
            "--case-limit",
            "5",
            "--mem-config",
            "carveout",
        ])
        .expect("parses");
        match cli.command {
            Command::Sweep {
                target: SweepTarget::Coder { langs, case_limit, mem_config, remote },
            } => {
                assert_eq!(langs.as_deref(), Some("rust,python"));
                assert_eq!(case_limit, Some(5));
                assert_eq!(mem_config.as_deref(), Some("carveout"));
                assert!(remote.is_none());
            }
            _ => panic!("expected Sweep(Coder)"),
        }
    }

    #[test]
    fn sweep_coder_flags_default_to_none() {
        let cli = Cli::try_parse_from(["mint", "sweep", "coder"]).expect("parses");
        match cli.command {
            Command::Sweep {
                target: SweepTarget::Coder { langs, case_limit, mem_config, remote },
            } => {
                assert!(langs.is_none());
                assert!(case_limit.is_none());
                assert!(mem_config.is_none());
                assert!(remote.is_none());
            }
            _ => panic!("expected Sweep(Coder)"),
        }
    }

    #[test]
    fn sweep_assistant_parses_with_no_flags() {
        let cli = Cli::try_parse_from(["mint", "sweep", "assistant"]).expect("parses");
        match cli.command {
            Command::Sweep { target: SweepTarget::Assistant { remote } } => assert!(remote.is_none()),
            _ => panic!("expected Sweep(Assistant)"),
        }
    }

    #[test]
    fn sweep_assistant_parses_remote_flag() {
        let cli = Cli::try_parse_from(["mint", "sweep", "assistant", "--remote", "pvf2:11434"])
            .expect("parses");
        match cli.command {
            Command::Sweep { target: SweepTarget::Assistant { remote } } => {
                assert_eq!(remote.as_deref(), Some("pvf2:11434"));
            }
            _ => panic!("expected Sweep(Assistant)"),
        }
    }

    #[test]
    fn sweep_coder_parses_remote_flag() {
        let cli = Cli::try_parse_from(["mint", "sweep", "coder", "--remote", "http://pvf2:11434"])
            .expect("parses");
        match cli.command {
            Command::Sweep { target: SweepTarget::Coder { remote, .. } } => {
                assert_eq!(remote.as_deref(), Some("http://pvf2:11434"));
            }
            _ => panic!("expected Sweep(Coder)"),
        }
    }

    #[test]
    fn case_parses_all_flags() {
        let cli = Cli::try_parse_from([
            "mint",
            "case",
            "--model",
            "qwen3-coder:30b",
            "--ids",
            "r-001,r-002",
            "--backend",
            "cpu",
            "--langs",
            "rust",
            "--mem-config",
            "dynamic_gtt",
        ])
        .expect("parses");
        match cli.command {
            Command::Case { model, ids, backend, langs, mem_config, remote } => {
                assert_eq!(model.as_deref(), Some("qwen3-coder:30b"));
                assert_eq!(ids.as_deref(), Some("r-001,r-002"));
                assert_eq!(backend.as_deref(), Some("cpu"));
                assert_eq!(langs.as_deref(), Some("rust"));
                assert_eq!(mem_config.as_deref(), Some("dynamic_gtt"));
                assert!(remote.is_none());
            }
            _ => panic!("expected Case"),
        }
    }

    #[test]
    fn case_flags_default_to_none() {
        let cli = Cli::try_parse_from(["mint", "case"]).expect("parses");
        match cli.command {
            Command::Case { model, ids, backend, langs, mem_config, remote } => {
                assert!(model.is_none());
                assert!(ids.is_none());
                assert!(backend.is_none());
                assert!(langs.is_none());
                assert!(mem_config.is_none());
                assert!(remote.is_none());
            }
            _ => panic!("expected Case"),
        }
    }

    #[test]
    fn case_parses_remote_flag() {
        let cli = Cli::try_parse_from(["mint", "case", "--remote", "pvf2:11434"]).expect("parses");
        match cli.command {
            Command::Case { remote, .. } => assert_eq!(remote.as_deref(), Some("pvf2:11434")),
            _ => panic!("expected Case"),
        }
    }

    #[test]
    fn gaps_parses_flags() {
        let cli = Cli::try_parse_from([
            "mint",
            "gaps",
            "--model",
            "qwen3-coder:30b",
            "--mem-config",
            "carveout",
        ])
        .expect("parses");
        match cli.command {
            Command::Gaps { model, mem_config, langs } => {
                assert_eq!(model.as_deref(), Some("qwen3-coder:30b"));
                assert_eq!(mem_config.as_deref(), Some("carveout"));
                assert!(langs.is_none());
            }
            _ => panic!("expected Gaps"),
        }
    }

    // ---- MINT Phase 5: fetch-model CLI wiring ----

    #[test]
    fn fetch_model_parses_model_flag() {
        let cli = Cli::try_parse_from(["mint", "fetch-model", "--model", "qwen3-coder:30b"])
            .expect("parses");
        match cli.command {
            Command::FetchModel { model } => assert_eq!(model.as_deref(), Some("qwen3-coder:30b")),
            _ => panic!("expected FetchModel"),
        }
    }

    #[test]
    fn fetch_model_flag_defaults_to_none() {
        let cli = Cli::try_parse_from(["mint", "fetch-model"]).expect("parses");
        match cli.command {
            Command::FetchModel { model } => assert!(model.is_none()),
            _ => panic!("expected FetchModel"),
        }
    }

    #[test]
    fn fetch_model_falls_back_to_env_when_flag_omitted() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("INTAKE_CASE_MODEL", "env-model:8b");
        let cli = Cli::try_parse_from(["mint", "fetch-model"]).expect("parses");
        let model = match cli.command {
            Command::FetchModel { model } => resolved_string(model, || env_opt("INTAKE_CASE_MODEL")),
            _ => panic!("expected FetchModel"),
        };
        assert_eq!(model.as_deref(), Some("env-model:8b"));
        std::env::remove_var("INTAKE_CASE_MODEL");
    }

    #[test]
    fn needs_schema_migrate_false_for_fetch_model() {
        // fetch-model only talks to Chord over HTTP; it never touches the
        // shared intake schema itself.
        let cmd = Cli::try_parse_from(["mint", "fetch-model", "--model", "m:1"])
            .unwrap()
            .command;
        assert!(!needs_schema_migrate(&cmd));
    }

    // ---- MINT Phase 5: fetch-model outcome → exit code mapping ----

    #[test]
    fn report_fetch_model_outcome_warmed_is_success() {
        let code = report_fetch_model_outcome(
            "m:1",
            chord_pull::PullOutcome::Warmed { model: "m:1".to_string() },
        );
        assert_eq!(code, std::process::ExitCode::SUCCESS);
    }

    #[test]
    fn report_fetch_model_outcome_every_other_variant_is_failure() {
        let variants = vec![
            chord_pull::PullOutcome::NotFound { detail: "x".to_string() },
            chord_pull::PullOutcome::InsufficientDiskSpace { detail: "x".to_string() },
            chord_pull::PullOutcome::Unauthorized,
            chord_pull::PullOutcome::Unreachable { detail: "x".to_string() },
            chord_pull::PullOutcome::Failed { detail: "x".to_string() },
        ];
        for v in variants {
            let code = report_fetch_model_outcome("m:1", v.clone());
            assert_eq!(code, std::process::ExitCode::FAILURE, "expected FAILURE for {v:?}");
        }
    }

    #[test]
    fn gpu_status_parses() {
        let cli = Cli::try_parse_from(["mint", "gpu", "status"]).expect("parses");
        assert!(matches!(cli.command, Command::Gpu { action: GpuAction::Status }));
    }

    #[test]
    fn gpu_acquire_defaults_exclusive_and_holder_mint() {
        let cli = Cli::try_parse_from(["mint", "gpu", "acquire"]).expect("parses");
        match cli.command {
            Command::Gpu { action: GpuAction::Acquire { mode, holder } } => {
                assert!(matches!(mode, GpuModeArg::Exclusive));
                assert_eq!(holder, "mint");
            }
            _ => panic!("expected Gpu(Acquire)"),
        }
    }

    #[test]
    fn gpu_acquire_shared_with_custom_holder() {
        let cli = Cli::try_parse_from(["mint", "gpu", "acquire", "--mode", "shared", "--holder", "s86"])
            .expect("parses");
        match cli.command {
            Command::Gpu { action: GpuAction::Acquire { mode, holder } } => {
                assert!(matches!(mode, GpuModeArg::Shared));
                assert_eq!(holder, "s86");
            }
            _ => panic!("expected Gpu(Acquire)"),
        }
    }

    #[test]
    fn gpu_release_defaults_holder_mint() {
        let cli = Cli::try_parse_from(["mint", "gpu", "release"]).expect("parses");
        match cli.command {
            Command::Gpu { action: GpuAction::Release { holder } } => assert_eq!(holder, "mint"),
            _ => panic!("expected Gpu(Release)"),
        }
    }

    #[test]
    fn gpu_acquire_rejects_unknown_mode() {
        let res = Cli::try_parse_from(["mint", "gpu", "acquire", "--mode", "bogus"]);
        assert!(res.is_err(), "an invalid --mode value must fail to parse");
    }

    // ---- resolved_string / env_opt: the flag-wins-else-env-fallback contract
    //      `main()`'s match arms rely on for every subcommand ----

    #[test]
    fn resolved_string_cli_value_wins_over_env_fallback() {
        let _g = ENV_LOCK.lock().unwrap();
        let got = resolved_string(Some("from-cli".to_string()), || Some("from-env".to_string()));
        assert_eq!(got.as_deref(), Some("from-cli"));
    }

    #[test]
    fn resolved_string_falls_back_to_env_when_cli_is_none() {
        let _g = ENV_LOCK.lock().unwrap();
        let got = resolved_string(None, || Some("from-env".to_string()));
        assert_eq!(got.as_deref(), Some("from-env"));
    }

    #[test]
    fn resolved_string_none_when_both_absent() {
        let _g = ENV_LOCK.lock().unwrap();
        let got = resolved_string(None, || None);
        assert!(got.is_none());
    }

    #[test]
    fn env_opt_reads_trims_and_treats_blank_as_unset() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("MINT_TEST_ENV_OPT", "  value  ");
        assert_eq!(env_opt("MINT_TEST_ENV_OPT").as_deref(), Some("value"));
        std::env::set_var("MINT_TEST_ENV_OPT", "   ");
        assert!(env_opt("MINT_TEST_ENV_OPT").is_none());
        std::env::remove_var("MINT_TEST_ENV_OPT");
        assert!(env_opt("MINT_TEST_ENV_OPT").is_none());
    }

    #[test]
    fn case_flag_wins_over_env_var_end_to_end() {
        // Exercises the exact pattern `Command::Case`'s arm uses: a flag
        // present on the CLI wins even when the corresponding env var is
        // also set to a different value.
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("INTAKE_CASE_MODEL", "env-model:8b");
        let cli = Cli::try_parse_from(["mint", "case", "--model", "cli-model:30b"]).expect("parses");
        let model = match cli.command {
            Command::Case { model, .. } => resolved_string(model, || env_opt("INTAKE_CASE_MODEL")),
            _ => panic!("expected Case"),
        };
        assert_eq!(model.as_deref(), Some("cli-model:30b"));
        std::env::remove_var("INTAKE_CASE_MODEL");
    }

    #[test]
    fn case_falls_back_to_env_when_flag_omitted() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("INTAKE_CASE_MODEL", "env-model:8b");
        let cli = Cli::try_parse_from(["mint", "case"]).expect("parses");
        let model = match cli.command {
            Command::Case { model, .. } => resolved_string(model, || env_opt("INTAKE_CASE_MODEL")),
            _ => panic!("expected Case"),
        };
        assert_eq!(model.as_deref(), Some("env-model:8b"));
        std::env::remove_var("INTAKE_CASE_MODEL");
    }

    #[test]
    fn sweep_coder_case_limit_zero_means_no_limit_end_to_end() {
        // Exercises the exact pattern `Command::Sweep(Coder)`'s arm uses:
        // `--case-limit 0` must normalize to "no limit" (None), the SAME
        // resolved value as never setting INTAKE_CODE_CASE_LIMIT at all —
        // not a literal zero-case cap.
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("INTAKE_CODE_CASE_LIMIT");

        let cli = Cli::try_parse_from(["mint", "sweep", "coder", "--case-limit", "0"]).expect("parses");
        let case_limit = match cli.command {
            Command::Sweep { target: SweepTarget::Coder { case_limit, .. } } => {
                coder_sweep::normalize_case_limit(case_limit).or_else(coder_sweep::case_limit_from_env)
            }
            _ => panic!("expected Sweep(Coder)"),
        };

        let unset_env_cli = Cli::try_parse_from(["mint", "sweep", "coder"]).expect("parses");
        let case_limit_when_env_unset = match unset_env_cli.command {
            Command::Sweep { target: SweepTarget::Coder { case_limit, .. } } => {
                coder_sweep::normalize_case_limit(case_limit).or_else(coder_sweep::case_limit_from_env)
            }
            _ => panic!("expected Sweep(Coder)"),
        };

        assert_eq!(case_limit, None, "--case-limit 0 must resolve to no limit");
        assert_eq!(
            case_limit, case_limit_when_env_unset,
            "--case-limit 0 must resolve identically to INTAKE_CODE_CASE_LIMIT being unset"
        );
    }

    #[test]
    fn case_limit_zero_defers_to_env_when_env_is_set() {
        // `--case-limit 0` means "no CLI preference expressed" — when
        // INTAKE_CODE_CASE_LIMIT is ALSO set, the flag must defer to it
        // exactly as an omitted flag would, not force None and not impose
        // a literal zero-case limit.
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("INTAKE_CODE_CASE_LIMIT", "5");

        let cli = Cli::try_parse_from(["mint", "sweep", "coder", "--case-limit", "0"]).expect("parses");
        let case_limit = match cli.command {
            Command::Sweep { target: SweepTarget::Coder { case_limit, .. } } => {
                coder_sweep::normalize_case_limit(case_limit).or_else(coder_sweep::case_limit_from_env)
            }
            _ => panic!("expected Sweep(Coder)"),
        };

        std::env::remove_var("INTAKE_CODE_CASE_LIMIT");

        assert_eq!(
            case_limit,
            Some(5),
            "--case-limit 0 with INTAKE_CODE_CASE_LIMIT=5 set must defer to the env value"
        );
    }

    // ---- MINT Phase 6: --remote resolution + normalization ----

    #[test]
    fn normalize_remote_url_adds_scheme_to_bare_host_port() {
        assert_eq!(normalize_remote_url("pvf2:11434"), "http://pvf2:11434");
    }

    #[test]
    fn normalize_remote_url_passes_through_explicit_scheme() {
        assert_eq!(normalize_remote_url("http://pvf2:11434"), "http://pvf2:11434");
        assert_eq!(normalize_remote_url("https://gpu.host:443"), "https://gpu.host:443");
    }

    #[test]
    fn normalize_remote_url_trims_trailing_slash_and_whitespace() {
        assert_eq!(normalize_remote_url("  http://pvf2:11434/  "), "http://pvf2:11434");
        assert_eq!(normalize_remote_url("pvf2:11434/"), "http://pvf2:11434");
    }

    #[test]
    fn resolve_remote_url_cli_wins_over_env_and_normalizes() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("MINT_REMOTE_OLLAMA_URL", "env-host:11434");
        let got = resolve_remote_url(Some("pvf2:11434".to_string()));
        std::env::remove_var("MINT_REMOTE_OLLAMA_URL");
        assert_eq!(got.as_deref(), Some("http://pvf2:11434"));
    }

    #[test]
    fn resolve_remote_url_falls_back_to_env_when_flag_omitted() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("MINT_REMOTE_OLLAMA_URL", "http://env-host:11434");
        let got = resolve_remote_url(None);
        std::env::remove_var("MINT_REMOTE_OLLAMA_URL");
        assert_eq!(got.as_deref(), Some("http://env-host:11434"));
    }

    #[test]
    fn resolve_remote_url_none_when_both_absent() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("MINT_REMOTE_OLLAMA_URL");
        assert!(resolve_remote_url(None).is_none());
    }

    #[test]
    fn resolve_remote_url_blank_flag_treated_as_unset() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("MINT_REMOTE_OLLAMA_URL");
        assert!(resolve_remote_url(Some("   ".to_string())).is_none());
    }

    // ---- needs_schema_migrate (item 5) ----

    #[test]
    fn needs_schema_migrate_true_for_every_non_gpu_subcommand() {
        let sweep_coder = Cli::try_parse_from(["mint", "sweep", "coder"]).unwrap().command;
        let sweep_assistant = Cli::try_parse_from(["mint", "sweep", "assistant"]).unwrap().command;
        let case = Cli::try_parse_from(["mint", "case"]).unwrap().command;
        let gaps = Cli::try_parse_from(["mint", "gaps"]).unwrap().command;

        assert!(needs_schema_migrate(&sweep_coder));
        assert!(needs_schema_migrate(&sweep_assistant));
        assert!(needs_schema_migrate(&case));
        assert!(needs_schema_migrate(&gaps));
    }

    #[test]
    fn needs_schema_migrate_false_for_every_gpu_action() {
        let status = Cli::try_parse_from(["mint", "gpu", "status"]).unwrap().command;
        let acquire = Cli::try_parse_from(["mint", "gpu", "acquire"]).unwrap().command;
        let release = Cli::try_parse_from(["mint", "gpu", "release"]).unwrap().command;

        assert!(!needs_schema_migrate(&status));
        assert!(!needs_schema_migrate(&acquire));
        assert!(!needs_schema_migrate(&release));
    }

    #[test]
    fn needs_schema_migrate_false_for_every_supervisor_action() {
        // The supervisor is a DB observer with its own reconnect/retry lifecycle
        // — it must never migrate the shared schema at startup (see the fn doc).
        let run = Cli::try_parse_from(["mint", "supervisor", "run"]).unwrap().command;
        let install = Cli::try_parse_from(["mint", "supervisor", "install"]).unwrap().command;
        let uninstall = Cli::try_parse_from(["mint", "supervisor", "uninstall"]).unwrap().command;

        assert!(!needs_schema_migrate(&run));
        assert!(!needs_schema_migrate(&install));
        assert!(!needs_schema_migrate(&uninstall));
    }

    #[test]
    fn supervisor_actions_parse() {
        assert!(matches!(
            Cli::try_parse_from(["mint", "supervisor", "run"]).unwrap().command,
            Command::Supervisor { action: SupervisorAction::Run }
        ));
        assert!(matches!(
            Cli::try_parse_from(["mint", "supervisor", "install"]).unwrap().command,
            Command::Supervisor { action: SupervisorAction::Install }
        ));
        assert!(matches!(
            Cli::try_parse_from(["mint", "supervisor", "uninstall"]).unwrap().command,
            Command::Supervisor { action: SupervisorAction::Uninstall }
        ));
    }

    // ---- item 7: mint's pre-acquired GPU-authority holder labels must match
    //      each library function's OWN internal acquire() call EXACTLY, or
    //      the inner acquire (a DIFFERENT holder, same live process/pid)
    //      would see a live competing holder and fail outright instead of
    //      taking the same-holder no-op path. These constants are `pub` on
    //      the library side specifically so the two acquisition points can
    //      never drift apart via a copy-pasted literal. ----

    #[test]
    fn gpu_holder_labels_are_stable_and_distinct_per_subcommand() {
        use std::collections::BTreeSet;
        let labels: BTreeSet<&str> = [
            terminus_rs::intake::coder_sweep::GPU_HOLDER,
            terminus_rs::intake::assistant::runner::GPU_HOLDER,
            terminus_rs::intake::coder_case::GPU_HOLDER,
        ]
        .into_iter()
        .collect();
        // Three subcommands, three DISTINCT holder labels — if two ever
        // collapsed to the same string, a coder sweep and an assistant
        // sweep (say) could run concurrently under an identical label and
        // never detect the collision via gpu_authority's is_blocked check.
        assert_eq!(labels.len(), 3);
        assert_eq!(terminus_rs::intake::coder_sweep::GPU_HOLDER, "intake_coder_sweep");
        assert_eq!(
            terminus_rs::intake::assistant::runner::GPU_HOLDER,
            "intake_assistant_sweep"
        );
        assert_eq!(terminus_rs::intake::coder_case::GPU_HOLDER, "intake_coder_case");
    }
}
