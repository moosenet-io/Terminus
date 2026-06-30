//! Centralized config helpers for terminus-rs (env-sourced, NO literals).
//!
//! terminus-rs historically read env vars inline per module (e.g.
//! `context::ollama_base`, `infer::registry_path`). This module collects the
//! helpers the S84 *assistant-profile* harness needs so the judge-CLI command
//! names, judge model names, and the intake Postgres URL all resolve through a
//! single, testable place — and so the `pii_gate` hook never sees a hardcoded
//! host / org / CLI path in the harness code.
//!
//! ## Judge CLIs
//! The 3-judge panel shells out to provider OAuth CLIs (`claude`, `gemini`,
//! `codex`) the way the validator harness shells out to `bash` (see
//! `intake::code_v2`). Each judge's *command* and *model* are read from env so
//! an operator can point at a wrapper script, pin a model, or disable a judge by
//! leaving its command empty. Defaults are the bare CLI names already on PATH in
//! a logged-in operator shell (not infra literals).
//!
//! ## Intake DB
//! [`intake_database_url`] prefers a dedicated `INTAKE_DATABASE_URL` and falls
//! back to the shared `DATABASE_URL` (the same pool S83 storage uses) so a single
//! DB deployment keeps working while a split deployment is possible.

/// Read an env var, trimmed; `None` when unset or empty.
fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// One judge provider in the panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JudgeProvider {
    Claude,
    Gemini,
    Codex,
}

impl JudgeProvider {
    /// Stable lowercase id stored in the `judge` column / used in env-var names.
    pub fn id(self) -> &'static str {
        match self {
            JudgeProvider::Claude => "claude",
            JudgeProvider::Gemini => "gemini",
            JudgeProvider::Codex => "codex",
        }
    }

    /// The default CLI command name (bare binary, assumed on PATH for a
    /// logged-in operator). Overridable via `JUDGE_<ID>_CLI`.
    fn default_cli(self) -> &'static str {
        // The provider CLI names are the canonical OAuth tools, not infra hosts.
        match self {
            JudgeProvider::Claude => "claude",
            JudgeProvider::Gemini => "gemini",
            JudgeProvider::Codex => "codex",
        }
    }

    /// All three providers in panel order.
    pub fn all() -> [JudgeProvider; 3] {
        [
            JudgeProvider::Claude,
            JudgeProvider::Gemini,
            JudgeProvider::Codex,
        ]
    }
}

/// CLI command for a judge, from `JUDGE_<ID>_CLI` (e.g. `JUDGE_CLAUDE_CLI`).
/// Falls back to the bare CLI name. Empty env value ⇒ falls back (never empty).
pub fn judge_cli(provider: JudgeProvider) -> String {
    let key = format!("JUDGE_{}_CLI", provider.id().to_uppercase());
    env_nonempty(&key).unwrap_or_else(|| provider.default_cli().to_string())
}

/// Model passed to a judge's CLI via `--model`, from `JUDGE_<ID>_MODEL`
/// (e.g. `JUDGE_CLAUDE_MODEL`). `None` ⇒ omit the `--model` flag and let the CLI
/// use its own default model.
pub fn judge_model(provider: JudgeProvider) -> Option<String> {
    let key = format!("JUDGE_{}_MODEL", provider.id().to_uppercase());
    env_nonempty(&key)
}

/// Per-judge wall-clock timeout (seconds) from `JUDGE_TIMEOUT_SECS`, default 120.
pub fn judge_timeout_secs() -> u64 {
    env_nonempty("JUDGE_TIMEOUT_SECS")
        .and_then(|v| v.parse().ok())
        .unwrap_or(120)
}

/// Postgres URL for the intake/assistant-profile tables. Prefers
/// `INTAKE_DATABASE_URL`, falls back to the shared `DATABASE_URL`.
/// Returns `None` (caller raises `NotConfigured`) when neither is set.
pub fn intake_database_url() -> Option<String> {
    env_nonempty("INTAKE_DATABASE_URL").or_else(|| env_nonempty("DATABASE_URL"))
}

// ── ASMT-09 consolidated runner: resilient staging + acquisition ──────────────
//
// The runner mirrors S83's reboot-survivable architecture: write-heavy small-file
// IO (nominations, corpora, the resume checkpoint) lives on the RELIABLE NAS,
// while read-heavy model GGUF loads come from the LOCAL SPAN with a NAS fallback.
// Every path resolves through these helpers — NEVER a literal in runner/acquire
// code — so the `pii_gate` hook never sees a hardcoded mount in the harness.

/// Reliable small-file staging root (NAS): nominations.json, the resume
/// checkpoint, and any other write-heavy harness state live here. From
/// `INTAKE_STAGING_DIR`; `None` ⇒ caller raises `NotConfigured` rather than
/// guessing a mount.
pub fn intake_staging_dir() -> Option<String> {
    env_nonempty("INTAKE_STAGING_DIR")
}

/// Local span root for read-heavy model GGUF loads (fast local card). From
/// `INTAKE_MODEL_SPAN_DIR`; `None` ⇒ no local span configured (the acquirer
/// falls back to [`intake_model_nas_dir`]).
pub fn intake_model_span_dir() -> Option<String> {
    env_nonempty("INTAKE_MODEL_SPAN_DIR")
}

/// NAS fallback root for model GGUFs when the local span is absent or drops
/// mid-run (the USB-card-drop recovery path). From `INTAKE_MODEL_NAS_DIR`.
pub fn intake_model_nas_dir() -> Option<String> {
    env_nonempty("INTAKE_MODEL_NAS_DIR")
}

/// Absolute path to the `nominations.json` produced by ASMT-08, under the
/// reliable NAS staging dir. `None` when staging is unconfigured.
pub fn intake_nominations_path() -> Option<String> {
    intake_staging_dir().map(|d| format!("{}/nominations.json", d.trim_end_matches('/')))
}

/// Absolute path to the resume checkpoint file (the reboot-survivable record of
/// completed per-(model, backend, dimension) work), under the reliable NAS
/// staging dir. `None` when staging is unconfigured.
pub fn intake_checkpoint_path() -> Option<String> {
    intake_staging_dir().map(|d| format!("{}/asmt09-checkpoint.json", d.trim_end_matches('/')))
}

/// Command/path for the S83 `gguf_path` acquisition binary (sharded / HF fetch).
/// From `GGUF_PATH_BIN`, default the bare binary name on PATH (not an infra
/// literal — the operator's logged-in toolchain provides it).
pub fn gguf_path_bin() -> String {
    env_nonempty("GGUF_PATH_BIN").unwrap_or_else(|| "gguf_path".to_string())
}

/// The `HSA_OVERRIDE_GFX_VERSION` value used to bring up experimental MoE models
/// on ROCm for the gfx1151 class. From `HSA_OVERRIDE_GFX_VERSION`; `None` ⇒ the
/// acquirer does not set the override (Vulkan-only path).
pub fn hsa_override_gfx_version() -> Option<String> {
    env_nonempty("HSA_OVERRIDE_GFX_VERSION")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    #[test]
    #[serial]
    fn judge_cli_defaults_to_bare_name() {
        std::env::remove_var("JUDGE_CLAUDE_CLI");
        assert_eq!(judge_cli(JudgeProvider::Claude), "claude");
        assert_eq!(judge_cli(JudgeProvider::Gemini), "gemini");
        assert_eq!(judge_cli(JudgeProvider::Codex), "codex");
    }

    #[test]
    #[serial]
    fn judge_cli_honors_override() {
        std::env::set_var("JUDGE_CODEX_CLI", "/usr/local/bin/codex-wrapper");
        assert_eq!(judge_cli(JudgeProvider::Codex), "/usr/local/bin/codex-wrapper");
        std::env::remove_var("JUDGE_CODEX_CLI");
    }

    #[test]
    #[serial]
    fn judge_model_is_optional() {
        std::env::remove_var("JUDGE_GEMINI_MODEL");
        assert_eq!(judge_model(JudgeProvider::Gemini), None);
        std::env::set_var("JUDGE_GEMINI_MODEL", "gemini-2.5-pro");
        assert_eq!(
            judge_model(JudgeProvider::Gemini),
            Some("gemini-2.5-pro".to_string())
        );
        std::env::remove_var("JUDGE_GEMINI_MODEL");
    }

    #[test]
    fn provider_ids_stable() {
        assert_eq!(JudgeProvider::all().map(|p| p.id()), ["claude", "gemini", "codex"]);
    }
}
