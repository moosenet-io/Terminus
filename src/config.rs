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

/// Postgres URL for the Atlas KG semantic-embeddings store (`kg_embeddings`
/// table, pgvector). Prefers a dedicated `ATLAS_DATABASE_URL`, falls back to
/// the shared `DATABASE_URL` (mirrors [`intake_database_url`]).
/// Returns `None` (caller raises `NotConfigured`) when neither is set.
pub fn atlas_database_url() -> Option<String> {
    // Dedicated DSN ONLY — deliberately no `DATABASE_URL` fallback. The atlas
    // embeddings store is an isolated database; falling back to a shared
    // `DATABASE_URL` would run pgvector migrations against an unrelated DB and
    // make `from_env()` connect when `ATLAS_DATABASE_URL` is unset (the store's
    // contract is: unset ⇒ NotConfigured, never connects).
    env_nonempty("ATLAS_DATABASE_URL")
}

/// Key-NAME prefix for `crate::pg`'s per-identity Postgres connection secrets.
/// A secret named `POSTGRES_URL_<NAME>` (e.g. `POSTGRES_URL_READONLY`)
/// configures the `<name>` (lowercased) connection identity. Single source of
/// truth for the prefix, mirroring `crate::plane`'s `PLANE_PAT_<NAME>`
/// convention for Plane identities.
const PG_CONNECTION_SECRET_PREFIX: &str = "POSTGRES_URL_";

/// The vault/secret-store KEY NAME (never the value) that carries the
/// connection URL for a given `pg_*` connection identity, e.g.
/// `pg_connection_secret_name("readonly")` => `"POSTGRES_URL_READONLY"`.
///
/// This function ONLY builds and returns the key NAME — it never reads the
/// secret store itself. The one sanctioned read site for the VALUE is
/// `crate::pg::conn` (mirroring how `crate::plane::PLANE_IDENTITY_PREFIX`
/// names a prefix without itself reading any `PLANE_PAT_<NAME>` value).
pub fn pg_connection_secret_name(identity: &str) -> String {
    format!("{PG_CONNECTION_SECRET_PREFIX}{}", identity.trim().to_uppercase())
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
/// meaning: the fleet's CPU-backed Ollama).
///
/// PII remediation (2026-07): this used to default to a compiled-in loopback
/// address when unset. Per explicit operator decision, that real-value
/// fallback has been removed. This is now a thin, `None`-on-unset alias for
/// [`ollama_secondary_url`] (kept as a distinct name for readability at
/// breakfix call sites) — callers must treat `None` as "this fallback
/// backend is unavailable," not substitute a guessed local address.
/// Deliberately NOT the GPU-serving Ollama's port/backend (see module doc
/// above — the whole point of breakfix is diagnosing a possibly-wedged GPU,
/// so its own reasoning must never contend for that GPU).
pub fn breakfix_ollama_cpu_url() -> Option<String> {
    ollama_secondary_url()
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
// Ported from the legacy host's Python `meridian_tools.py`, which SSH'd to the fleet
// host and shelled out to a `meridian.py` / `market_data.py` pair under the
// fleet host's Meridian directory. That directory does not exist on the fleet host (nor
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

// ── TCLI-01: embedded CA (`crate::pki`) storage — non-secret path only ────────
//
// The CA key/cert MATERIAL always goes through `crate::pki`'s own
// load-or-generate logic (env-materialized secret store first, this local
// path as the fallback tier) — this helper only resolves a *path name*, never
// the material itself. See `crate::pki` module docs for the full precedence
// and the "no secret-store write path in this crate" rationale for why a
// local store tier exists at all.

/// Local fallback persistence path for the embedded CA when no
/// `TERMINUS_CA_CERT`/`TERMINUS_CA_KEY` are provisioned via the runtime
/// secret store. From `TERMINUS_CA_STORE_PATH`; defaults to
/// `~/.terminus/pki/ca_store.json` (or a relative fallback if the home
/// directory can't be resolved). This is a path name only — non-secret.
pub fn ca_store_path() -> String {
    env_nonempty("TERMINUS_CA_STORE_PATH").unwrap_or_else(default_ca_store_path)
}

fn default_ca_store_path() -> String {
    dirs::home_dir()
        .map(|home| home.join(".terminus").join("pki").join("ca_store.json"))
        .and_then(|p| p.to_str().map(str::to_string))
        .unwrap_or_else(|| ".terminus/pki/ca_store.json".to_string())
}

// ── TCLI-02: enrollment endpoint config (non-secret) ──────────────────────────
//
// The enrollment BOOTSTRAP SECRET (`TERMINUS_ENROLLMENT_SHARED_SECRET`) and the
// JWT signing key (`TERMINUS_JWT_SIGNING_KEY`) are secret material and are read
// directly from the env-materialized runtime secret store inside
// `crate::pki::enroll` (same convention as `crate::pki`'s CA material — see
// that module's doc comment for why this crate has no separate
// `SecretManager`/`vault` API of its own). This section only resolves
// non-secret knobs: path, TTLs.

/// HTTP path the enrollment endpoint is mounted at (merged into whichever
/// router a binary builds — see `crate::pki::enroll::build_enroll_router`).
/// From `TERMINUS_ENROLLMENT_PATH`; defaults to `/enroll`.
pub fn enrollment_path() -> String {
    env_nonempty("TERMINUS_ENROLLMENT_PATH").unwrap_or_else(|| "/enroll".to_string())
}

/// Issued client-cert TTL, in hours. Deliberately short-lived compared to the
/// CA's multi-year validity (see `crate::pki::ca::CA_FORWARD_YEARS`) — this is
/// a leaf cert meant to be re-enrolled periodically, not a long-lived
/// credential. From `TERMINUS_ENROLLMENT_CERT_TTL_HOURS`; defaults to 24h.
pub fn enrollment_cert_ttl_hours() -> i64 {
    env_nonempty("TERMINUS_ENROLLMENT_CERT_TTL_HOURS")
        .and_then(|v| v.parse().ok())
        .filter(|h: &i64| *h > 0)
        .unwrap_or(24)
}

/// Issued JWT TTL, in seconds. Matching-or-shorter than the paired cert's TTL
/// per the TCLI-02 spec item (belt-and-suspenders: the JWT is the
/// application-layer claim, the cert is the transport-layer identity — the
/// JWT should never outlive the cert it's paired with). From
/// `TERMINUS_ENROLLMENT_JWT_TTL_SECONDS`; defaults to 1800s (30 minutes),
/// comfortably shorter than the default 24h cert TTL.
pub fn enrollment_jwt_ttl_seconds() -> i64 {
    env_nonempty("TERMINUS_ENROLLMENT_JWT_TTL_SECONDS")
        .and_then(|v| v.parse().ok())
        .filter(|s: &i64| *s > 0)
        .unwrap_or(1800)
}

// ── TCLI-03: mTLS listener config (non-secret) ─────────────────────────────
//
// The mTLS listener's TLS material (CA + server cert/key) is PKI material,
// not a plain secret string, and is handled entirely by `crate::pki`/
// `crate::pki::mtls` (load-or-generate CA, issue-on-startup server cert) --
// this section only resolves non-secret knobs: bind address, port, TTL,
// server identity name. Deliberately a SEPARATE port from
// `TERMINUS_PERSONAL_PORT`/`TERMINUS_PERSONAL_BIND` (the existing plain
// HTTP+JWT listener) -- this listener is additive, not a replacement, and
// must never collide with it (see `crate::pki::mtls` module docs).

/// Bind address for the mTLS listener. From `TERMINUS_MTLS_BIND`; defaults
/// to `127.0.0.1`, matching the existing plain listener's default posture
/// (`crate::bin::terminus_personal`'s `TERMINUS_PERSONAL_BIND` default) --
/// an operator opts into a wider bind explicitly for either listener.
pub fn mtls_bind_addr() -> String {
    env_nonempty("TERMINUS_MTLS_BIND").unwrap_or_else(|| "127.0.0.1".to_string())
}

/// Bind port for the mTLS listener. From `TERMINUS_MTLS_PORT`; defaults to
/// `8301` -- one past the existing plain listener's default `8300`
/// (`TERMINUS_PERSONAL_PORT`), never the same port (this listener is a
/// second, additive one, not a replacement).
pub fn mtls_port() -> u16 {
    env_nonempty("TERMINUS_MTLS_PORT")
        .and_then(|v| v.parse().ok())
        .unwrap_or(8301)
}

/// The terminus primary's own mTLS server-cert identity name, embedded in
/// CN/SAN at issuance (`crate::pki::mtls::issue_server_cert`). From
/// `TERMINUS_MTLS_SERVER_IDENTITY`; defaults to `terminus-primary`. Purely
/// an operator-facing label -- plays no role in client-side authz (a server
/// cert is not client input).
pub fn mtls_server_identity() -> String {
    env_nonempty("TERMINUS_MTLS_SERVER_IDENTITY").unwrap_or_else(|| "terminus-primary".to_string())
}

/// Validity window, in days, for the terminus primary's own mTLS server
/// cert. From `TERMINUS_MTLS_SERVER_CERT_TTL_DAYS`; defaults to 365 --
/// deliberately much longer than TCLI-02's per-client leaf cert TTL (see
/// `crate::pki::mtls`'s module doc "server cert issuance" section for why).
pub fn mtls_server_cert_ttl_days() -> i64 {
    env_nonempty("TERMINUS_MTLS_SERVER_CERT_TTL_DAYS")
        .and_then(|v| v.parse().ok())
        .filter(|d: &i64| *d > 0)
        .unwrap_or(365)
}

// ── TGW-01: terminus-primary mTLS listener config (non-secret) ────────────
//
// The new `terminus-primary` binary (aggregated-core-registry gateway)
// needs its OWN bind/port/identity config, deliberately NOT reusing
// `TERMINUS_MTLS_*` (terminus_personal's own var family) — the two binaries
// are meant to be able to run side by side (see the TGW-01 spec item's
// design decision #1, "ALONGSIDE"), including on the same host during
// testing/dev, so sharing a var family would make their default ports
// collide. CA/PKI material itself is still resolved the normal way
// (`crate::pki::ca()`'s env-then-local-store-then-generate precedence,
// unchanged) — each process's own environment naturally gives it its own
// independently-provisioned CA (TGW-01 design decision #3), no special
// casing needed here.

/// Bind address for `terminus-primary`'s mTLS listener. From
/// `TERMINUS_PRIMARY_MTLS_BIND`; defaults to `127.0.0.1`, matching the same
/// "opt into a wider bind explicitly" posture as every other listener in
/// this crate.
pub fn mtls_primary_bind_addr() -> String {
    env_nonempty("TERMINUS_PRIMARY_MTLS_BIND").unwrap_or_else(|| "127.0.0.1".to_string())
}

/// Bind port for `terminus-primary`'s mTLS listener. From
/// `TERMINUS_PRIMARY_MTLS_PORT`; defaults to `8311` — distinct from
/// `terminus_personal`'s own `TERMINUS_MTLS_PORT` default (`8301`) so both
/// binaries can run concurrently on the same host without a port collision.
pub fn mtls_primary_port() -> u16 {
    env_nonempty("TERMINUS_PRIMARY_MTLS_PORT")
        .and_then(|v| v.parse().ok())
        .unwrap_or(8311)
}

/// `terminus-primary`'s own mTLS server-cert identity name, embedded in
/// CN/SAN at issuance. From `TERMINUS_PRIMARY_MTLS_SERVER_IDENTITY`;
/// defaults to `terminus-primary`. Purely an operator-facing label — plays
/// no role in client-side authz.
pub fn mtls_primary_server_identity() -> String {
    env_nonempty("TERMINUS_PRIMARY_MTLS_SERVER_IDENTITY")
        .unwrap_or_else(|| "terminus-primary".to_string())
}

// ── TGW-02: personal-tool federation via Chord's `/v1/personal/tools/*` ────
//
// Per the S108 spec's RESOLVED design decision (2): `terminus-primary`
// reaches the personal-registry-exclusive tools (the `git_private` set, and
// any other tool name not in its local `register_all` registry) by proxying
// through Chord's EXISTING `/v1/personal/tools/*` relay (already federates
// to the personal-registry deployment's `terminus_personal` — see
// `moosenet/Chord`'s `src/routes.rs`
// `personal_tools_list`/`personal_tools_call`), not a new direct
// primary→personal-registry path. This section resolves the non-secret knobs
// for that hop: Chord's base URL and the federation HTTP client's timeout.
// The JWT SIGNING SECRET used to authenticate to Chord's relay is
// intentionally NOT resolved here (it's read next to its one caller in
// `crate::federation` rather than mixed in with these plain env-var knobs —
// same "PKI/secret material gets its own section" convention this file
// already uses for `crate::pki`).

/// Base URL `terminus-primary` calls to reach Chord's personal-tool relay
/// (`{base}/v1/personal/tools/list`, `{base}/v1/personal/tools/call`). From
/// `TERMINUS_PRIMARY_CHORD_URL`; defaults to Chord's loopback proxy port for
/// a co-located deploy — a loopback default only, never a real non-loopback
/// host baked in. An operator overrides this if Chord is not co-located with
/// `terminus-primary`.
pub fn chord_personal_federation_url() -> String {
    // Loopback default (precedent: `crate::intake::gpu_authority`'s own
    // chord-base-url helper uses the same literal).
    env_nonempty("TERMINUS_PRIMARY_CHORD_URL")
        .unwrap_or_else(|| "http://127.0.0.1:8099".to_string()) // pii-test-fixture
}

/// Timeout, in milliseconds, for a single federated tool call to Chord's
/// `/v1/personal/tools/call`. From `TERMINUS_PRIMARY_CHORD_FEDERATION_TIMEOUT_MS`;
/// defaults to 30000 (30s) — generous enough for a real personal-tool call
/// (Chord itself hops on to the personal-registry deployment), but bounded so
/// a dead/unreachable Chord process fails a caller's request instead of
/// hanging it indefinitely (see TGW-02's spec item edge cases).
pub fn chord_personal_federation_timeout_ms() -> u64 {
    env_nonempty("TERMINUS_PRIMARY_CHORD_FEDERATION_TIMEOUT_MS")
        .and_then(|v| v.parse().ok())
        .filter(|ms: &u64| *ms > 0)
        .unwrap_or(30_000)
}

// ── DOCGEN-05: doc-generation routing via Chord's SLM router ──────────────
// The doc engine (`crate::tools::docgen::generate`) never picks a model
// itself -- per the S95 design overview's seam, Chord's SLM router (DOCGEN-03,
// shipped in `moosenet/Chord`) owns that decision. This crate only needs to
// name the routing tag it sends on `POST /v1/infer`'s `model` field; Chord's
// router resolves the actual backend model from it. Reuses
// [`chord_personal_federation_url`]/[`chord_personal_federation_timeout_ms`]
// for transport (same co-located Chord process, same auth scheme) rather than
// adding a third pair of always-identical base-URL/timeout knobs.

/// The routing tag `ChordDocGenerator` sends as `model` on `POST /v1/infer`
/// for doc-generation requests. From `DOCGEN_CHORD_MODEL`; defaults to
/// `"auto"` (let Chord's SLM router pick), never a literal model/host name.
pub fn docgen_chord_model() -> String {
    env_nonempty("DOCGEN_CHORD_MODEL").unwrap_or_else(|| "auto".to_string())
}

// ── DGDG-01: cloud-provider fallback when local Chord/GPU inference is jammed ──
// The exact failure that blocked the DGRICH rollout: `ChordDocGenerator` errors
// (unreachable/timeout/OOM) with no way to recover for that generation. Rather
// than a new secret/transport story, `crate::tools::docgen::generate::
// OpenRouterDocGenerator` delegates to `crate::review::dispatch::ReviewConfig::
// dispatch_openrouter` -- the SAME OpenRouter chat-completions client
// `nemotron`/`qwen_coder`/`gpt56` already use (same URL resolution, same
// `OPENROUTER_API_KEY` bearer-auth convention -- see that module's doc comment
// for why a plain env read IS the vault read in this crate). This section only
// resolves the ONE new, non-secret knob: which model tag the fallback sends.

/// The OpenRouter model tag `FallbackDocGenerator`'s cloud fallback sends when
/// local Chord/GPU doc-generation inference fails. From
/// `DOCGEN_CLOUD_FALLBACK_MODEL`; `None` (unset or empty) disables the fallback
/// entirely -- `FallbackDocGenerator::from_env` then wires ONLY the existing
/// `ChordDocGenerator` path, byte-for-byte today's pre-DGDG-01 behavior. Never a
/// literal model id: an operator picks whichever OpenRouter model they want
/// doc-generation to fall back to.
pub fn docgen_cloud_fallback_model() -> Option<String> {
    env_nonempty("DOCGEN_CLOUD_FALLBACK_MODEL")
}

/// Fast-fail timeout (ms) for the LOCAL docgen primary when a cloud fallback is
/// configured (DGDG-03). A jammed local inference backend should error quickly
/// so `FallbackDocGenerator` reaches the cloud promptly rather than hanging for
/// the full federation timeout. `DOCGEN_LOCAL_TIMEOUT_MS`, default 45_000 (45s);
/// a non-positive/unparseable value falls back to the default. Only applied when
/// a cloud fallback exists — with none, the primary keeps the full timeout.
pub fn docgen_local_timeout_ms() -> u64 {
    env_nonempty("DOCGEN_LOCAL_TIMEOUT_MS")
        .and_then(|v| v.parse().ok())
        .filter(|ms: &u64| *ms > 0)
        .unwrap_or(45_000)
}

// ── TGW-03: inference proxy to Chord ──────────────────────────────────────
// `terminus-primary` forwards `/v1/chat/completions`, `/v1/infer`,
// `/v1/agent/execute`, and `/v1/coding/select` to the SAME co-located Chord
// process the personal-tool federation above already relays to (confirmed by
// reading Chord's `src/routes.rs`: both `/v1/personal/tools/*` and these
// inference routes are mounted on Chord's one router, behind the same
// `auth_check`/`CHORD_JWT_SECRET` scheme) — so this reuses
// [`chord_personal_federation_url`] rather than adding a second,
// always-identical `TERMINUS_PRIMARY_CHORD_INFERENCE_URL` base-URL knob. Only
// the connect timeout gets its own knob here: inference responses
// (especially streamed ones) can legitimately run far longer than a
// personal-tool call, so this must NOT reuse
// `chord_personal_federation_timeout_ms` as a *total* request timeout (that
// would cut off a long generation mid-stream — see the TGW-03 spec item's
// "very large or long-running inference responses" edge case). The inference
// HTTP client therefore only bounds the initial TCP connect, never the whole
// response body.

/// Connect timeout, in milliseconds, for `terminus-primary`'s hop to Chord's
/// inference routes. From `TERMINUS_PRIMARY_CHORD_INFERENCE_CONNECT_TIMEOUT_MS`;
/// defaults to 5000 (5s) — long enough for a co-located loopback connect under
/// load, short enough that a genuinely down/unreachable Chord process fails
/// fast instead of hanging a caller. Deliberately NOT a total-response
/// timeout: once connected, a streamed inference response is relayed for as
/// long as Chord keeps sending it.
pub fn chord_inference_connect_timeout_ms() -> u64 {
    env_nonempty("TERMINUS_PRIMARY_CHORD_INFERENCE_CONNECT_TIMEOUT_MS")
        .and_then(|v| v.parse().ok())
        .filter(|ms: &u64| *ms > 0)
        .unwrap_or(5_000)
}

// ── TGW-04: gateway framework (identity → allowlist → rate-limit → audit) ──
//
// The uniform per-request pipeline (`crate::gateway_framework`) wraps every
// request path on `terminus-primary` (tool-dispatch AND inference-proxy).
// These are its non-secret config knobs; the allowlist policy itself is
// data (identity -> allowed actions), not a credential, so it's read here
// like any other config, not through the secret-env convention `crate::pki`
// documents.

/// Per-identity allowlist policy, as a JSON object string: `{"<identity>":
/// ["<tool-or-route>", ...], ...}`. A `"*"` entry in an identity's array
/// allows every action for that identity. From
/// `TERMINUS_GATEWAY_ALLOWLIST_JSON`; defaults to `"{}"` (empty policy) — per
/// the TGW-04 spec item's edge case, an identity with NO configured entry is
/// denied, not allowed by default (default-deny), so an empty policy denies
/// every identity until the operator provisions entries.
pub fn gateway_allowlist_json() -> String {
    env_nonempty("TERMINUS_GATEWAY_ALLOWLIST_JSON").unwrap_or_else(|| "{}".to_string())
}

/// The gateway's tailnet MagicDNS name (or any other operator-chosen
/// reachable hostname), for display in a [`crate::mesh::client_onboarding`]
/// (MESH-12) connection profile ONLY — never used to make a connection from
/// this process itself. From `TERMINUS_MESH_GATEWAY_MAGICDNS_NAME`; `None`
/// when unset, since this crate has no legitimate infra literal to default
/// to (a real tailnet hostname is deployment-specific and must be
/// operator-provisioned, never guessed or hardcoded — see the "no hardcoded
/// infrastructure values" acceptance criterion). A `None` here means the
/// emitted client profile carries an explicit placeholder + warning instead
/// of a made-up hostname.
pub fn gateway_magicdns_name() -> Option<String> {
    env_nonempty("TERMINUS_MESH_GATEWAY_MAGICDNS_NAME")
}

/// Token-bucket burst capacity for the interim in-process rate limiter, per
/// `(identity, action)` key. From `TERMINUS_GATEWAY_RATE_LIMIT_BURST`;
/// defaults to 20 — generous enough for a legitimate multi-tool-call
/// workflow (see the TGW-04 spec item's edge case) while still bounding a
/// runaway burst.
pub fn gateway_rate_limit_burst() -> u32 {
    env_nonempty("TERMINUS_GATEWAY_RATE_LIMIT_BURST")
        .and_then(|v| v.parse().ok())
        .filter(|n: &u32| *n > 0)
        .unwrap_or(20)
}

/// Token-bucket refill rate, in tokens per second, for the interim
/// in-process rate limiter. From
/// `TERMINUS_GATEWAY_RATE_LIMIT_REFILL_PER_SEC`; defaults to 5.
pub fn gateway_rate_limit_refill_per_sec() -> f64 {
    env_nonempty("TERMINUS_GATEWAY_RATE_LIMIT_REFILL_PER_SEC")
        .and_then(|v| v.parse().ok())
        .filter(|n: &f64| *n > 0.0)
        .unwrap_or(5.0)
}

// BLD-20: bounded FIFO request-queue knobs for the proxy admission path. When
// the rate limiter says over-limit, the gateway admits the request through the
// Redis FIFO queue with a BOUNDED wait instead of an immediate 429 — only
// shedding load (429) when the queue is full or the wait times out.

/// Max depth of the proxy admission queue. From
/// `TERMINUS_GATEWAY_QUEUE_MAX_DEPTH`; defaults to 128. A request that arrives
/// when the queue is at this depth is shed immediately (429) rather than piling
/// on unbounded backlog.
pub fn gateway_queue_max_depth() -> i64 {
    env_nonempty("TERMINUS_GATEWAY_QUEUE_MAX_DEPTH")
        .and_then(|v| v.parse().ok())
        .filter(|n: &i64| *n > 0)
        .unwrap_or(128)
}

/// Max time an over-limit request waits in the admission queue before it is shed
/// (429). From `TERMINUS_GATEWAY_QUEUE_MAX_WAIT_MS`; defaults to 500ms — bounded
/// so a caller never blocks indefinitely behind the queue.
pub fn gateway_queue_max_wait() -> std::time::Duration {
    let ms = env_nonempty("TERMINUS_GATEWAY_QUEUE_MAX_WAIT_MS")
        .and_then(|v| v.parse().ok())
        .filter(|n: &u64| *n > 0)
        .unwrap_or(500);
    std::time::Duration::from_millis(ms)
}

/// Poll interval while waiting in the admission queue. From
/// `TERMINUS_GATEWAY_QUEUE_POLL_MS`; defaults to 25ms.
pub fn gateway_queue_poll() -> std::time::Duration {
    let ms = env_nonempty("TERMINUS_GATEWAY_QUEUE_POLL_MS")
        .and_then(|v| v.parse().ok())
        .filter(|n: &u64| *n > 0)
        .unwrap_or(25);
    std::time::Duration::from_millis(ms)
}

/// RLQ-01: conservative backoff (seconds) handed to a caller when the
/// rate-limiter BACKEND itself is degraded (Redis unreachable/erroring, or a
/// misconfigured `REDIS_URL` selecting the fail-closed sentinel) — i.e.
/// `RateLimitDecision::Degraded`, not a real over-limit. There is no bucket
/// state to derive an exact recovery time from in this case, so this is a
/// fixed, operator-tunable value rather than a computed one. From
/// `TERMINUS_GATEWAY_RATE_LIMIT_DEGRADED_RETRY_SECS`; defaults to 2.0 — long
/// enough that a naive immediate-retry loop doesn't hammer an already-broken
/// backend, short enough that a caller notices recovery promptly once the
/// backend is fixed.
pub fn gateway_rate_limit_degraded_retry_secs() -> f64 {
    env_nonempty("TERMINUS_GATEWAY_RATE_LIMIT_DEGRADED_RETRY_SECS")
        .and_then(|v| v.parse().ok())
        .filter(|n: &f64| *n > 0.0)
        .unwrap_or(2.0)
}

// ── DISC-04: HF Hub public model-listing client ──────────────────────────────
//
// `intake::discovery::hf_client::HfHubClient` queries the PUBLIC HuggingFace Hub
// models-listing API (no auth token — see the module doc there for why that's a
// deliberate distinction from DISC-08's authenticated fetch). Both knobs below are
// read from env here — NEVER a literal in hf_client.rs — matching this file's own
// convention for every other tool in this crate.

/// Base URL for the public HuggingFace Hub API. From `HF_API_BASE_URL`; defaults to
/// the well-known public HF Hub host. This is a documented PUBLIC API endpoint (the
/// same kind of "well-known default" `model_advisor::mod`'s `OLLAMA_HOST` default
/// already establishes for a well-known *local* endpoint), not an internal infra
/// literal — flagged explicitly as a borderline S1 case in the DISC-04 PR
/// description. Unlike most `Option<String>`-returning helpers in this file, this
/// one always resolves to a value (a listing client with no configured override
/// falls back to the public default rather than raising `NotConfigured`, since a
/// sane default genuinely exists here).
pub fn hf_api_base_url() -> String {
    env_nonempty("HF_API_BASE_URL").unwrap_or_else(|| "https://huggingface.co".to_string())
}

/// Self-imposed rate limit (requests/minute) the HF Hub listing client throttles
/// itself to. From `HF_DISCOVERY_RATE_LIMIT_PER_MIN`, default 30 — a conservative,
/// documented default; HF's public API publishes no hard rate limit for anonymous
/// listing calls, so this is a courtesy self-throttle, not a value HF mandated.
/// A non-positive or unparseable override falls back to the default rather than
/// disabling throttling.
pub fn hf_discovery_rate_limit_per_min() -> u32 {
    env_nonempty("HF_DISCOVERY_RATE_LIMIT_PER_MIN")
        .and_then(|v| v.parse().ok())
        .filter(|n: &u32| *n > 0)
        .unwrap_or(30)
}

// ── KGEMB-02: KG semantic-embeddings client config ────────────────────────
// `crate::scribe::graph::vec_embed::EmbedClient` turns a node "card" (short
// text) into a vector against a configurable endpoint. URL/model/timeout are
// non-secret knobs resolved here; the optional bearer key for hosted
// providers (`EMBEDDINGS_API_KEY`) is secret material and is read directly
// from the env-materialized runtime secret store in `vec_embed` itself (same
// "no separate SecretManager/vault API in this crate" convention documented
// in `crate::pki`'s module doc and used by `review::dispatch`'s
// `OPENROUTER_API_KEY` — a plain env read post-materialization IS the
// SecretManager read here), not in this non-secret config section.

/// HTTP endpoint the KG embeddings client POSTs to. From `EMBEDDINGS_URL`;
/// EMBED-02: now defaults to the co-located Chord process's OpenAI-compatible
/// `/v1/embeddings` proxy ([`chord_personal_federation_url`]'s loopback-only
/// default) rather than a raw Ollama endpoint — Chord fronts the actual
/// embeddings backend (Qwen3-Embedding), so the KG embeddings client should
/// go through the same relay every other Chord-routed call in this crate
/// uses, not talk to Ollama directly. An operator with a non-co-located or
/// still-Ollama deployment overrides via `EMBEDDINGS_URL` (e.g. back to
/// [`ollama_secondary_url`]'s `/api/embeddings` route) — never a real
/// non-loopback host baked in here.
pub fn embeddings_url() -> String {
    env_nonempty("EMBEDDINGS_URL").unwrap_or_else(|| {
        format!("{}/v1/embeddings", chord_personal_federation_url().trim_end_matches('/'))
    })
}

/// Embeddings model name sent on each request. From `EMBEDDINGS_MODEL`;
/// EMBED-02: defaults to `"Qwen3-Embedding"` (1024-dim; see
/// `scribe::graph::vec_store::KG_EMBED_DIM`) now that the default endpoint
/// above is Chord's `/v1/embeddings` proxy rather than raw Ollama — the model
/// name is still just a routing tag Chord resolves, never a literal
/// host/infra value, and is fully overridable.
pub fn embeddings_model() -> String {
    env_nonempty("EMBEDDINGS_MODEL").unwrap_or_else(|| "Qwen3-Embedding".to_string())
}

/// Per-request timeout, in milliseconds, for the embeddings client. From
/// `EMBEDDINGS_TIMEOUT_MS`; defaults to 30000 (30s) — generous enough for a
/// CPU-tier embeddings backend, bounded so a dead/unreachable endpoint fails
/// a single embed call instead of hanging a build indefinitely.
pub fn embeddings_timeout_ms() -> u64 {
    env_nonempty("EMBEDDINGS_TIMEOUT_MS")
        .and_then(|v| v.parse().ok())
        .filter(|ms: &u64| *ms > 0)
        .unwrap_or(30_000)
}

// ── TMOD-02: broker-side per-worker transport config ───────────────────────
//
// Additive: per-worker transport-tier selection + the `MinTierPolicy`
// minimum-tier-floor enforced at CONFIG-LOAD time (not merely at dial time),
// so a `write_scoped`/`secret_holding` worker declared below T2 is rejected
// before the broker ever attempts to reach it — see
// `crate::broker::transport`'s module doc for the tiers and the floor policy
// itself. Mirrors `crate::mesh::registry::UpstreamRegistry`'s
// parse-from-env-JSON + `validate()` shape (unique names, one clear error at
// a time), applied to worker transports instead of upstream mesh servers.
//
// All fields here are STRUCTURAL (tier, capability class, socket path,
// host/port, expected uid/identity) — none of them are secret-shaped. The
// actual cert/key material a T2/T0 worker transport needs comes from the
// crate's embedded CA (`crate::pki::ca`), reused unmodified rather than
// introducing a second secret surface for this item (see
// `crate::broker::transport::uds_mtls`/`mtls_tcp`'s module docs).

/// One worker's transport configuration, as authored by an operator into
/// `TERMINUS_BROKER_WORKERS_JSON`.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct WorkerTransportEntry {
    /// Stable, unique identifier for this worker (e.g. `"gitea-worker"`).
    pub name: String,
    /// The transport tier this worker registers at.
    pub tier: crate::broker::transport::TransportTier,
    /// This worker's declared capability class — what
    /// [`crate::broker::transport::MinTierPolicy`] floors `tier` against.
    pub capability_class: crate::broker::transport::CapabilityClass,
    /// Required for T1/T2 (UDS-based tiers): the worker's listening socket
    /// path.
    #[serde(default)]
    pub socket_path: Option<String>,
    /// Required for T0 (TCP-based tier): the worker's host.
    #[serde(default)]
    pub host: Option<String>,
    /// Required for T0: the worker's port.
    #[serde(default)]
    pub port: Option<u16>,
    /// Required for T1/T2: the uid this worker's process is expected to run
    /// as, checked against `SO_PEERCRED` before any request is sent.
    #[serde(default)]
    pub expected_uid: Option<u32>,
    /// Required for T0/T2 (the mTLS-bearing tiers): the Subject CN this
    /// worker's TLS leaf certificate must carry.
    #[serde(default)]
    pub expected_identity: Option<String>,
}

/// Errors from loading/validating the worker-transport registry. Every
/// variant names the offending worker/field so a misconfigured
/// `TERMINUS_BROKER_WORKERS_JSON` is easy to fix — none of them ever include
/// secret material (this config carries none — see the section doc above).
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum WorkerTransportConfigError {
    #[error("TERMINUS_BROKER_WORKERS_JSON is not valid JSON: {0}")]
    InvalidJson(String),
    #[error("worker entry at index {index} has an empty \"name\"")]
    EmptyName { index: usize },
    #[error("duplicate worker \"name\": \"{name}\"")]
    DuplicateName { name: String },
    #[error(
        "worker \"{name}\" is declared at tier {tier} but its capability class \"{class}\" requires at least {minimum}"
    )]
    BelowMinimumTier {
        name: String,
        tier: crate::broker::transport::TransportTier,
        class: String,
        minimum: crate::broker::transport::TransportTier,
    },
    #[error("worker \"{name}\" (tier {tier}) is missing its required \"socket_path\"")]
    MissingSocketPath { name: String, tier: crate::broker::transport::TransportTier },
    #[error("worker \"{name}\" (tier {tier}) is missing its required \"expected_uid\"")]
    MissingExpectedUid { name: String, tier: crate::broker::transport::TransportTier },
    #[error("worker \"{name}\" (tier {tier}) is missing its required \"expected_identity\"")]
    MissingExpectedIdentity { name: String, tier: crate::broker::transport::TransportTier },
    #[error("worker \"{name}\" (tier {tier}) is missing its required \"host\"/\"port\"")]
    MissingHostPort { name: String, tier: crate::broker::transport::TransportTier },
}

fn capability_class_label(class: crate::broker::transport::CapabilityClass) -> &'static str {
    match class {
        crate::broker::transport::CapabilityClass::ReadOnly => "read_only",
        crate::broker::transport::CapabilityClass::WriteScoped => "write_scoped",
        crate::broker::transport::CapabilityClass::SecretHolding => "secret_holding",
    }
}

/// The validated set of worker-transport entries. Construct via
/// [`WorkerTransportRegistry::from_env`] in production, or
/// [`WorkerTransportRegistry::from_json`] directly in tests. There is no
/// public constructor that skips validation.
#[derive(Debug, Clone, Default)]
pub struct WorkerTransportRegistry {
    workers: Vec<WorkerTransportEntry>,
}

impl WorkerTransportRegistry {
    /// An empty, dormant registry — the default when no broker workers are
    /// configured. Never an error: a dormant feature is not a
    /// misconfiguration.
    pub fn empty() -> Self {
        Self { workers: Vec::new() }
    }

    /// Build the registry from `TERMINUS_BROKER_WORKERS_JSON` (non-secret
    /// structural JSON, read via plain `std::env::var` — nothing in this
    /// shape is a credential). Unset/blank ⇒ `Ok(Self::empty())`, never an
    /// error.
    pub fn from_env() -> Result<Self, WorkerTransportConfigError> {
        match env_nonempty("TERMINUS_BROKER_WORKERS_JSON") {
            Some(raw) => Self::from_json(&raw),
            None => Ok(Self::empty()),
        }
    }

    /// Parse + validate a registry from a raw JSON array string.
    pub fn from_json(json: &str) -> Result<Self, WorkerTransportConfigError> {
        let workers: Vec<WorkerTransportEntry> = serde_json::from_str(json)
            .map_err(|e| WorkerTransportConfigError::InvalidJson(e.to_string()))?;
        validate_worker_transports(&workers)?;
        Ok(Self { workers })
    }

    pub fn all(&self) -> &[WorkerTransportEntry] {
        &self.workers
    }

    pub fn by_name(&self, name: &str) -> Option<&WorkerTransportEntry> {
        self.workers.iter().find(|w| w.name == name)
    }

    pub fn len(&self) -> usize {
        self.workers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.workers.is_empty()
    }
}

/// Validate a parsed entry list: unique `name`, every entry's `tier` at or
/// above its `capability_class`'s [`crate::broker::transport::MinTierPolicy`]
/// floor (**the minimum-tier floor enforcement point**), and every
/// tier-required field present. Stops at the first violation.
fn validate_worker_transports(
    workers: &[WorkerTransportEntry],
) -> Result<(), WorkerTransportConfigError> {
    use crate::broker::transport::{MinTierPolicy, TransportTier};
    use std::collections::HashSet;

    let mut seen_names: HashSet<String> = HashSet::new();

    for (index, w) in workers.iter().enumerate() {
        if w.name.trim().is_empty() {
            return Err(WorkerTransportConfigError::EmptyName { index });
        }
        if !seen_names.insert(w.name.clone()) {
            return Err(WorkerTransportConfigError::DuplicateName { name: w.name.clone() });
        }

        if !MinTierPolicy::permits(w.capability_class, w.tier) {
            return Err(WorkerTransportConfigError::BelowMinimumTier {
                name: w.name.clone(),
                tier: w.tier,
                class: capability_class_label(w.capability_class).to_string(),
                minimum: MinTierPolicy::minimum_tier(w.capability_class),
            });
        }

        match w.tier {
            TransportTier::T1 => {
                if w.socket_path.as_deref().unwrap_or("").trim().is_empty() {
                    return Err(WorkerTransportConfigError::MissingSocketPath {
                        name: w.name.clone(),
                        tier: w.tier,
                    });
                }
                if w.expected_uid.is_none() {
                    return Err(WorkerTransportConfigError::MissingExpectedUid {
                        name: w.name.clone(),
                        tier: w.tier,
                    });
                }
            }
            TransportTier::T2 => {
                if w.socket_path.as_deref().unwrap_or("").trim().is_empty() {
                    return Err(WorkerTransportConfigError::MissingSocketPath {
                        name: w.name.clone(),
                        tier: w.tier,
                    });
                }
                if w.expected_uid.is_none() {
                    return Err(WorkerTransportConfigError::MissingExpectedUid {
                        name: w.name.clone(),
                        tier: w.tier,
                    });
                }
                if w.expected_identity.as_deref().unwrap_or("").trim().is_empty() {
                    return Err(WorkerTransportConfigError::MissingExpectedIdentity {
                        name: w.name.clone(),
                        tier: w.tier,
                    });
                }
            }
            TransportTier::T0 => {
                if w.host.as_deref().unwrap_or("").trim().is_empty() || w.port.is_none() {
                    return Err(WorkerTransportConfigError::MissingHostPort {
                        name: w.name.clone(),
                        tier: w.tier,
                    });
                }
                if w.expected_identity.as_deref().unwrap_or("").trim().is_empty() {
                    return Err(WorkerTransportConfigError::MissingExpectedIdentity {
                        name: w.name.clone(),
                        tier: w.tier,
                    });
                }
            }
        }
    }
    Ok(())
}

/// TMOD-05: validate a single, already-deserialized [`WorkerTransportEntry`]
/// against the exact same rule set [`WorkerTransportRegistry::from_json`]
/// enforces on a whole batch (non-empty name, the
/// [`crate::broker::transport::MinTierPolicy`] floor for `capability_class`
/// vs `tier` -- **the minimum-tier floor enforcement point** for a live
/// registration, not just a config-file load -- and every tier-required
/// field present). Reused by the broker admin control plane
/// (`crate::broker::control`) so an incoming `POST /admin/workers/register`
/// manifest is checked against the identical logic
/// `TERMINUS_BROKER_WORKERS_JSON` startup config already uses, rather than a
/// second, drifting copy of the same rules.
///
/// Deliberately does NOT check for a duplicate name against any
/// already-registered worker -- this function validates one entry in
/// isolation; "is this name already in use" is a route-table-level
/// (`crate::broker::routes::RouteTable`) concern the caller checks
/// separately, since a `WorkerTransportEntry` alone has no view of what's
/// currently registered.
pub fn validate_worker_transport_entry(
    entry: &WorkerTransportEntry,
) -> Result<(), WorkerTransportConfigError> {
    validate_worker_transports(std::slice::from_ref(entry))
}

// ── CONST-02: constellation aggregation-layer config ───────────────────────
//
// The Terminus aggregation layer (`crate::constellation`) is a compiled-in
// module of the primary/gateway binary (see `docs/architecture/broker.md` —
// this is deliberately NOT a broker worker: it is an operator-facing HTTP
// API + static-asset host, not an MCP tool domain). Every backend base URL
// it proxies to, and every filesystem path it needs, is resolved here —
// NEVER a literal in `crate::constellation::*` — matching this file's own
// convention for every other tool in this crate. Following this crate's
// established secret/config convention (see the `crate::<secret-manager>` and
// `crate::review::dispatch` module docs: there is no separate
// `SecretManager`/`vault::manager()` API in terminus-rs — a plain env read
// of a runtime-materialized value, via [`env_nonempty`], IS the vault read
// here), these are plain env-sourced config helpers, not secret-shaped
// values in their own right (a backend base URL is infra config, not a
// credential) — no auth token is read or forwarded by this section at all.

/// Base URL of the Harmony backend the aggregation layer proxies
/// `/api/harmony/*path` to. From `CONSTELLATION_HARMONY_URL`; `None` ⇒ the
/// proxy handler reports that system as `available:false` rather than
/// guessing an infra host.
pub fn constellation_harmony_url() -> Option<String> {
    env_nonempty("CONSTELLATION_HARMONY_URL")
}

/// Base URL of the Chord backend the aggregation layer proxies
/// `/api/chord/*path` to. From `CONSTELLATION_CHORD_URL`; `None` ⇒
/// `available:false` for that system.
pub fn constellation_chord_url() -> Option<String> {
    env_nonempty("CONSTELLATION_CHORD_URL")
}

/// Base URL of the Lumina backend the aggregation layer proxies
/// `/api/lumina/*path` to. From `CONSTELLATION_LUMINA_URL`; `None` ⇒
/// `available:false` for that system.
pub fn constellation_lumina_url() -> Option<String> {
    env_nonempty("CONSTELLATION_LUMINA_URL")
}

/// Base URL of the Muse backend the aggregation layer proxies
/// `/api/muse/*path` to (the fourth namespaced proxy arm, CONST-19). From
/// `CONSTELLATION_MUSE_URL`; `None` ⇒ `available:false` for that system,
/// same convention as the other three backend URLs above.
pub fn constellation_muse_url() -> Option<String> {
    env_nonempty("CONSTELLATION_MUSE_URL")
}

/// Base URL of Harmony's own event WebSocket the `/ws` relay
/// (`crate::constellation::ws`, CONST-18) dials as its upstream leg. From
/// `CONSTELLATION_HARMONY_WS_URL`; `None` ⇒ the relay accepts the
/// browser's upgrade (session-cookie-verified first) and immediately sends
/// a typed close frame rather than dialing a guessed host — the client
/// stays on 30s polling in that case (§3.5). Same "infra config, not a
/// credential" posture as the sibling `constellation_{harmony,chord,
/// lumina}_url` helpers above — a bare `ws://`/`wss://` URL, no auth token
/// embedded in it.
pub fn constellation_harmony_ws_url() -> Option<String> {
    env_nonempty("CONSTELLATION_HARMONY_WS_URL")
}

/// Filesystem directory holding the built `constellation-web` static
/// assets (its `dist/` output) that this layer serves as a SPA, with
/// `index.html` as the not-found fallback. From
/// `CONSTELLATION_WEB_DIST_DIR`; `None` ⇒ no static-asset host is mounted
/// at all (an API-only deployment — e.g. a dev box that never built the
/// web bundle), rather than guessing a path.
pub fn constellation_web_dist_dir() -> Option<String> {
    env_nonempty("CONSTELLATION_WEB_DIST_DIR")
}

/// Per-request timeout, in milliseconds, for a proxied `/api/{system}/*`
/// backend call. From `CONSTELLATION_BACKEND_TIMEOUT_MS`; defaults to 5000
/// (5s) — long enough for a healthy co-located backend, bounded so a
/// wedged/unreachable one degrades a single system's panel instead of
/// hanging the whole aggregation request.
pub fn constellation_backend_timeout_ms() -> u64 {
    env_nonempty("CONSTELLATION_BACKEND_TIMEOUT_MS")
        .and_then(|v| v.parse().ok())
        .filter(|ms: &u64| *ms > 0)
        .unwrap_or(5_000)
}

/// Filesystem path the aggregation layer's mutating-request audit log
/// (S6-sanitized JSONL, one line per POST/PUT/PATCH/DELETE through
/// `/api/*`) is appended to. From `CONSTELLATION_AUDIT_LOG_PATH`; defaults
/// to a relative file in the process's working directory (low-stakes local
/// operational log, same "sensible default over hard `NotConfigured`"
/// posture as [`meridian_state_path`] above, not shared infra).
pub fn constellation_audit_log_path() -> String {
    env_nonempty("CONSTELLATION_AUDIT_LOG_PATH")
        .unwrap_or_else(|| "constellation-audit.jsonl".to_string())
}

/// CONST-26: max number of entries `GET /api/terminus/activity` will ever
/// tail-read from the constellation audit JSONL ([`constellation_audit_log_path`])
/// and return in one response, regardless of a caller-supplied `?limit=`
/// query value (a caller may ask for FEWER, never more — see
/// `crate::constellation::activity`'s module doc). From
/// `CONSTELLATION_ACTIVITY_TAIL_LIMIT`; defaults to 200 — enough for a
/// useful operator-facing feed without ever reading the whole (potentially
/// large, long-lived) audit log into memory.
pub fn constellation_activity_tail_limit() -> usize {
    env_nonempty("CONSTELLATION_ACTIVITY_TAIL_LIMIT")
        .and_then(|v| v.parse().ok())
        .filter(|n: &usize| *n > 0)
        .unwrap_or(200)
}

// ── CONST-03: constellation control-plane auth ─────────────────────────────
//
// The session token itself is signed with `TERMINUS_JWT_SIGNING_KEY` (the
// SAME signing key `crate::pki::enroll`'s TCLI-02 enrollment JWT uses —
// `crate::pki::enroll::mint_jwt_with_ttl`/`verify_jwt`) and the operator
// credential compared at login is `CONSTELLATION_OPERATOR_SECRET` — both are
// secret-shaped, so per this crate's convention (see this file's "CONST-02"
// section doc above, and `crate::pki`'s module doc: no separate
// `SecretManager`/`vault::manager()` API here) they are read directly via
// `std::env::var`/`env_nonempty` at the point of use in
// `crate::constellation::auth` and `crate::pki::enroll`, never resolved by a
// `crate::config` helper — this file only resolves the NON-secret knobs
// below (TTL, a boolean flag).

/// Operator shared secret compared (constant-time) against the submitted
/// login password. From `CONSTELLATION_OPERATOR_SECRET`. `None` when unset
/// — callers MUST fail-closed (reject every login attempt) in that case,
/// never fall back to a default-allow. Deliberately returns the secret
/// value itself (unlike every other helper in this section) because this
/// crate has no separate secret-store API to route it through instead — see
/// this section's doc comment; `crate::constellation::auth` is the only
/// caller and never logs the returned value.
pub fn constellation_operator_secret() -> Option<String> {
    env_nonempty("CONSTELLATION_OPERATOR_SECRET")
}

/// Viewer shared secret (CONST-27, §3.4) compared (constant-time) against the
/// submitted login password AFTER the operator secret has already been
/// checked and didn't match. From `CONSTELLATION_VIEWER_SECRET`
/// (operator-provisioned in <secret-manager> — never hardcoded). `None` when unset
/// — callers MUST fail-closed (every viewer-tier login attempt rejected,
/// same posture as an unset `CONSTELLATION_OPERATOR_SECRET`): an operator who
/// hasn't provisioned this secret simply hasn't enabled the viewer tier yet,
/// never a default-allow. Same "no separate secret-store API in this crate"
/// rationale as [`constellation_operator_secret`] above; the only caller is
/// `crate::constellation::auth::auth_login`, which never logs the returned
/// value.
pub fn constellation_viewer_secret() -> Option<String> {
    env_nonempty("CONSTELLATION_VIEWER_SECRET")
}

/// LGUI-05 (LUMINA-GUI-SPEC.md §6 decision D2): the bearer credential
/// `crate::constellation::proxy::proxy_lumina` attaches as
/// `Authorization: Bearer <token>` on every proxied `/api/lumina/*path`
/// request to the Lumina backend, so the browser never holds -- and can
/// never supply -- a Lumina credential; the operator's Constellation session
/// cookie only ever authenticates the *browser* to Terminus, this token is
/// what separately authenticates *Terminus to Lumina* server-side. From
/// `CONSTELLATION_LUMINA_TOKEN` (<secret-manager>-provisioned; same value as
/// Lumina's own `LUMINA_HTTP_TOKEN` -- two consumers, one secret, per the
/// spec's Pre-flight section). `None` when unset -- `proxy_lumina` forwards
/// unauthenticated in that case, exactly as it did before this item (a
/// token-less dev Lumina instance keeps working), never a hard failure.
/// Same "no separate secret-store API in this crate" rationale as
/// [`constellation_operator_secret`] above; the only caller is
/// `crate::constellation::proxy::proxy_lumina`, which never logs the
/// returned value.
pub fn constellation_lumina_token() -> Option<String> {
    env_nonempty("CONSTELLATION_LUMINA_TOKEN")
}

/// Constellation session token TTL, in seconds. Independent of
/// `TERMINUS_ENROLLMENT_JWT_TTL_SECONDS` (a different credential with a
/// different lifecycle: an operator's browser session vs. a paired
/// cert+JWT enrollment) — deliberately its own knob rather than reusing the
/// enrollment TTL. From `CONSTELLATION_SESSION_TTL_SECONDS`; defaults to
/// 3600s (1 hour), long enough for a normal operator session without being
/// a durable credential.
pub fn constellation_session_ttl_seconds() -> i64 {
    env_nonempty("CONSTELLATION_SESSION_TTL_SECONDS")
        .and_then(|v| v.parse().ok())
        .filter(|s: &i64| *s > 0)
        .unwrap_or(3600)
}

/// Whether the session cookie should carry the `Secure` attribute (only
/// sent over HTTPS). From `CONSTELLATION_COOKIE_SECURE` (`"true"`/`"1"` ⇒
/// true); defaults to `false` — a LAN-served dev/operator UI may run over
/// plain HTTP, matching this layer's existing default posture (see
/// `crate::constellation::auth`'s cookie-header doc). An operator serving
/// this behind TLS should set this to `true`.
pub fn constellation_cookie_secure() -> bool {
    env_nonempty("CONSTELLATION_COOKIE_SECURE")
        .map(|v| matches!(v.to_ascii_lowercase().as_str(), "true" | "1"))
        .unwrap_or(false)
}

// ── GMQ-02: Gitea merge-queue config (per-base merge lock + FIFO ordering) ──
//
// `crate::gitea::merge_queue::MergeQueue` serializes merges to the same base
// branch (see `docs/specs/S120-gitea-merge-queue.md`). It degrades open when
// Redis is absent/unreachable, so these knobs only matter once Redis is
// configured; all three are optional with safe defaults.

/// Whether the merge-queue path is enabled. From `GITEA_MERGE_QUEUE_ENABLED`
/// (`"true"`/`"1"` ⇒ true, anything else ⇒ false); defaults to `true` — once
/// Redis is present the queue should protect concurrent merges by default,
/// but an operator can flip this off (e.g. to rule it out while debugging)
/// without unsetting `REDIS_URL` for every other Redis-backed feature.
pub fn gitea_merge_queue_enabled() -> bool {
    env_nonempty("GITEA_MERGE_QUEUE_ENABLED")
        .map(|v| matches!(v.to_ascii_lowercase().as_str(), "true" | "1"))
        .unwrap_or(true)
}

/// Merge-lock TTL (seconds): the crash backstop that frees a holder's lock if
/// it never releases (a crashed worker, a hung Gitea request). From
/// `GITEA_MERGE_QUEUE_LOCK_TTL_SECS`; defaults to 120s. Must exceed a
/// realistic merge time — a merge that runs longer than this can have its
/// lock reclaimed by the next waiter while still in flight.
pub fn gitea_merge_queue_lock_ttl_secs() -> u64 {
    env_nonempty("GITEA_MERGE_QUEUE_LOCK_TTL_SECS")
        .and_then(|v| v.parse().ok())
        .filter(|n: &u64| *n > 0)
        .unwrap_or(120)
}

/// Max time (seconds) a waiter polls for its turn before giving up with a
/// clear "queue busy, retry" instead of hanging indefinitely. From
/// `GITEA_MERGE_QUEUE_MAX_WAIT_SECS`; defaults to 300s.
pub fn gitea_merge_queue_max_wait_secs() -> u64 {
    env_nonempty("GITEA_MERGE_QUEUE_MAX_WAIT_SECS")
        .and_then(|v| v.parse().ok())
        .filter(|n: &u64| *n > 0)
        .unwrap_or(300)
}

/// Margin (seconds) added above `max_wait_secs` when clamping an
/// operator-configured `wait_ttl_secs` that would otherwise be too short (see
/// [`gitea_merge_queue_wait_ttl_secs`]).
const WAIT_TTL_MIN_MARGIN_SECS: u64 = 60;

/// TTL (seconds) for the wait ZSET's own `EXPIRE` backstop (`queue:merge:wait:{key}`,
/// see `crate::gitea::merge_queue::RedisMergeLockStore::enqueue`) — bounds how long an
/// abandoned waiter (e.g. a caller that crashed between enqueue and its first poll) can
/// wedge a key. From `GITEA_MERGE_QUEUE_WAIT_TTL_SECS`; defaults to
/// `gitea_merge_queue_max_wait_secs() + 60`, i.e. derived from the max-wait ceiling so it
/// always outlives the longest a real waiter can legitimately be polling, rather than a
/// bare hardcoded literal that could end up shorter than `max_wait_secs`.
///
/// **Invariant, enforced here (not just documented):** the effective value can
/// never be lower than `max_wait_secs + WAIT_TTL_MIN_MARGIN_SECS`, even when an
/// operator explicitly configures a smaller `GITEA_MERGE_QUEUE_WAIT_TTL_SECS` —
/// a shorter wait TTL would expire the wait ordering out from under a
/// still-legitimately-polling waiter. A too-small configured value is clamped
/// up (not rejected) so a misconfigured `.env` degrades to "safe but maybe
/// slower to self-heal an abandoned ticket" rather than wedging the queue.
pub fn gitea_merge_queue_wait_ttl_secs() -> u64 {
    let max_wait = gitea_merge_queue_max_wait_secs();
    let floor = max_wait.saturating_add(WAIT_TTL_MIN_MARGIN_SECS);
    let configured = env_nonempty("GITEA_MERGE_QUEUE_WAIT_TTL_SECS")
        .and_then(|v| v.parse().ok())
        .filter(|n: &u64| *n > 0);
    match configured {
        Some(v) => v.max(floor),
        None => floor,
    }
}

// ── GMQ-03: Gitea merge-queue min-delay spacing ─────────────────────────────
//
// `crate::gitea::merge_queue::MergeQueue::enforce_spacing` enforces a minimum
// gap between successive merges to the same base branch (see
// `docs/specs/S120-gitea-merge-queue.md`, GMQ-03) — lets `main` settle /
// mirror+CI react before the next merge lands. `0` disables spacing entirely
// (no artificial delay), matching the item's stated contract.

/// Minimum seconds required between two merges to the same base key. From
/// `GITEA_MERGE_QUEUE_MIN_DELAY_SECS`; defaults to 8. `0` means no spacing
/// delay is enforced.
pub fn gitea_merge_queue_min_delay_secs() -> u64 {
    env_nonempty("GITEA_MERGE_QUEUE_MIN_DELAY_SECS")
        .and_then(|v| v.parse().ok())
        .unwrap_or(8)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    // ── S115/PGT-01: pg connection secret naming ────────────────────────

    #[test]
    fn pg_connection_secret_name_builds_expected_key() {
        assert_eq!(pg_connection_secret_name("readonly"), "POSTGRES_URL_READONLY");
        assert_eq!(pg_connection_secret_name("writer"), "POSTGRES_URL_WRITER");
        assert_eq!(pg_connection_secret_name("admin"), "POSTGRES_URL_ADMIN");
    }

    #[test]
    fn pg_connection_secret_name_normalizes_case_and_whitespace() {
        assert_eq!(pg_connection_secret_name("  Readonly  "), "POSTGRES_URL_READONLY");
        assert_eq!(pg_connection_secret_name("CamelCaseName"), "POSTGRES_URL_CAMELCASENAME");
    }

    #[test]
    fn pg_connection_secret_name_never_reads_the_environment() {
        // This function only builds a key NAME; it must not itself resolve a
        // secret VALUE. Setting a matching env var must not change its output.
        std::env::set_var("POSTGRES_URL_PROBE", "postgres://should-not-be-read@example/db");
        assert_eq!(pg_connection_secret_name("probe"), "POSTGRES_URL_PROBE");
        std::env::remove_var("POSTGRES_URL_PROBE");
    }

    // ── KGEMB-02: embeddings config ─────────────────────────────────────

    #[test]
    #[serial]
    fn embeddings_url_defaults_from_chord_personal_federation_url() {
        // EMBED-02: default now follows the co-located Chord loopback proxy
        // (TERMINUS_PRIMARY_CHORD_URL / chord_personal_federation_url), not a
        // raw Ollama endpoint.
        std::env::remove_var("EMBEDDINGS_URL");
        std::env::remove_var("TERMINUS_PRIMARY_CHORD_URL");
        assert_eq!(embeddings_url(), "http://127.0.0.1:8099/v1/embeddings"); // pii-test-fixture
        std::env::set_var("TERMINUS_PRIMARY_CHORD_URL", "http://127.0.0.1:9199"); // pii-test-fixture
        assert_eq!(embeddings_url(), "http://127.0.0.1:9199/v1/embeddings"); // pii-test-fixture
        std::env::remove_var("TERMINUS_PRIMARY_CHORD_URL");
    }

    #[test]
    #[serial]
    fn embeddings_url_explicit_override_wins() {
        std::env::set_var("EMBEDDINGS_URL", "http://127.0.0.1:9/v1/embeddings"); // pii-test-fixture
        assert_eq!(embeddings_url(), "http://127.0.0.1:9/v1/embeddings"); // pii-test-fixture
        std::env::remove_var("EMBEDDINGS_URL");
    }

    #[test]
    #[serial]
    fn embeddings_model_defaults_and_overrides() {
        std::env::remove_var("EMBEDDINGS_MODEL");
        assert_eq!(embeddings_model(), "Qwen3-Embedding");
        std::env::set_var("EMBEDDINGS_MODEL", "custom-embed");
        assert_eq!(embeddings_model(), "custom-embed");
        std::env::remove_var("EMBEDDINGS_MODEL");
    }

    #[test]
    #[serial]
    fn embeddings_timeout_ms_defaults_and_rejects_nonpositive() {
        std::env::remove_var("EMBEDDINGS_TIMEOUT_MS");
        assert_eq!(embeddings_timeout_ms(), 30_000);
        std::env::set_var("EMBEDDINGS_TIMEOUT_MS", "0");
        assert_eq!(embeddings_timeout_ms(), 30_000);
        std::env::set_var("EMBEDDINGS_TIMEOUT_MS", "5000");
        assert_eq!(embeddings_timeout_ms(), 5_000);
        std::env::remove_var("EMBEDDINGS_TIMEOUT_MS");
    }

    // ── TGW-02: Chord federation config ─────────────────────────────────

    #[test]
    #[serial]
    fn chord_personal_federation_url_defaults_and_overrides() {
        std::env::remove_var("TERMINUS_PRIMARY_CHORD_URL");
        assert_eq!(chord_personal_federation_url(), "http://127.0.0.1:8099"); // pii-test-fixture
        std::env::set_var("TERMINUS_PRIMARY_CHORD_URL", "http://127.0.0.1:9999"); // pii-test-fixture
        assert_eq!(chord_personal_federation_url(), "http://127.0.0.1:9999"); // pii-test-fixture
        std::env::remove_var("TERMINUS_PRIMARY_CHORD_URL");
    }

    // ── DOCGEN-05: doc-generation routing tag ───────────────────────────

    #[test]
    #[serial]
    fn docgen_chord_model_defaults_to_auto_and_honors_override() {
        std::env::remove_var("DOCGEN_CHORD_MODEL");
        assert_eq!(docgen_chord_model(), "auto");
        std::env::set_var("DOCGEN_CHORD_MODEL", "docs-slm");
        assert_eq!(docgen_chord_model(), "docs-slm");
        std::env::remove_var("DOCGEN_CHORD_MODEL");
    }

    // ── DGDG-01: cloud-provider fallback model ──────────────────────────

    #[test]
    #[serial]
    fn docgen_cloud_fallback_model_disabled_when_unset() {
        std::env::remove_var("DOCGEN_CLOUD_FALLBACK_MODEL");
        assert_eq!(docgen_cloud_fallback_model(), None);
        std::env::set_var("DOCGEN_CLOUD_FALLBACK_MODEL", "  ");
        assert_eq!(docgen_cloud_fallback_model(), None, "whitespace-only must also disable");
        std::env::remove_var("DOCGEN_CLOUD_FALLBACK_MODEL");
    }

    #[test]
    #[serial]
    fn docgen_cloud_fallback_model_honors_override() {
        std::env::set_var("DOCGEN_CLOUD_FALLBACK_MODEL", "qwen/qwen3-coder:free");
        assert_eq!(docgen_cloud_fallback_model().as_deref(), Some("qwen/qwen3-coder:free"));
        std::env::remove_var("DOCGEN_CLOUD_FALLBACK_MODEL");
    }

    #[test]
    #[serial]
    fn chord_personal_federation_timeout_ms_defaults_and_overrides() {
        std::env::remove_var("TERMINUS_PRIMARY_CHORD_FEDERATION_TIMEOUT_MS");
        assert_eq!(chord_personal_federation_timeout_ms(), 30_000);
        std::env::set_var("TERMINUS_PRIMARY_CHORD_FEDERATION_TIMEOUT_MS", "5000");
        assert_eq!(chord_personal_federation_timeout_ms(), 5_000);
        // Non-positive values fall back to the default rather than producing
        // a zero-duration (instant-timeout) client.
        std::env::set_var("TERMINUS_PRIMARY_CHORD_FEDERATION_TIMEOUT_MS", "0");
        assert_eq!(chord_personal_federation_timeout_ms(), 30_000);
        std::env::remove_var("TERMINUS_PRIMARY_CHORD_FEDERATION_TIMEOUT_MS");
    }

    #[test]
    #[serial]
    fn docgen_local_timeout_ms_defaults_and_overrides() {
        std::env::remove_var("DOCGEN_LOCAL_TIMEOUT_MS");
        assert_eq!(docgen_local_timeout_ms(), 45_000);
        std::env::set_var("DOCGEN_LOCAL_TIMEOUT_MS", "20000");
        assert_eq!(docgen_local_timeout_ms(), 20_000);
        // Non-positive / unparseable -> default (never a zero-duration primary).
        std::env::set_var("DOCGEN_LOCAL_TIMEOUT_MS", "0");
        assert_eq!(docgen_local_timeout_ms(), 45_000);
        std::env::set_var("DOCGEN_LOCAL_TIMEOUT_MS", "nonsense");
        assert_eq!(docgen_local_timeout_ms(), 45_000);
        std::env::remove_var("DOCGEN_LOCAL_TIMEOUT_MS");
    }

    // ── TGW-03: inference-proxy config ──────────────────────────────────

    #[test]
    #[serial]
    fn chord_inference_connect_timeout_ms_defaults_and_overrides() {
        std::env::remove_var("TERMINUS_PRIMARY_CHORD_INFERENCE_CONNECT_TIMEOUT_MS");
        assert_eq!(chord_inference_connect_timeout_ms(), 5_000);
        std::env::set_var("TERMINUS_PRIMARY_CHORD_INFERENCE_CONNECT_TIMEOUT_MS", "1500");
        assert_eq!(chord_inference_connect_timeout_ms(), 1_500);
        // Non-positive values fall back to the default rather than an
        // instant-timeout connect.
        std::env::set_var("TERMINUS_PRIMARY_CHORD_INFERENCE_CONNECT_TIMEOUT_MS", "0");
        assert_eq!(chord_inference_connect_timeout_ms(), 5_000);
        std::env::remove_var("TERMINUS_PRIMARY_CHORD_INFERENCE_CONNECT_TIMEOUT_MS");
    }

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
    fn breakfix_ollama_fallback_matches_sibling_accessor() {
        std::env::remove_var("OLLAMA_CPU_URL");
        std::env::remove_var("MINT_BREAKFIX_FALLBACK_MODEL");
        // Both accessors now agree: None on unset, no compiled-in fallback.
        assert_eq!(ollama_secondary_url(), None);
        assert_eq!(breakfix_ollama_cpu_url(), None);
        assert_eq!(breakfix_fallback_model(), "qwen2.5:7b");
        std::env::set_var("OLLAMA_CPU_URL", "http://198.51.100.5:11435"); // pii-test-fixture
        std::env::set_var("MINT_BREAKFIX_FALLBACK_MODEL", "phi3:mini");
        assert_eq!(breakfix_ollama_cpu_url(), Some("http://198.51.100.5:11435".to_string())); // pii-test-fixture
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

    #[test]
    #[serial]
    fn ca_store_path_defaults_and_overrides() {
        std::env::remove_var("TERMINUS_CA_STORE_PATH");
        let default_path = ca_store_path();
        assert!(default_path.ends_with(".terminus/pki/ca_store.json"));

        std::env::set_var("TERMINUS_CA_STORE_PATH", "/tmp/example-ca-store.json");
        assert_eq!(ca_store_path(), "/tmp/example-ca-store.json");
        std::env::remove_var("TERMINUS_CA_STORE_PATH");
    }

    // ── TGW-01: terminus-primary mTLS config defaults + overrides, and that
    //    they never collide with terminus_personal's own TERMINUS_MTLS_*
    //    family (the whole point of a separate var family). ─────────────────

    #[test]
    #[serial]
    fn mtls_primary_bind_addr_defaults_and_overrides() {
        std::env::remove_var("TERMINUS_PRIMARY_MTLS_BIND");
        assert_eq!(mtls_primary_bind_addr(), "127.0.0.1");
        std::env::set_var("TERMINUS_PRIMARY_MTLS_BIND", "0.0.0.0");
        assert_eq!(mtls_primary_bind_addr(), "0.0.0.0");
        std::env::remove_var("TERMINUS_PRIMARY_MTLS_BIND");
    }

    #[test]
    #[serial]
    fn mtls_primary_port_defaults_and_overrides() {
        std::env::remove_var("TERMINUS_PRIMARY_MTLS_PORT");
        assert_eq!(mtls_primary_port(), 8311);
        std::env::set_var("TERMINUS_PRIMARY_MTLS_PORT", "9911");
        assert_eq!(mtls_primary_port(), 9911);
        std::env::remove_var("TERMINUS_PRIMARY_MTLS_PORT");
    }

    #[test]
    #[serial]
    fn mtls_primary_server_identity_defaults_and_overrides() {
        std::env::remove_var("TERMINUS_PRIMARY_MTLS_SERVER_IDENTITY");
        assert_eq!(mtls_primary_server_identity(), "terminus-primary");
        std::env::set_var("TERMINUS_PRIMARY_MTLS_SERVER_IDENTITY", "custom-primary");
        assert_eq!(mtls_primary_server_identity(), "custom-primary");
        std::env::remove_var("TERMINUS_PRIMARY_MTLS_SERVER_IDENTITY");
    }

    #[test]
    #[serial]
    fn mtls_primary_defaults_never_collide_with_terminus_personal_mtls_defaults() {
        std::env::remove_var("TERMINUS_PRIMARY_MTLS_PORT");
        std::env::remove_var("TERMINUS_MTLS_PORT");
        assert_ne!(
            mtls_primary_port(),
            mtls_port(),
            "terminus-primary and terminus_personal must default to different mTLS ports \
             so both can run alongside each other on the same host"
        );
    }

    // ── DISC-04 ─────────────────────────────────────────────────────────────

    #[test]
    #[serial]
    fn hf_api_base_url_defaults_and_overrides() {
        std::env::remove_var("HF_API_BASE_URL");
        assert_eq!(hf_api_base_url(), "https://huggingface.co");
        std::env::set_var("HF_API_BASE_URL", "http://mock-hf.example");
        assert_eq!(hf_api_base_url(), "http://mock-hf.example");
        std::env::remove_var("HF_API_BASE_URL");
    }

    #[test]
    #[serial]
    fn hf_discovery_rate_limit_per_min_defaults_and_overrides() {
        std::env::remove_var("HF_DISCOVERY_RATE_LIMIT_PER_MIN");
        assert_eq!(hf_discovery_rate_limit_per_min(), 30);
        std::env::set_var("HF_DISCOVERY_RATE_LIMIT_PER_MIN", "60");
        assert_eq!(hf_discovery_rate_limit_per_min(), 60);
        std::env::remove_var("HF_DISCOVERY_RATE_LIMIT_PER_MIN");
    }

    #[test]
    #[serial]
    fn hf_discovery_rate_limit_per_min_ignores_non_positive_override() {
        std::env::set_var("HF_DISCOVERY_RATE_LIMIT_PER_MIN", "0");
        assert_eq!(hf_discovery_rate_limit_per_min(), 30);
        std::env::set_var("HF_DISCOVERY_RATE_LIMIT_PER_MIN", "not-a-number");
        assert_eq!(hf_discovery_rate_limit_per_min(), 30);
        std::env::remove_var("HF_DISCOVERY_RATE_LIMIT_PER_MIN");
    }

    // ── TMOD-02: WorkerTransportRegistry ────────────────────────────────

    const VALID_WORKER_JSON: &str = r#"[
        {
            "name": "read-worker",
            "tier": "T1",
            "capability_class": "read_only",
            "socket_path": "/run/terminus/read-worker.sock",
            "expected_uid": 1000
        },
        {
            "name": "write-worker",
            "tier": "T2",
            "capability_class": "write_scoped",
            "socket_path": "/run/terminus/write-worker.sock",
            "expected_uid": 1001,
            "expected_identity": "write-worker"
        },
        {
            "name": "offbox-worker",
            "tier": "T0",
            "capability_class": "read_only",
            "host": "worker.example.test",
            "port": 8443,
            "expected_identity": "offbox-worker"
        }
    ]"#;

    #[test]
    fn worker_transport_registry_parses_valid_json() {
        let reg = WorkerTransportRegistry::from_json(VALID_WORKER_JSON).expect("valid JSON should parse");
        assert_eq!(reg.len(), 3);
        assert_eq!(reg.by_name("read-worker").unwrap().tier, crate::broker::transport::TransportTier::T1);
        assert_eq!(reg.by_name("write-worker").unwrap().tier, crate::broker::transport::TransportTier::T2);
        assert_eq!(reg.by_name("offbox-worker").unwrap().tier, crate::broker::transport::TransportTier::T0);
    }

    #[test]
    fn worker_transport_registry_rejects_write_scoped_below_t2() {
        let json = r#"[{
            "name": "under-floored",
            "tier": "T1",
            "capability_class": "write_scoped",
            "socket_path": "/run/terminus/w.sock",
            "expected_uid": 1000
        }]"#;
        let err = WorkerTransportRegistry::from_json(json)
            .expect_err("a write_scoped worker declared at T1 must be rejected at config-load time");
        assert!(matches!(err, WorkerTransportConfigError::BelowMinimumTier { name, .. } if name == "under-floored"));
    }

    #[test]
    fn worker_transport_registry_rejects_secret_holding_below_t2() {
        let json = r#"[{
            "name": "under-floored-2",
            "tier": "T0",
            "capability_class": "secret_holding",
            "host": "worker.example.test",
            "port": 8443,
            "expected_identity": "under-floored-2"
        }]"#;
        let err = WorkerTransportRegistry::from_json(json)
            .expect_err("a secret_holding worker declared at T0 must be rejected -- T0 alone doesn't meet the T2 floor");
        assert!(matches!(err, WorkerTransportConfigError::BelowMinimumTier { name, .. } if name == "under-floored-2"));
    }

    #[test]
    fn worker_transport_registry_allows_read_only_at_t1() {
        let json = r#"[{
            "name": "reader",
            "tier": "T1",
            "capability_class": "read_only",
            "socket_path": "/run/terminus/reader.sock",
            "expected_uid": 1000
        }]"#;
        assert!(WorkerTransportRegistry::from_json(json).is_ok());
    }

    #[test]
    fn worker_transport_registry_rejects_duplicate_name() {
        let json = r#"[
            {"name": "dup", "tier": "T1", "capability_class": "read_only", "socket_path": "/a.sock", "expected_uid": 1},
            {"name": "dup", "tier": "T1", "capability_class": "read_only", "socket_path": "/b.sock", "expected_uid": 2}
        ]"#;
        let err = WorkerTransportRegistry::from_json(json).expect_err("duplicate name must be rejected");
        assert!(matches!(err, WorkerTransportConfigError::DuplicateName { name } if name == "dup"));
    }

    #[test]
    fn worker_transport_registry_rejects_t2_missing_expected_identity() {
        let json = r#"[{
            "name": "no-identity",
            "tier": "T2",
            "capability_class": "write_scoped",
            "socket_path": "/run/terminus/w.sock",
            "expected_uid": 1000
        }]"#;
        let err = WorkerTransportRegistry::from_json(json).expect_err("T2 worker missing expected_identity must be rejected");
        assert!(matches!(err, WorkerTransportConfigError::MissingExpectedIdentity { .. }));
    }

    #[test]
    fn worker_transport_registry_rejects_t0_missing_host_port() {
        let json = r#"[{
            "name": "no-host",
            "tier": "T0",
            "capability_class": "read_only",
            "expected_identity": "no-host"
        }]"#;
        let err = WorkerTransportRegistry::from_json(json).expect_err("T0 worker missing host/port must be rejected");
        assert!(matches!(err, WorkerTransportConfigError::MissingHostPort { .. }));
    }

    #[test]
    fn worker_transport_registry_malformed_json_is_a_clear_error_not_a_panic() {
        let err = WorkerTransportRegistry::from_json("not valid json {{{")
            .expect_err("malformed JSON must error, never panic");
        assert!(matches!(err, WorkerTransportConfigError::InvalidJson(_)));
    }

    #[test]
    #[serial]
    fn worker_transport_registry_from_env_is_empty_when_unset() {
        std::env::remove_var("TERMINUS_BROKER_WORKERS_JSON");
        let reg = WorkerTransportRegistry::from_env().expect("unset must never error");
        assert!(reg.is_empty());
    }

    #[test]
    #[serial]
    fn worker_transport_registry_from_env_parses_when_set() {
        std::env::set_var("TERMINUS_BROKER_WORKERS_JSON", VALID_WORKER_JSON);
        let reg = WorkerTransportRegistry::from_env().expect("should parse");
        assert_eq!(reg.len(), 3);
        std::env::remove_var("TERMINUS_BROKER_WORKERS_JSON");
    }

    // ── CONST-02: constellation aggregation-layer config ────────────────

    #[test]
    #[serial]
    fn constellation_backend_urls_none_when_unset() {
        std::env::remove_var("CONSTELLATION_HARMONY_URL");
        std::env::remove_var("CONSTELLATION_CHORD_URL");
        std::env::remove_var("CONSTELLATION_LUMINA_URL");
        std::env::remove_var("CONSTELLATION_MUSE_URL");
        assert_eq!(constellation_harmony_url(), None);
        assert_eq!(constellation_chord_url(), None);
        assert_eq!(constellation_lumina_url(), None);
        assert_eq!(constellation_muse_url(), None);
    }

    #[test]
    #[serial]
    fn constellation_backend_urls_read_when_set() {
        std::env::set_var("CONSTELLATION_HARMONY_URL", "http://127.0.0.1:9001"); // pii-test-fixture
        assert_eq!(
            constellation_harmony_url().as_deref(),
            Some("http://127.0.0.1:9001") // pii-test-fixture
        );
        std::env::remove_var("CONSTELLATION_HARMONY_URL");
    }

    #[test]
    #[serial]
    fn constellation_web_dist_dir_none_when_unset() {
        std::env::remove_var("CONSTELLATION_WEB_DIST_DIR");
        assert_eq!(constellation_web_dist_dir(), None);
    }

    #[test]
    #[serial]
    fn constellation_backend_timeout_ms_default_and_override() {
        std::env::remove_var("CONSTELLATION_BACKEND_TIMEOUT_MS");
        assert_eq!(constellation_backend_timeout_ms(), 5_000);
        std::env::set_var("CONSTELLATION_BACKEND_TIMEOUT_MS", "1500");
        assert_eq!(constellation_backend_timeout_ms(), 1_500);
        std::env::remove_var("CONSTELLATION_BACKEND_TIMEOUT_MS");
    }

    #[test]
    #[serial]
    fn constellation_activity_tail_limit_default_and_override() {
        std::env::remove_var("CONSTELLATION_ACTIVITY_TAIL_LIMIT");
        assert_eq!(constellation_activity_tail_limit(), 200);
        std::env::set_var("CONSTELLATION_ACTIVITY_TAIL_LIMIT", "50");
        assert_eq!(constellation_activity_tail_limit(), 50);
        // Zero/invalid falls back to the default rather than yielding a
        // degenerate always-empty feed.
        std::env::set_var("CONSTELLATION_ACTIVITY_TAIL_LIMIT", "0");
        assert_eq!(constellation_activity_tail_limit(), 200);
        std::env::set_var("CONSTELLATION_ACTIVITY_TAIL_LIMIT", "not-a-number");
        assert_eq!(constellation_activity_tail_limit(), 200);
        std::env::remove_var("CONSTELLATION_ACTIVITY_TAIL_LIMIT");
    }

    #[test]
    #[serial]
    fn constellation_audit_log_path_has_sane_default() {
        std::env::remove_var("CONSTELLATION_AUDIT_LOG_PATH");
        assert_eq!(constellation_audit_log_path(), "constellation-audit.jsonl");
    }

    #[test]
    #[serial]
    fn constellation_operator_secret_unset_is_none() {
        std::env::remove_var("CONSTELLATION_OPERATOR_SECRET");
        assert_eq!(constellation_operator_secret(), None);
        std::env::set_var("CONSTELLATION_OPERATOR_SECRET", "op-secret"); // pii-test-fixture
        assert_eq!(constellation_operator_secret(), Some("op-secret".to_string()));
        std::env::remove_var("CONSTELLATION_OPERATOR_SECRET");
    }

    #[test]
    #[serial]
    fn constellation_viewer_secret_unset_is_none() {
        std::env::remove_var("CONSTELLATION_VIEWER_SECRET");
        assert_eq!(constellation_viewer_secret(), None);
        std::env::set_var("CONSTELLATION_VIEWER_SECRET", "view-secret"); // pii-test-fixture
        assert_eq!(constellation_viewer_secret(), Some("view-secret".to_string()));
        std::env::remove_var("CONSTELLATION_VIEWER_SECRET");
    }

    #[test]
    #[serial]
    fn constellation_lumina_token_unset_is_none() {
        std::env::remove_var("CONSTELLATION_LUMINA_TOKEN");
        assert_eq!(constellation_lumina_token(), None);
        std::env::set_var("CONSTELLATION_LUMINA_TOKEN", "lumina-secret"); // pii-test-fixture
        assert_eq!(constellation_lumina_token(), Some("lumina-secret".to_string()));
        std::env::remove_var("CONSTELLATION_LUMINA_TOKEN");
    }

    #[test]
    #[serial]
    fn constellation_session_ttl_seconds_default_and_override() {
        std::env::remove_var("CONSTELLATION_SESSION_TTL_SECONDS");
        assert_eq!(constellation_session_ttl_seconds(), 3_600);
        std::env::set_var("CONSTELLATION_SESSION_TTL_SECONDS", "60");
        assert_eq!(constellation_session_ttl_seconds(), 60);
        std::env::remove_var("CONSTELLATION_SESSION_TTL_SECONDS");
    }

    #[test]
    #[serial]
    fn constellation_cookie_secure_defaults_false() {
        std::env::remove_var("CONSTELLATION_COOKIE_SECURE");
        assert!(!constellation_cookie_secure());
        std::env::set_var("CONSTELLATION_COOKIE_SECURE", "true");
        assert!(constellation_cookie_secure());
        std::env::remove_var("CONSTELLATION_COOKIE_SECURE");
    }

    // ── GMQ-02 r2: wait_ttl_secs must never be shorter than max_wait_secs ──

    #[test]
    #[serial]
    fn gitea_merge_queue_wait_ttl_defaults_above_max_wait() {
        std::env::remove_var("GITEA_MERGE_QUEUE_WAIT_TTL_SECS");
        std::env::remove_var("GITEA_MERGE_QUEUE_MAX_WAIT_SECS");
        assert_eq!(gitea_merge_queue_max_wait_secs(), 300);
        assert_eq!(gitea_merge_queue_wait_ttl_secs(), 360);
    }

    #[test]
    #[serial]
    fn gitea_merge_queue_wait_ttl_below_max_wait_is_clamped_up() {
        // A misconfigured `.env` (operator sets a wait TTL shorter than the
        // max-wait ceiling) must never be honored literally — it would expire
        // the wait ordering out from under a still-legitimately-polling
        // waiter. The effective value must be clamped to
        // `max_wait_secs + WAIT_TTL_MIN_MARGIN_SECS`.
        std::env::set_var("GITEA_MERGE_QUEUE_MAX_WAIT_SECS", "300");
        std::env::set_var("GITEA_MERGE_QUEUE_WAIT_TTL_SECS", "10"); // far too short
        assert_eq!(gitea_merge_queue_max_wait_secs(), 300);
        assert_eq!(
            gitea_merge_queue_wait_ttl_secs(),
            360,
            "a too-short configured wait_ttl must be clamped up to max_wait + margin"
        );
        std::env::remove_var("GITEA_MERGE_QUEUE_MAX_WAIT_SECS");
        std::env::remove_var("GITEA_MERGE_QUEUE_WAIT_TTL_SECS");
    }

    #[test]
    #[serial]
    fn gitea_merge_queue_wait_ttl_above_max_wait_is_honored() {
        std::env::set_var("GITEA_MERGE_QUEUE_MAX_WAIT_SECS", "60");
        std::env::set_var("GITEA_MERGE_QUEUE_WAIT_TTL_SECS", "500"); // already generous
        assert_eq!(gitea_merge_queue_wait_ttl_secs(), 500);
        std::env::remove_var("GITEA_MERGE_QUEUE_MAX_WAIT_SECS");
        std::env::remove_var("GITEA_MERGE_QUEUE_WAIT_TTL_SECS");
    }
}
