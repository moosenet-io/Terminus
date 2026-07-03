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

/// Split-topology judge host from `JUDGE_SSH_HOST` (e.g. `user@judge-host`).
/// `Some` ⇒ every judge CLI is invoked over `ssh <host>` instead of locally —
/// the runner lives on the inference host, but the judge CLIs are OAuth-logged-in
/// on `host`. `None` ⇒ shell out locally (single-host topology).
pub fn judge_ssh_host() -> Option<String> {
    env_nonempty("JUDGE_SSH_HOST")
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

// ── S85 SRV-01: serving-runtime command names/paths ──────────────────────────
//
// The serving harness (SRV-02/03) and Chord (SRV-04..06) launch three runtimes:
// the HIP `llama-server` binary, the primary (GPU) ollama unit, and the secondary
// (CPU) ollama unit. Both the launch BINARY and the runtime ENDPOINT for each are
// read from env here — NEVER a literal in runner/probe/Chord code — so the
// `pii_gate` hook never sees a hardcoded host/path in the serving code. Binary
// defaults are bare names on PATH (the operator's logged-in toolchain provides
// them); endpoints have NO default (a `None` makes the caller raise
// `NotConfigured` rather than guessing an infra host).

/// Launch command for the HIP `llama-server` binary (llama.cpp-rocm tier). From
/// `LLAMA_SERVER_BIN`, default the bare binary on PATH (not an infra literal).
pub fn llama_server_bin() -> String {
    env_nonempty("LLAMA_SERVER_BIN").unwrap_or_else(|| "llama-server".to_string())
}

/// HTTP endpoint of the running `llama-server` (health-check + serve target).
/// From `LLAMA_SERVER_URL`; `None` ⇒ caller raises `NotConfigured` (no infra
/// host guessed).
pub fn llama_server_url() -> Option<String> {
    env_nonempty("LLAMA_SERVER_URL")
}

/// Launch command for the primary (GPU) ollama unit (ollama-rocm tier). From
/// `OLLAMA_BIN`, default the bare `ollama` binary on PATH.
pub fn ollama_bin() -> String {
    env_nonempty("OLLAMA_BIN").unwrap_or_else(|| "ollama".to_string())
}

/// HTTP endpoint of the primary (GPU) ollama unit. From `OLLAMA_URL`; `None` ⇒
/// caller raises `NotConfigured` (no infra host guessed).
pub fn ollama_primary_url() -> Option<String> {
    env_nonempty("OLLAMA_URL")
}

/// HTTP endpoint of the secondary (CPU) ollama unit (the genuine-CPU tier). From
/// `OLLAMA_CPU_URL`; `None` ⇒ caller raises `NotConfigured` (no infra host
/// guessed).
pub fn ollama_secondary_url() -> Option<String> {
    env_nonempty("OLLAMA_CPU_URL")
}

/// The cpu-runtime library override the secondary ollama unit / CPU serve uses
/// (the empty-gfx-override CPU path). From `OLLAMA_CPU_LIBRARY`; `None` ⇒ no
/// explicit cpu lib set.
pub fn ollama_cpu_library() -> Option<String> {
    env_nonempty("OLLAMA_CPU_LIBRARY")
}

/// The host's `HSA_OVERRIDE_GFX_VERSION` value to apply when a serving-profile row
/// asks for the gfx override (the runner records `gfx_override: true`, i.e. "apply
/// the host's gfx override", not the literal version — the version is a host
/// constant, not per-model data). From `CHORD_GFX_OVERRIDE_VERSION`; `None` ⇒
/// unset → the launcher omits the override rather than guess a value (pii_gate).
/// A row carrying an explicit gfx string (the CPU empty-override path or a pinned
/// value) is honored directly and never consults this helper.
pub fn gfx_override_version() -> Option<String> {
    env_nonempty("CHORD_GFX_OVERRIDE_VERSION")
}

/// Cold-load threshold (seconds) above which a serving row is marked `keep_warm`.
/// From `SERVING_KEEP_WARM_THRESHOLD_SECS`, default 120 (the v2-sweep lesson:
/// the big MoEs cold-load in ~8–10 min and must be held resident).
pub fn serving_keep_warm_threshold_secs() -> f64 {
    env_nonempty("SERVING_KEEP_WARM_THRESHOLD_SECS")
        .and_then(|v| v.parse().ok())
        .unwrap_or(120.0)
}

// ── S85 SRV-07: Chord residency state + control endpoint (terminus tools) ─────
//
// The SRV-07 status/control tools READ the residency snapshot SRV-05 writes and
// SIGNAL Chord to reload its routing map. Both the state-file PATH and the Chord
// control ENDPOINT are sourced from env here — NEVER a literal in the tool code —
// so the `pii_gate` hook never sees a hardcoded mount/host in the serving tools.
// Neither has a default: a `None` makes the tool return a clear `NotConfigured`
// rather than guessing an infra path/host.

/// Filesystem path to the residency-state snapshot SRV-05's residency manager
/// writes (current residents, free VRAM, the pinned chat role). The
/// `serving_residency_status` tool reads it. From `CHORD_RESIDENCY_STATE_PATH`;
/// `None` ⇒ the tool returns `NotConfigured` (no mount guessed). Tests point this
/// at a temp file.
pub fn chord_residency_state_path() -> Option<String> {
    env_nonempty("CHORD_RESIDENCY_STATE_PATH")
}

/// HTTP control endpoint Chord exposes for a routing-map reload. The
/// `serving_profile_refresh` tool POSTs to it. From `CHORD_CONTROL_URL`; `None` ⇒
/// the tool returns `NotConfigured` (no infra host guessed). Tests point this at a
/// mock server.
pub fn chord_control_url() -> Option<String> {
    env_nonempty("CHORD_CONTROL_URL")
}

/// The CURRENT llama.cpp (HIP `llama-server`) build identifier (S85 SRV-03).
///
/// Stamped onto a `--recheck-build-conditional` run so the drift report can say
/// "rechecked against build X" and an unchanged row records "still
/// build-incompatible at build X". The operator sets `LLAMA_CPP_BUILD_ID` to the
/// build tag they just upgraded to (e.g. the `b####` release / commit) BEFORE
/// pulling the recheck trigger. NO literal here — an unset build id makes the
/// caller raise `NotConfigured` rather than recording a guessed/empty build,
/// which would silently poison the "rechecked against build X" provenance.
pub fn llama_cpp_build_id() -> Option<String> {
    env_nonempty("LLAMA_CPP_BUILD_ID")
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

    #[test]
    #[serial]
    fn judge_ssh_host_none_when_unset_or_blank() {
        std::env::remove_var("JUDGE_SSH_HOST");
        assert_eq!(judge_ssh_host(), None);
        std::env::set_var("JUDGE_SSH_HOST", "   ");
        assert_eq!(judge_ssh_host(), None);
        std::env::remove_var("JUDGE_SSH_HOST");
    }

    #[test]
    #[serial]
    fn judge_ssh_host_reads_and_trims_set_value() {
        std::env::set_var("JUDGE_SSH_HOST", "  user@judge-host  ");
        assert_eq!(judge_ssh_host(), Some("user@judge-host".to_string()));
        std::env::remove_var("JUDGE_SSH_HOST");
    }

    #[test]
    #[serial]
    fn serving_runtime_bins_default_to_bare_names() {
        std::env::remove_var("LLAMA_SERVER_BIN");
        std::env::remove_var("OLLAMA_BIN");
        assert_eq!(llama_server_bin(), "llama-server");
        assert_eq!(ollama_bin(), "ollama");
    }

    #[test]
    #[serial]
    fn serving_runtime_bins_honor_override() {
        std::env::set_var("LLAMA_SERVER_BIN", "/opt/rocm/bin/llama-server");
        assert_eq!(llama_server_bin(), "/opt/rocm/bin/llama-server");
        std::env::remove_var("LLAMA_SERVER_BIN");
    }

    #[test]
    #[serial]
    fn llama_cpp_build_id_has_no_default() {
        std::env::remove_var("LLAMA_CPP_BUILD_ID");
        // Unset ⇒ None (the recheck caller raises NotConfigured, never records a
        // guessed/empty build id).
        assert_eq!(llama_cpp_build_id(), None);
        std::env::set_var("LLAMA_CPP_BUILD_ID", "b1402");
        assert_eq!(llama_cpp_build_id(), Some("b1402".to_string()));
        std::env::remove_var("LLAMA_CPP_BUILD_ID");
    }

    #[test]
    #[serial]
    fn serving_endpoints_have_no_default() {
        std::env::remove_var("LLAMA_SERVER_URL");
        std::env::remove_var("OLLAMA_URL");
        std::env::remove_var("OLLAMA_CPU_URL");
        // No literal infra host is guessed when unset.
        assert_eq!(llama_server_url(), None);
        assert_eq!(ollama_primary_url(), None);
        assert_eq!(ollama_secondary_url(), None);
    }

    #[test]
    #[serial]
    fn keep_warm_threshold_defaults_and_parses() {
        std::env::remove_var("SERVING_KEEP_WARM_THRESHOLD_SECS");
        assert_eq!(serving_keep_warm_threshold_secs(), 120.0);
        std::env::set_var("SERVING_KEEP_WARM_THRESHOLD_SECS", "300");
        assert_eq!(serving_keep_warm_threshold_secs(), 300.0);
        std::env::remove_var("SERVING_KEEP_WARM_THRESHOLD_SECS");
    }

    #[test]
    #[serial]
    fn chord_residency_and_control_have_no_default() {
        std::env::remove_var("CHORD_RESIDENCY_STATE_PATH");
        std::env::remove_var("CHORD_CONTROL_URL");
        // No literal infra path/host is guessed when unset.
        assert_eq!(chord_residency_state_path(), None);
        assert_eq!(chord_control_url(), None);
    }

    #[test]
    #[serial]
    fn chord_residency_and_control_honor_override() {
        std::env::set_var("CHORD_RESIDENCY_STATE_PATH", "/tmp/residency.json");
        std::env::set_var("CHORD_CONTROL_URL", "http://control.invalid:9/x");
        assert_eq!(
            chord_residency_state_path(),
            Some("/tmp/residency.json".to_string())
        );
        assert_eq!(
            chord_control_url(),
            Some("http://control.invalid:9/x".to_string())
        );
        std::env::remove_var("CHORD_RESIDENCY_STATE_PATH");
        std::env::remove_var("CHORD_CONTROL_URL");
    }
}
