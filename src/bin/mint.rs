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

use terminus_rs::intake::{assistant::runner, coder_case, coder_gaps, coder_sweep, gpu_authority};

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
}

#[derive(Subcommand)]
enum SweepTarget {
    /// The S86 coder fleet sweep (builder/coder profiling suite).
    Coder {
        /// Comma-separated language narrowing. Falls back to INTAKE_CODE_LANGS.
        #[arg(long)]
        langs: Option<String>,
        /// Per-model case cap (smoke/debug). Falls back to INTAKE_CODE_CASE_LIMIT.
        #[arg(long = "case-limit")]
        case_limit: Option<usize>,
        /// mem_config tag. Falls back to SWEEP_MEM_CONFIG.
        #[arg(long = "mem-config")]
        mem_config: Option<String>,
    },
    /// The S84 consolidated assistant profiling sweep.
    Assistant,
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

#[tokio::main(flavor = "multi_thread")]
async fn main() -> std::process::ExitCode {
    let cli = Cli::parse();

    match cli.command {
        Command::Sweep { target } => match target {
            SweepTarget::Coder { langs, case_limit, mem_config } => {
                let langs = match langs {
                    Some(s) => coder_sweep::parse_langs(Some(&s)),
                    None => coder_sweep::langs_from_env(),
                };
                let case_limit = case_limit.or_else(coder_sweep::case_limit_from_env);
                let mem_config = resolved_string(mem_config, coder_sweep::mem_config_from_env);
                coder_sweep::run(&langs, case_limit, mem_config.as_deref()).await
            }
            SweepTarget::Assistant => match runner::run().await {
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
            },
        },

        Command::Case { model, ids, backend, langs, mem_config } => {
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
            coder_case::run(model.as_deref(), &case_ids, backend.as_deref(), &langs, mem_config.as_deref()).await
        }

        Command::Gaps { model, mem_config, langs } => {
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
                target: SweepTarget::Coder { langs, case_limit, mem_config },
            } => {
                assert_eq!(langs.as_deref(), Some("rust,python"));
                assert_eq!(case_limit, Some(5));
                assert_eq!(mem_config.as_deref(), Some("carveout"));
            }
            _ => panic!("expected Sweep(Coder)"),
        }
    }

    #[test]
    fn sweep_coder_flags_default_to_none() {
        let cli = Cli::try_parse_from(["mint", "sweep", "coder"]).expect("parses");
        match cli.command {
            Command::Sweep {
                target: SweepTarget::Coder { langs, case_limit, mem_config },
            } => {
                assert!(langs.is_none());
                assert!(case_limit.is_none());
                assert!(mem_config.is_none());
            }
            _ => panic!("expected Sweep(Coder)"),
        }
    }

    #[test]
    fn sweep_assistant_parses_with_no_flags() {
        let cli = Cli::try_parse_from(["mint", "sweep", "assistant"]).expect("parses");
        assert!(matches!(
            cli.command,
            Command::Sweep { target: SweepTarget::Assistant }
        ));
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
            Command::Case { model, ids, backend, langs, mem_config } => {
                assert_eq!(model.as_deref(), Some("qwen3-coder:30b"));
                assert_eq!(ids.as_deref(), Some("r-001,r-002"));
                assert_eq!(backend.as_deref(), Some("cpu"));
                assert_eq!(langs.as_deref(), Some("rust"));
                assert_eq!(mem_config.as_deref(), Some("dynamic_gtt"));
            }
            _ => panic!("expected Case"),
        }
    }

    #[test]
    fn case_flags_default_to_none() {
        let cli = Cli::try_parse_from(["mint", "case"]).expect("parses");
        match cli.command {
            Command::Case { model, ids, backend, langs, mem_config } => {
                assert!(model.is_none());
                assert!(ids.is_none());
                assert!(backend.is_none());
                assert!(langs.is_none());
                assert!(mem_config.is_none());
            }
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
}
