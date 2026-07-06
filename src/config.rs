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

// ── MINT Phase 4: breakfix reasoning backend ─────────────────────────────────
//
// The breakfix subagent's PRIMARY reasoning backend is a headless `claude` CLI
// subprocess; its FALLBACK is a local CPU-backed Ollama (deliberately NOT the
// GPU backend — the whole point of breakfix is diagnosing a possibly-wedged
// GPU, so the diagnostic reasoning itself must never compete for that same
// GPU). All four knobs below are env-sourced, mirroring the judge-CLI
// convention above (no literals in `intake::breakfix`).

/// The `claude` CLI binary name/path, from `MINT_BREAKFIX_CLAUDE_CLI`. Falls
/// back to the bare `claude` name (assumed on `PATH` for a logged-in operator,
/// same convention as [`judge_cli`]).
pub fn breakfix_claude_cli() -> String {
    env_nonempty("MINT_BREAKFIX_CLAUDE_CLI").unwrap_or_else(|| "claude".to_string())
}

/// Model passed to the primary `claude` CLI via `--model`, from
/// `MINT_BREAKFIX_CLAUDE_MODEL`. Defaults to `sonnet` (a bare model alias
/// rather than a dated snapshot id, so this stays valid as the CLI's aliases
/// roll forward).
pub fn breakfix_claude_model() -> String {
    env_nonempty("MINT_BREAKFIX_CLAUDE_MODEL").unwrap_or_else(|| "sonnet".to_string())
}

/// Base URL of the local CPU-backed Ollama fallback, from the SAME
/// `OLLAMA_CPU_URL` var [`ollama_secondary_url`] reads (one env var, one
/// meaning: the fleet's CPU-backed Ollama). Unlike that sibling accessor
/// (which returns `None` on unset — its callers raise `NotConfigured`), this
/// one defaults to `http://127.0.0.1:11435` per the Phase-4 spec: breakfix's
/// fallback reasoning must degrade gracefully even on a host where the var
/// was never set, rather than failing the whole breakfix attempt over a
/// missing config for what is already a best-effort fallback path.
/// Deliberately NOT the GPU-serving Ollama's port/backend (see module doc
/// above — the whole point of breakfix is diagnosing a possibly-wedged GPU,
/// so its own reasoning must never contend for that GPU).
pub fn breakfix_ollama_cpu_url() -> String {
    env_nonempty("OLLAMA_CPU_URL").unwrap_or_else(|| "http://127.0.0.1:11435".to_string())
}

/// Model requested from the CPU Ollama fallback, from
/// `MINT_BREAKFIX_FALLBACK_MODEL`. Defaults to a small/fast model already
/// referenced elsewhere in this fleet's serving stack.
pub fn breakfix_fallback_model() -> String {
    env_nonempty("MINT_BREAKFIX_FALLBACK_MODEL").unwrap_or_else(|| "qwen2.5:7b".to_string())
}

/// Wall-clock timeout (seconds) for a single reasoning-backend call, from
/// `MINT_BREAKFIX_TIMEOUT_SECS`. Default 120 — mirrors [`judge_timeout_secs`].
pub fn breakfix_timeout_secs() -> u64 {
    env_nonempty("MINT_BREAKFIX_TIMEOUT_SECS")
        .and_then(|v| v.parse().ok())
        .unwrap_or(120)
}

/// Wall-clock cap (seconds) on a single-case retest's GPU-authority acquire,
/// from `MINT_BREAKFIX_GPU_ACQUIRE_TIMEOUT_SECS`. Default 60.
///
/// Caught in review: `gpu_authority::acquire`'s reconciliation path shells out
/// to `systemctl restart`/`stop` with NO timeout of its own, and breakfix
/// calls it from INSIDE the supervisor daemon's single tick loop (via
/// `block_in_place` + `Handle::current().block_on`) — precisely in the
/// scenario (a GPU already pegged/jammed) where a `systemctl` operation is
/// most likely to itself hang. Without a bound here, that would wedge the
/// ENTIRE daemon forever (no further ticks, no prompt SIGTERM response) —
/// see `breakfix::bounded_blocking`, which this value feeds.
pub fn breakfix_gpu_acquire_timeout_secs() -> u64 {
    env_nonempty("MINT_BREAKFIX_GPU_ACQUIRE_TIMEOUT_SECS")
        .and_then(|v| v.parse().ok())
        .unwrap_or(60)
}

/// Wall-clock cap (seconds) on breakfix's OWN `fetch_model` tool call (MINT
/// Phase 5), from `MINT_BREAKFIX_FETCH_MODEL_TIMEOUT_SECS`. Default 120.
///
/// Flagged in adversarial review: `chord_pull::fetch_model` already carries
/// its own generous HTTP timeout (`MINT_FETCH_MODEL_TIMEOUT_SECS`, default
/// 600s — sized for an operator's `mint fetch-model` CLI call legitimately
/// waiting out a multi-GB archive copy). But breakfix's call to the SAME
/// function runs inside the supervisor daemon's single tick task (same
/// `block_in_place` + `block_on` bridge documented on
/// `breakfix::bounded_blocking`), where a merely-slow-but-alive Chord — not
/// even fully hung, just slow — would otherwise stall EVERY combo's tick for
/// up to the full 600s per attempt (up to `MAX_ATTEMPTS` times). This value
/// is deliberately its OWN, TIGHTER knob rather than reusing
/// `MINT_FETCH_MODEL_TIMEOUT_SECS`: breakfix's bounded diagnostic loop values
/// staying responsive over letting one slow pull run to completion — a
/// timeout here is not treated as fatal, just as evidence fed back into the
/// next reasoning-backend attempt (see `breakfix::decide_breakfix`'s
/// `Verdict::FetchModel` arm), so a real pull that needs more than 120s isn't
/// lost — the next attempt tries again.
pub fn breakfix_fetch_model_timeout_secs() -> u64 {
    env_nonempty("MINT_BREAKFIX_FETCH_MODEL_TIMEOUT_SECS")
        .and_then(|v| v.parse().ok())
        .unwrap_or(120)
}

// ── Meridian (SIMULATED paper-trading sandbox) ────────────────────────────
//
// Ported from <host>'s Python `meridian_tools.py`, which SSH'd to <host> and
// shelled out to a `meridian.py` / `market_data.py` pair under
// `<path>/meridian/`. That directory does not exist on <host> (nor
// anywhere else reachable) — there was never a real backend to port state
// persistence *from*. This module introduces its own local JSON-file
// persistence (whole-document load/save, mirroring `intake`'s
// `Nominations::load()` shape) rather than guessing at a Postgres schema that
// was never observed running.

/// Path to the local JSON file holding the (single, `"default"`) SIMULATED
/// portfolio state. From `MERIDIAN_STATE_PATH`; defaults to a relative file
/// in the process's working directory (this is low-stakes local sandbox
/// state, not shared infra, so a sensible default — rather than a hard
/// `NotConfigured` error — keeps the tool usable out of the box).
pub fn meridian_state_path() -> String {
    env_nonempty("MERIDIAN_STATE_PATH").unwrap_or_else(|| "meridian_portfolio.json".to_string())
}

/// Path the `meridian_report` tool writes its generated HTML dashboard to.
/// From `MERIDIAN_REPORT_PATH`; defaults to a relative file. The Python
/// original published to a fixed docroot on an internal host — no infra
/// literal is hardcoded here; an operator points this at their own docroot
/// via env var, same as every other infra path in this repo.
pub fn meridian_report_path() -> String {
    env_nonempty("MERIDIAN_REPORT_PATH").unwrap_or_else(|| "meridian_report.html".to_string())
}

/// URL reported back to the caller as "where the report was published".
/// From `MERIDIAN_REPORT_URL`; `None` when unset (no infra literal is
/// guessed) — the caller should treat this as "ask the operator where
/// reports are served from" rather than a real published location.
pub fn meridian_report_url() -> Option<String> {
    env_nonempty("MERIDIAN_REPORT_URL")
}

/// Base URL for the CoinGecko public API. From `MERIDIAN_COINGECKO_URL`
/// (test/override hook); defaults to the real public endpoint.
pub fn meridian_coingecko_url() -> String {
    env_nonempty("MERIDIAN_COINGECKO_URL")
        .unwrap_or_else(|| "https://api.coingecko.com".to_string())
}

/// Base URL for the alternative.me Fear & Greed Index API. From
/// `MERIDIAN_FEARGREED_URL` (test/override hook); defaults to the real public
/// endpoint.
pub fn meridian_feargreed_url() -> String {
    env_nonempty("MERIDIAN_FEARGREED_URL")
        .unwrap_or_else(|| "https://api.alternative.me".to_string())
}

/// Base URL for the Stooq quote CSV API (used for the SPY spot quote). From
/// `MERIDIAN_STOOQ_URL` (test/override hook); defaults to the real public
/// endpoint.
pub fn meridian_stooq_url() -> String {
    env_nonempty("MERIDIAN_STOOQ_URL").unwrap_or_else(|| "https://stooq.com".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    // ---- intake_database_url precedence (Phase 2 item 6) ----
    // `storage::get_pool()` (S83's model-intake pool) and
    // `assistant::schema::get_pool()` (S84's) both now delegate to this ONE
    // resolver, so its precedence order — INTAKE_DATABASE_URL wins,
    // DATABASE_URL is the fallback, a blank value counts as unset — is the
    // single source of truth for both. Tested here, pure and network-free
    // (no `PgPool::connect` attempt), rather than by observing a connection
    // failure against a fake host from each pool's own test module.

    #[test]
    #[serial]
    fn intake_database_url_prefers_intake_over_database_url() {
        std::env::set_var("INTAKE_DATABASE_URL", "postgres://intake-wins/db");
        std::env::set_var("DATABASE_URL", "postgres://database-url-loses/db");
        assert_eq!(
            intake_database_url().as_deref(),
            Some("postgres://intake-wins/db")
        );
        std::env::remove_var("INTAKE_DATABASE_URL");
        std::env::remove_var("DATABASE_URL");
    }

    #[test]
    #[serial]
    fn intake_database_url_falls_back_to_database_url() {
        std::env::remove_var("INTAKE_DATABASE_URL");
        std::env::set_var("DATABASE_URL", "postgres://database-url-fallback/db");
        assert_eq!(
            intake_database_url().as_deref(),
            Some("postgres://database-url-fallback/db")
        );
        std::env::remove_var("DATABASE_URL");
    }

    #[test]
    #[serial]
    fn intake_database_url_none_when_both_unset() {
        std::env::remove_var("INTAKE_DATABASE_URL");
        std::env::remove_var("DATABASE_URL");
        assert_eq!(intake_database_url(), None);
    }

    #[test]
    #[serial]
    fn intake_database_url_blank_intake_value_falls_back() {
        // A blank INTAKE_DATABASE_URL must be treated as unset, not as a
        // literal empty-string DB URL — same tolerance `env_nonempty` gives
        // every other setting in this module.
        std::env::set_var("INTAKE_DATABASE_URL", "   ");
        std::env::set_var("DATABASE_URL", "postgres://database-url-fallback/db");
        assert_eq!(
            intake_database_url().as_deref(),
            Some("postgres://database-url-fallback/db")
        );
        std::env::remove_var("INTAKE_DATABASE_URL");
        std::env::remove_var("DATABASE_URL");
    }

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

    // ---- MINT Phase 4 breakfix config ----

    #[test]
    #[serial]
    fn breakfix_claude_defaults_and_overrides() {
        std::env::remove_var("MINT_BREAKFIX_CLAUDE_CLI");
        std::env::remove_var("MINT_BREAKFIX_CLAUDE_MODEL");
        assert_eq!(breakfix_claude_cli(), "claude");
        assert_eq!(breakfix_claude_model(), "sonnet");
        std::env::set_var("MINT_BREAKFIX_CLAUDE_CLI", "/opt/bin/claude-wrapper");
        std::env::set_var("MINT_BREAKFIX_CLAUDE_MODEL", "opus");
        assert_eq!(breakfix_claude_cli(), "/opt/bin/claude-wrapper");
        assert_eq!(breakfix_claude_model(), "opus");
        std::env::remove_var("MINT_BREAKFIX_CLAUDE_CLI");
        std::env::remove_var("MINT_BREAKFIX_CLAUDE_MODEL");
    }

    #[test]
    #[serial]
    fn breakfix_ollama_fallback_defaults_unlike_sibling_accessor() {
        std::env::remove_var("OLLAMA_CPU_URL");
        std::env::remove_var("MINT_BREAKFIX_FALLBACK_MODEL");
        // Unlike `ollama_secondary_url()` (None on unset), the breakfix
        // accessor for the SAME var defaults rather than failing.
        assert_eq!(ollama_secondary_url(), None);
        assert_eq!(breakfix_ollama_cpu_url(), "http://127.0.0.1:11435");
        assert_eq!(breakfix_fallback_model(), "qwen2.5:7b");
        std::env::set_var("OLLAMA_CPU_URL", "http://<internal-ip>:11435");
        std::env::set_var("MINT_BREAKFIX_FALLBACK_MODEL", "phi3:mini");
        assert_eq!(breakfix_ollama_cpu_url(), "http://<internal-ip>:11435");
        assert_eq!(breakfix_fallback_model(), "phi3:mini");
        std::env::remove_var("OLLAMA_CPU_URL");
        std::env::remove_var("MINT_BREAKFIX_FALLBACK_MODEL");
    }

    #[test]
    #[serial]
    fn breakfix_timeout_defaults_and_parses() {
        std::env::remove_var("MINT_BREAKFIX_TIMEOUT_SECS");
        assert_eq!(breakfix_timeout_secs(), 120);
        std::env::set_var("MINT_BREAKFIX_TIMEOUT_SECS", "45");
        assert_eq!(breakfix_timeout_secs(), 45);
        std::env::remove_var("MINT_BREAKFIX_TIMEOUT_SECS");
    }

    #[test]
    #[serial]
    fn breakfix_gpu_acquire_timeout_defaults_and_parses() {
        std::env::remove_var("MINT_BREAKFIX_GPU_ACQUIRE_TIMEOUT_SECS");
        assert_eq!(breakfix_gpu_acquire_timeout_secs(), 60);
        std::env::set_var("MINT_BREAKFIX_GPU_ACQUIRE_TIMEOUT_SECS", "15");
        assert_eq!(breakfix_gpu_acquire_timeout_secs(), 15);
        std::env::remove_var("MINT_BREAKFIX_GPU_ACQUIRE_TIMEOUT_SECS");
    }

    #[test]
    #[serial]
    fn breakfix_fetch_model_timeout_defaults_and_parses() {
        std::env::remove_var("MINT_BREAKFIX_FETCH_MODEL_TIMEOUT_SECS");
        assert_eq!(breakfix_fetch_model_timeout_secs(), 120);
        std::env::set_var("MINT_BREAKFIX_FETCH_MODEL_TIMEOUT_SECS", "30");
        assert_eq!(breakfix_fetch_model_timeout_secs(), 30);
        std::env::remove_var("MINT_BREAKFIX_FETCH_MODEL_TIMEOUT_SECS");
    }

    #[test]
    #[serial]
    fn meridian_state_path_defaults_and_overrides() {
        std::env::remove_var("MERIDIAN_STATE_PATH");
        assert_eq!(meridian_state_path(), "meridian_portfolio.json");
        std::env::set_var("MERIDIAN_STATE_PATH", "/tmp/custom.json");
        assert_eq!(meridian_state_path(), "/tmp/custom.json");
        std::env::remove_var("MERIDIAN_STATE_PATH");
    }

    #[test]
    #[serial]
    fn meridian_report_path_and_url_default_and_override() {
        std::env::remove_var("MERIDIAN_REPORT_PATH");
        std::env::remove_var("MERIDIAN_REPORT_URL");
        assert_eq!(meridian_report_path(), "meridian_report.html");
        assert_eq!(meridian_report_url(), None);
        std::env::set_var("MERIDIAN_REPORT_PATH", "/tmp/report.html");
        std::env::set_var("MERIDIAN_REPORT_URL", "http://example.test/trading/");
        assert_eq!(meridian_report_path(), "/tmp/report.html");
        assert_eq!(
            meridian_report_url().as_deref(),
            Some("http://example.test/trading/")
        );
        std::env::remove_var("MERIDIAN_REPORT_PATH");
        std::env::remove_var("MERIDIAN_REPORT_URL");
    }

    #[test]
    #[serial]
    fn meridian_external_api_urls_default_and_override() {
        std::env::remove_var("MERIDIAN_COINGECKO_URL");
        std::env::remove_var("MERIDIAN_FEARGREED_URL");
        std::env::remove_var("MERIDIAN_STOOQ_URL");
        assert_eq!(meridian_coingecko_url(), "https://api.coingecko.com");
        assert_eq!(meridian_feargreed_url(), "https://api.alternative.me");
        assert_eq!(meridian_stooq_url(), "https://stooq.com");
        std::env::set_var("MERIDIAN_COINGECKO_URL", "http://mock/cg");
        std::env::set_var("MERIDIAN_FEARGREED_URL", "http://mock/fg");
        std::env::set_var("MERIDIAN_STOOQ_URL", "http://mock/st");
        assert_eq!(meridian_coingecko_url(), "http://mock/cg");
        assert_eq!(meridian_feargreed_url(), "http://mock/fg");
        assert_eq!(meridian_stooq_url(), "http://mock/st");
        std::env::remove_var("MERIDIAN_COINGECKO_URL");
        std::env::remove_var("MERIDIAN_FEARGREED_URL");
        std::env::remove_var("MERIDIAN_STOOQ_URL");
    }
}
