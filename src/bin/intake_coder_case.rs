//! HFIX-06: ad hoc single/multi-case rerun for the v2 code suite.
//!
//! Reruns an explicit `(model, backend, case_ids)` slice directly, bypassing
//! the fleet driver (`intake_coder_sweep`) and its (model, backend)-atomic
//! checkpoint entirely. Intended for filling a specific result gap — e.g. a
//! handful of cases that hard-failed on a now-fixed transient error, or a
//! manifest addition — without re-running a model's full ~40-200 case suite.
//!
//! No checkpoint is read or written. Every invocation runs the requested
//! cases fresh and persists new `code_profile_runs` rows (append-only, same
//! as the fleet sweep — old rows for the same case id are not overwritten).
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

use terminus_rs::intake::assistant::schema;
use terminus_rs::intake::gpu_authority::{self, GpuMode};
use terminus_rs::intake::{self, infer};

/// Read a required, trimmed, non-empty env var. `Err` names the var so a
/// misconfigured invocation fails fast with a clear reason.
fn env_required(key: &str) -> Result<String, String> {
    std::env::var(key)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("{key} is required (env-sourced — see file header)"))
}

/// Parse a comma list of case ids, trimming and dropping empties. Pure.
fn case_ids_from_env(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Resolve the short backend tag (`"gpu"` default, or `"cpu"`) from
/// `INTAKE_CASE_BACKEND`. Anything other than a case-insensitive `"cpu"`
/// resolves to `"gpu"` — mirrors the fleet sweep's two-backend model. Pure.
fn backend_from_env(raw: Option<&str>) -> String {
    match raw.map(str::trim).map(str::to_lowercase).as_deref() {
        Some("cpu") => "cpu".to_string(),
        _ => "gpu".to_string(),
    }
}

/// Map a short backend tag to the serving-backend override string, matching
/// `intake_coder_sweep`'s S86/gfx1151 routing (ollama-rocm for GPU, never
/// llama-server — see that binary's `run_fleet` comment for why). Pure.
fn override_str_for_backend(backend: &str) -> &'static str {
    match backend {
        "cpu" => "ollama-cpu",
        _ => "ollama",
    }
}

fn langs_from_env() -> Vec<String> {
    std::env::var("INTAKE_CODE_LANGS")
        .ok()
        .map(|s| {
            s.split(',')
                .map(|t| t.trim().to_lowercase())
                .filter(|t| !t.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

fn mem_config_from_env() -> Option<String> {
    std::env::var("SWEEP_MEM_CONFIG")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> std::process::ExitCode {
    let model_id = match env_required("INTAKE_CASE_MODEL") {
        Ok(m) => m,
        Err(e) => {
            eprintln!("case rerun did not start: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let ids_raw = match env_required("INTAKE_CASE_IDS") {
        Ok(m) => m,
        Err(e) => {
            eprintln!("case rerun did not start: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let case_ids = case_ids_from_env(&ids_raw);
    if case_ids.is_empty() {
        eprintln!("case rerun did not start: INTAKE_CASE_IDS parsed to zero case ids");
        return std::process::ExitCode::FAILURE;
    }
    let backend = backend_from_env(std::env::var("INTAKE_CASE_BACKEND").ok().as_deref());
    let override_str = override_str_for_backend(&backend);
    let langs = langs_from_env();
    let mem_config = mem_config_from_env();

    // Schema-dependency ordering — same reasoning as intake_coder_sweep.rs:
    // this binary is an independent entry point into the shared DB and must
    // not assume the assistant sweep (or intake_coder_sweep) ran first on a
    // fresh host. migrate() is idempotent and cheap.
    let pool = match schema::get_pool().await {
        Ok(p) => p,
        Err(e) => {
            eprintln!("case rerun did not start: schema pool connect failed: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    if let Err(e) = schema::migrate(&pool).await {
        eprintln!("case rerun did not start: schema migrate failed: {e}");
        return std::process::ExitCode::FAILURE;
    }

    // HFIX-07: exclusive GPU use, same as the fleet sweep — a gap-fill rerun
    // is still live inference on the shared Ollama instance, and must not
    // silently overlap with an active sweep (the exact incident that
    // produced false "wedge" timeouts earlier — see gpu_authority's module
    // doc). A DIFFERENT holder label than the sweep's means this correctly
    // refuses to start while the sweep holds the GPU, rather than racing it.
    let _gpu_guard = match gpu_authority::ExclusiveGuard::acquire(GpuMode::Exclusive, "intake_coder_case") {
        Ok(g) => g,
        Err(e) => {
            eprintln!("case rerun did not start: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };

    struct ClearOverride;
    impl Drop for ClearOverride {
        fn drop(&mut self) {
            infer::set_backend_override(None);
        }
    }
    infer::set_backend_override(Some(override_str.to_string()));
    let _clear = ClearOverride;

    eprintln!(
        "case rerun starting: model={model_id} backend={backend} case_ids=[{}] mem_config={}",
        case_ids.join(", "),
        mem_config.as_deref().unwrap_or("(unset — rows land with mem_config=NULL)"),
    );

    let profile_id = match intake::create_profile_row(&model_id).await {
        Ok(id) => id,
        Err(e) => {
            eprintln!("case rerun did not start: profile row create failed: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };

    let outcome = intake::run_code_suite_v2_cases(
        &model_id,
        &langs,
        Some(&case_ids),
        profile_id,
        None,
        Some(&backend),
        mem_config.as_deref(),
    )
    .await;

    match outcome {
        Ok(res) => {
            eprintln!(
                "case rerun complete: requested={} ran={} scored={} errors={} \
                 avg_first_pass={:.2} avg_effective={:.2} approved=[{}]",
                case_ids.len(),
                res.cases_run,
                res.scored,
                res.errors,
                res.avg_first_pass,
                res.avg_effective,
                res.approved.join(", "),
            );
            if res.cases_run < case_ids.len() {
                eprintln!(
                    "WARNING: {} requested case id(s) were not found in the corpus manifest \
                     (check INTAKE_CASE_IDS for typos or a stale manifest)",
                    case_ids.len() - res.cases_run
                );
            }
            std::process::ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("case rerun failed: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn case_ids_from_env_trims_and_drops_empties() {
        assert_eq!(
            case_ids_from_env(" a , b,, c "),
            vec!["a".to_string(), "b".to_string(), "c".to_string()]
        );
    }

    #[test]
    fn case_ids_from_env_all_empty_yields_empty_vec() {
        assert!(case_ids_from_env("  ,  ,").is_empty());
    }

    #[test]
    fn backend_from_env_defaults_to_gpu() {
        assert_eq!(backend_from_env(None), "gpu");
        assert_eq!(backend_from_env(Some("")), "gpu");
        assert_eq!(backend_from_env(Some("weird")), "gpu");
    }

    #[test]
    fn backend_from_env_recognizes_cpu_case_insensitively() {
        assert_eq!(backend_from_env(Some("cpu")), "cpu");
        assert_eq!(backend_from_env(Some(" CPU ")), "cpu");
    }

    #[test]
    fn override_str_matches_backend() {
        assert_eq!(override_str_for_backend("gpu"), "ollama");
        assert_eq!(override_str_for_backend("cpu"), "ollama-cpu");
    }
}
