//! Intake orchestrator (S83 MINT-01).
//!
//! Drives the context stress suite across a list of tiers, records each tier in
//! Postgres, derives the operational profile, and stores it. Implements the
//! single-VRAM lifecycle policy: record the currently-hot model first; if the
//! target is not hot, load it (Ollama lazy-load) and restore the prior hot
//! model afterward; if the target IS already hot, skip load/unload entirely.
//!
//! The code/agent suites (MINT-02/03) are stubbed so the tool never fails when
//! they are requested.

use std::time::Duration;

use serde::Deserialize;

use async_trait::async_trait;

use crate::error::ToolError;
use crate::intake::context::{self, TierResult};
use crate::intake::storage::{self, ContextRunRow, OperationalProfileRow};
use crate::intake::{coder_sweep, RunKind, SweepRunner};

// ---------------------------------------------------------------------------
// Coder sub-runner under the unified MINT harness (MINT2-04)
// ---------------------------------------------------------------------------

/// The coder sub-runner registered into [`crate::intake::MintHarness`]. A thin
/// adapter over the existing `coder_sweep::run` fleet driver: the coder cases
/// and their measurement are unchanged — this only routes the coder sweep
/// through the one shared harness surface. Its config (languages, case-limit,
/// mem-config) is read from the SAME env vars the standalone
/// `intake_coder_sweep` binary read, so runtime behavior is byte-for-byte the
/// same; the binary is now merely `MintHarness::run(RunKind::Coder)`.
pub struct CoderSweepRunner {
    langs: Vec<String>,
    case_limit: Option<usize>,
    mem_config: Option<String>,
    /// MINT2-06: `--only-stale` run mode (`MINT_ONLY_STALE`). Default `false` →
    /// the FULL sweep; `true` → re-run only the models with a stale coder cell.
    only_stale: bool,
}

impl CoderSweepRunner {
    /// Build from the env-sourced config (identical vars/behavior to the old
    /// `intake_coder_sweep` binary). Pure env reads that default gracefully —
    /// no DB, no network — so it is safe to construct in a unit test.
    pub fn from_env() -> Self {
        CoderSweepRunner {
            langs: coder_sweep::langs_from_env(),
            case_limit: coder_sweep::case_limit_from_env(),
            mem_config: coder_sweep::mem_config_from_env(),
            only_stale: crate::intake::only_stale_from_env(),
        }
    }
}

#[async_trait]
impl SweepRunner for CoderSweepRunner {
    fn kind(&self) -> RunKind {
        RunKind::Coder
    }

    async fn run(&self) -> std::process::ExitCode {
        coder_sweep::run(
            &self.langs,
            self.case_limit,
            self.mem_config.as_deref(),
            self.only_stale,
        )
        .await
    }
}

/// The full graduated tier list from the spec.
pub const FULL_TIERS: [usize; 9] =
    [2000, 4000, 8000, 16000, 32000, 48000, 64000, 96000, 128000];

/// Reduced tier set for the smoke run (no model swap, hot model only).
pub const SMOKE_TIERS: [usize; 3] = [2000, 8000, 16000];

/// Per-tier inference timeout. Generous so a slow large-context generation
/// isn't mistaken for an OOM. Overridable via `INTAKE_TIER_TIMEOUT_SEC`.
/// Delegates to the canonical resolver (Phase 2 item 3) — same default
/// (600s), same env var, same behavior.
fn tier_timeout() -> Duration {
    super::timeouts::env_timeout("INTAKE_TIER_TIMEOUT_SEC", 600)
}

// ---------------------------------------------------------------------------
// FIX2 (S125): bound the context sweep for large models
// ---------------------------------------------------------------------------
//
// A very large model (e.g. 120b) cannot serve the huge context tiers (32k+) on
// this fleet's VRAM, so before these two changes the sweep would attempt every
// tier up to 128k and burn a FULL `INTAKE_TIER_TIMEOUT_SEC` (~10 min) timing out
// on each — ~80 min/model of pure waste. Two pure, testable levers bound it:
//   1. `max_ctx_tier_for` caps the ladder by parameter scale (parsed from the
//      model name's trailing `<N>b`) so guaranteed-infeasible tiers are never
//      even attempted.
//   2. `tier_hit_ceiling` stops escalation the moment a tier OOMs, times out, or
//      otherwise fails — a larger tier can only be heavier, so there is no point
//      attempting it (this bounds a big model to ~one timeout, not one per tier).

/// FIX2 (S125): parse a model's parameter scale in BILLIONS from a trailing
/// `<N>b` token in its name (`gpt-oss:120b` → 120, `qwen3-coder:30b` → 30,
/// `llama3:8b` → 8, `gemma2:2.6b` → 2 (fractional rounds down)). Takes the LAST
/// `<number>b` token at a name boundary (the `b` not followed by another
/// alphanumeric), so `qwen2.5-coder:32b` → 32 (not 2), and a `2.5` version tag
/// not followed by `b` is ignored. Returns `None` when no `<N>b` token exists.
/// Pure.
pub fn parse_param_scale_b(model_name: &str) -> Option<u32> {
    let n = model_name.to_ascii_lowercase();
    let bytes = n.as_bytes();
    let mut best: Option<u32> = None;
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let start = i;
            let mut seen_dot = false;
            let mut j = i;
            while j < bytes.len()
                && (bytes[j].is_ascii_digit() || (bytes[j] == b'.' && !seen_dot))
            {
                if bytes[j] == b'.' {
                    seen_dot = true;
                }
                j += 1;
            }
            // Require a trailing 'b' immediately after the number, at a name
            // boundary (end, or a non-alphanumeric like '-', ':', '_').
            if j < bytes.len() && bytes[j] == b'b' {
                let after = j + 1;
                let boundary = after >= bytes.len() || !bytes[after].is_ascii_alphanumeric();
                if boundary {
                    if let Ok(v) = n[start..j].parse::<f64>() {
                        best = Some(v as u32); // trailing token wins (overwrite)
                    }
                }
            }
            i = j.max(start + 1);
        } else {
            i += 1;
        }
    }
    best
}

/// FIX2 (S125): the max context tier for a given parameter scale (billions).
/// Pure — the env override is applied separately in [`max_ctx_tier_for`] so this
/// mapping stays deterministically testable:
///   >=100b → 16000, >=30b → 32000, >=13b → 64000, else FULL (128000).
/// `None` (no parseable size) is assumed small → FULL ladder.
pub fn cap_from_scale(scale_b: Option<u32>) -> usize {
    let full = *FULL_TIERS.last().expect("FULL_TIERS is non-empty");
    match scale_b {
        Some(b) if b >= 100 => 16000,
        Some(b) if b >= 30 => 32000,
        Some(b) if b >= 13 => 64000,
        _ => full,
    }
}

/// FIX2 (S125): the largest context tier worth attempting for `model_name`. A
/// global override (`INTAKE_MAX_CTX_TIER`) forces a ceiling for every model;
/// otherwise the cap is derived from the model's parsed parameter scale via
/// [`cap_from_scale`].
pub fn max_ctx_tier_for(model_name: &str) -> usize {
    if let Some(cap) = std::env::var("INTAKE_MAX_CTX_TIER")
        .ok()
        .and_then(|s| s.trim().parse::<usize>().ok())
        .filter(|n| *n > 0)
    {
        return cap;
    }
    cap_from_scale(parse_param_scale_b(model_name))
}

/// S125 (SUITE-WALLCLOCK): pure core of [`suite_wallclock_cap_for`]. Given an
/// optional env override (seconds) and the model's parsed parameter scale
/// (billions), produce the per-suite wall-clock cap. The env override, when a
/// POSITIVE integer, WINS over the size mapping (`0`/`None` → fall through to the
/// size mapping): `>=100b → 20 min`, `>=30b → 40 min`, else `None` (uncapped —
/// small models comfortably finish any suite well inside a bound). Pure so the
/// mapping AND the override precedence are deterministically unit-testable
/// without touching the environment.
fn suite_wallclock_cap_core(env_override_secs: Option<u64>, scale_b: Option<u32>) -> Option<Duration> {
    if let Some(secs) = env_override_secs.filter(|n| *n > 0) {
        return Some(Duration::from_secs(secs));
    }
    match scale_b {
        Some(b) if b >= 100 => Some(Duration::from_secs(20 * 60)),
        Some(b) if b >= 30 => Some(Duration::from_secs(40 * 60)),
        _ => None,
    }
}

/// S125 (SUITE-WALLCLOCK): a per-suite WALL-CLOCK cap for `model_name`, scaled by
/// its parameter size, so a huge model (e.g. `gpt-oss:120b`) cannot spend
/// unbounded time in any single inference-bound suite (code/agent/tool_routing/
/// …). The context suite already has internal tier bounds; this cap is a hard
/// ceiling applied ON TOP, for uniformity and as a guard against pathological
/// cases. Size mapping (via [`parse_param_scale_b`]): `>=100b → 20 min`,
/// `>=30b → 40 min`, else `None`.
///
/// A global env override `INTAKE_SUITE_WALLCLOCK_CAP_SEC`, when set to a POSITIVE
/// integer, forces that cap (seconds) for EVERY model and WINS over the size
/// mapping; `0`/unset/unparseable → the size mapping. The pure precedence + size
/// mapping live in [`suite_wallclock_cap_core`]; this wrapper only reads the env.
pub fn suite_wallclock_cap_for(model_name: &str) -> Option<Duration> {
    let env_override = std::env::var("INTAKE_SUITE_WALLCLOCK_CAP_SEC")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok());
    suite_wallclock_cap_core(env_override, parse_param_scale_b(model_name))
}

/// S125 (SUITE-WALLCLOCK): await `fut`, enforcing `cap` as a hard wall-clock
/// ceiling via [`tokio::time::timeout`] when it is `Some`. Returns
/// `Some(output)` when the future completed within the cap (or when `cap` is
/// `None`, in which case it is awaited normally with no wrapper), and `None`
/// when the cap elapsed first — in which case the suite future is dropped and
/// cancelled, abandoning only the un-run remainder; any rows the suite already
/// wrote incrementally (per case/tier/scenario) persist. Never itself errors,
/// so a cap NEVER fails the whole model — the caller records a capped/partial
/// note and moves on to the next suite. Composes cleanly on top of the suites'
/// existing idle-switch yields (the cap is just a ceiling above them).
async fn with_suite_cap<F, T>(cap: Option<Duration>, fut: F) -> Option<T>
where
    F: std::future::Future<Output = T>,
{
    match cap {
        Some(d) => tokio::time::timeout(d, fut).await.ok(),
        None => Some(fut.await),
    }
}

/// FIX2 (S125): apply a max-tier `cap` to a tier ladder, keeping only tiers
/// `<= cap`. If the cap would exclude EVERY tier, the single smallest tier is
/// retained so a model always gets at least one measurement. Pure.
pub fn cap_tiers(tiers: &[usize], cap: usize) -> Vec<usize> {
    let mut kept: Vec<usize> = tiers.iter().copied().filter(|t| *t <= cap).collect();
    if kept.is_empty() {
        if let Some(min) = tiers.iter().copied().min() {
            kept.push(min);
        }
    }
    kept
}

/// FIX2 (S125): whether a tier result is this model's feasible-context CEILING —
/// an OOM or a genuine TIMEOUT (the per-tier deadline elapsed) — after which the
/// runner records the ceiling and STOPS escalating to larger tiers. A larger
/// context can only be heavier, so a real capacity limit at this tier means the
/// next tier is hopeless too; stopping bounds a big model to ~one timeout rather
/// than one per tier.
///
/// Deliberately NARROW (codex re-review): a transient connect/body/parse error
/// is NOT a ceiling — it does not prove a larger context is infeasible, so a
/// one-off network blip on (say) the 8k tier must not wrongly cap the model at
/// 8k. Such a tier is still RECORDED as a failure, but escalation CONTINUES to
/// the next tier. A timeout surfaces as `error = Some("... timed out ...")` with
/// `oom = false`, matched by [`context::is_timeout_error`]; an OOM/overload (incl.
/// HTTP 500/503) is already flagged `oom = true` by `run_tier`. Pure.
fn tier_hit_ceiling(oom: bool, error: Option<&str>) -> bool {
    oom || error.map(context::is_timeout_error).unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Ollama /api/ps — currently-hot model + VRAM
// ---------------------------------------------------------------------------

#[derive(Deserialize, Default)]
struct PsResponse {
    #[serde(default)]
    models: Vec<PsModel>,
}

#[derive(Deserialize, Default, Clone)]
struct PsModel {
    #[serde(default)]
    name: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    size_vram: u64,
}

/// Query `/api/ps`. Returns the list of (name, size_vram_bytes) loaded models.
async fn query_ps(client: &reqwest::Client) -> Vec<(String, u64)> {
    let base = context::ollama_base();
    match client.get(format!("{base}/api/ps")).send().await {
        Ok(resp) if resp.status().is_success() => match resp.json::<PsResponse>().await {
            Ok(ps) => ps
                .models
                .into_iter()
                .map(|m| {
                    let name = if m.name.is_empty() { m.model } else { m.name };
                    (name, m.size_vram)
                })
                .filter(|(n, _)| !n.is_empty())
                .collect(),
            Err(_) => Vec::new(),
        },
        _ => Vec::new(),
    }
}

/// The name of the currently-hot (first loaded) model, if any.
async fn current_hot_model(client: &reqwest::Client) -> Option<String> {
    query_ps(client).await.into_iter().next().map(|(n, _)| n)
}

/// VRAM (MB) reported for `model_name` in `/api/ps`, if loaded. Matches on exact
/// name or a tag-prefix (so "gpt-oss:20b" matches "gpt-oss:20b").
async fn model_vram_mb(client: &reqwest::Client, model_name: &str) -> Option<i32> {
    let loaded = query_ps(client).await;
    loaded
        .iter()
        .find(|(n, _)| n == model_name || n.starts_with(model_name))
        .map(|(_, bytes)| (*bytes / (1024 * 1024)) as i32)
}

/// Whether `target` is currently the (or a) hot model.
fn is_hot(loaded: &[(String, u64)], target: &str) -> bool {
    loaded
        .iter()
        .any(|(n, _)| n == target || n.starts_with(target))
}

/// Ask Ollama to load a model (keep_alive long enough for the run) by issuing a
/// trivial generate with an empty prompt. Lazy-load brings the model hot.
async fn load_model(client: &reqwest::Client, model: &str) -> Result<(), ToolError> {
    // BT-03: registry-resolve the backend instead of assuming local ollama. The Ollama
    // `keep_alive` pre-warm is ollama-specific; for any other backend kind (openai/
    // llama-server/daemon) the model is loaded on first request and the backend process
    // is brought up by `lifecycle::ensure_up` in `infer_with_metrics`, so there is no
    // separate pre-warm to do here — skip it rather than POST an ollama route at a
    // non-ollama URL. Uses the registry-resolved base, not the hardcoded loopback.
    let backend = crate::intake::infer::resolve_backend(model);
    if backend.kind != "ollama" {
        return Ok(());
    }
    let base = backend.url;
    let body = serde_json::json!({ "model": model, "keep_alive": context::OLLAMA_KEEP_ALIVE });
    client
        .post(format!("{base}/api/generate"))
        .json(&body)
        .timeout(Duration::from_secs(300))
        .send()
        .await
        .map_err(|e| ToolError::Http(format!("load '{model}' failed: {e}")))?;
    Ok(())
}

/// Restore the previously-hot model by lazy-loading it again (and evicting the
/// intake target via keep_alive:0). Best-effort: logs but never errors.
async fn restore_model(client: &reqwest::Client, prior: &str, evict: &str) {
    // BT-03: evict via the backend-aware helper (no-op for non-ollama kinds).
    evict_model(client, evict).await;
    // Reload the prior hot model.
    if let Err(e) = load_model(client, prior).await {
        tracing::warn!("intake: failed to restore prior hot model '{prior}': {e}");
    }
}

// ---------------------------------------------------------------------------
// Derived operational profile (pure)
// ---------------------------------------------------------------------------

/// A minimal view of a completed tier used to derive the operational profile.
#[derive(Debug, Clone)]
pub struct TierSummary {
    pub context_tokens: i32,
    pub throughput: Option<f64>,
    pub recall: Option<i32>,
    pub oom: bool,
}

/// Compute recommended timeouts (seconds) from the degradation context and the
/// throughput at that tier:
///   chat  = ceil(context / throughput) + 10
///   build = chat * 4
///   deep  = chat * 10
/// Returns (chat, build, deep). Falls back to a conservative 30/120/300 when
/// throughput is unknown/zero.
pub fn recommended_timeouts(context_at_degradation: i32, throughput: Option<f64>) -> (i32, i32, i32) {
    let chat = match throughput {
        Some(tp) if tp > 0.0 => {
            ((context_at_degradation as f64 / tp).ceil() as i32) + 10
        }
        _ => 30,
    };
    let chat = chat.max(10);
    (chat, chat * 4, chat * 10)
}

/// Derive the operational profile from the completed tier series.
///
/// - `max_context_safe`        = highest tier with recall == 3
/// - `max_context_absolute`    = highest non-OOM tier
/// - `quality_degradation_point` = first tier where recall < 2 (else None)
/// - throughput_at_{2k,8k,16k,32k,64k} = nearest measured tier at that size
/// - recommended timeouts from the degradation tier (or the absolute max)
pub fn derive_profile(tiers: &[TierSummary]) -> OperationalProfileRow {
    let mut op = OperationalProfileRow::default();

    // Safe: highest with full recall.
    op.max_context_safe = tiers
        .iter()
        .filter(|t| t.recall == Some(3) && !t.oom)
        .map(|t| t.context_tokens)
        .max();

    // Absolute: highest non-OOM tier.
    op.max_context_absolute = tiers
        .iter()
        .filter(|t| !t.oom)
        .map(|t| t.context_tokens)
        .max();

    // Degradation: first (ascending) tier with recall < 2.
    let mut sorted: Vec<&TierSummary> = tiers.iter().collect();
    sorted.sort_by_key(|t| t.context_tokens);
    let degradation = sorted
        .iter()
        .find(|t| matches!(t.recall, Some(r) if r < 2))
        .map(|t| t.context_tokens);
    op.quality_degradation_point = degradation;

    // Throughput at standard tiers (exact match on the tier token target).
    let tp_at = |tokens: i32| -> Option<f64> {
        // Match the closest measured tier whose context is near `tokens`.
        sorted
            .iter()
            .filter_map(|t| t.throughput.map(|tp| (t.context_tokens, tp)))
            .min_by_key(|(ct, _)| (ct - tokens).abs())
            .and_then(|(ct, tp)| {
                // Only accept if reasonably close (within 25%) to the requested tier.
                if (ct - tokens).abs() <= (tokens / 4).max(1) {
                    Some(tp)
                } else {
                    None
                }
            })
    };
    op.throughput_at_2k = tp_at(2000);
    op.throughput_at_8k = tp_at(8000);
    op.throughput_at_16k = tp_at(16000);
    op.throughput_at_32k = tp_at(32000);
    op.throughput_at_64k = tp_at(64000);

    // Timeouts: base on the degradation context (or absolute max if no
    // degradation observed), using throughput at that tier.
    let timeout_ctx = degradation
        .or(op.max_context_absolute)
        .unwrap_or(2000);
    let tp_at_timeout = sorted
        .iter()
        .filter(|t| t.context_tokens == timeout_ctx)
        .find_map(|t| t.throughput);
    let (chat, build, deep) = recommended_timeouts(timeout_ctx, tp_at_timeout);
    op.recommended_timeout_chat_sec = Some(chat);
    op.recommended_timeout_build_sec = Some(build);
    op.recommended_timeout_deep_sec = Some(deep);

    // Coarse tier label.
    op.overall_tier = Some(classify_tier(op.max_context_safe));

    op
}

/// Coarse capacity label from the safe context ceiling.
pub fn classify_tier(max_context_safe: Option<i32>) -> String {
    match max_context_safe {
        Some(c) if c >= 64000 => "deep".to_string(),
        Some(c) if c >= 16000 => "standard".to_string(),
        Some(c) if c > 0 => "blitz".to_string(),
        _ => "review-only".to_string(),
    }
}

// ---------------------------------------------------------------------------
// Orchestration (live)
// ---------------------------------------------------------------------------

/// Outcome of the context suite for the tool return summary.
pub struct ContextSuiteOutcome {
    pub tiers_run: usize,
    pub stopped_on_oom: bool,
    pub op: OperationalProfileRow,
    pub prior_hot: Option<String>,
    /// The model_profiles row id created for this run — so the code/agent suites
    /// can attach their rows to the same profile.
    pub profile_id: uuid::Uuid,
}

/// Create a fresh `model_profiles` row for a model not going through the context
/// suite (e.g. a code-only or agent-only run). Returns the new profile id.
pub async fn create_profile_row(model_name: &str) -> Result<uuid::Uuid, ToolError> {
    create_profile_row_for_provider(model_name, "ollama").await
}

/// [`create_profile_row`], but with an explicit `provider` tag rather than the
/// hardcoded `"ollama"` — MINT-DIFF-01's diffusion suite runs models on the
/// dgem daemon, not Ollama, so its profile rows should say so.
pub async fn create_profile_row_for_provider(model_name: &str, provider: &str) -> Result<uuid::Uuid, ToolError> {
    let pool = storage::get_pool().await?;
    // MINT-INTAKE-SCHEMA: ensure the base profiling schema exists before writing.
    // The MCP intake tools reach the profile tables through here / run_context_suite
    // WITHOUT going through the sweep's own migrate() call, so an intake DB that
    // only ran the discovery migration would 500 on every write. migrate() is
    // idempotent + advisory-locked (same call the sweep already makes).
    crate::intake::assistant::schema::migrate(&pool).await?;
    storage::insert_model_profile(&pool, model_name, provider, None, None).await
}

/// Run the context suite end-to-end against `model_name` for the given `tiers`.
/// Stores a model_profiles row, one context_profile_runs row per tier, and the
/// derived model_operational_profiles row. Honors the single-VRAM policy.
pub async fn run_context_suite(
    model_name: &str,
    tiers: &[usize],
    manage_lifecycle: bool,
) -> Result<ContextSuiteOutcome, ToolError> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(900))
        .build()
        .map_err(|e| ToolError::Http(format!("client build failed: {e}")))?;

    // VRAM lifecycle: record currently-hot model.
    let loaded = query_ps(&client).await;
    let prior_hot = loaded.first().map(|(n, _)| n.clone());
    let target_hot = is_hot(&loaded, model_name);

    // Single-model path manages its own load/restore. The fleet path
    // (`manage_lifecycle=false`) loads/unloads externally and restores the
    // daily driver only once, after the entire run — so here it's a no-op.
    let need_restore = if manage_lifecycle && !target_hot {
        load_model(&client, model_name).await?;
        prior_hot.clone()
    } else {
        None
    };

    let pool = storage::get_pool().await?;
    // MINT-INTAKE-SCHEMA: ensure the base profiling schema exists (see
    // create_profile_row) — the MCP context suite writes model_profiles +
    // context_profile_runs + model_operational_profiles and must not 500 on a
    // DB where only the discovery migration ran.
    crate::intake::assistant::schema::migrate(&pool).await?;
    let vram_now = model_vram_mb(&client, model_name).await;
    let vram_gb = vram_now.map(|mb| mb as f64 / 1024.0);
    let profile_id =
        storage::insert_model_profile(&pool, model_name, "ollama", None, vram_gb).await?;

    let timeout = tier_timeout();

    // FIX2 (S125): cap the tier ladder by model size so guaranteed-infeasible
    // huge tiers on a big model aren't even attempted (a 120b model otherwise
    // burns a full ~10-min timeout on each of 32k..128k). A global override
    // (`INTAKE_MAX_CTX_TIER`) can force a ceiling for any model.
    let tier_cap = max_ctx_tier_for(model_name);
    let capped_tiers = cap_tiers(tiers, tier_cap);
    let tiers: &[usize] = &capped_tiers;

    let mut summaries: Vec<TierSummary> = Vec::new();
    let mut stopped_on_oom = false;
    let mut tiers_run = 0usize;

    for &target in tiers {
        let mut tr: TierResult = context::run_tier(&client, model_name, target, timeout).await;
        // Memory snapshot for this tier.
        tr.memory_usage_mb = model_vram_mb(&client, model_name).await;

        let row = ContextRunRow {
            context_tokens: tr.context_tokens as i32,
            throughput_tok_per_sec: tr.throughput_tok_per_sec,
            ttft_ms: tr.ttft_ms,
            total_time_ms: tr.total_time_ms,
            recall_score: tr.recall_score,
            coherence_score: tr.coherence_score, // None — coherence judge deferred
            memory_usage_mb: tr.memory_usage_mb,
            oom: tr.oom,
            error: tr.error.clone(),
        };
        let context_run_id = storage::insert_context_run(&pool, profile_id, &row).await?;
        tiers_run += 1;

        // multi-point-score-tracking: preserve this tier's per-point
        // measurements (throughput + recall vs. context length) alongside the
        // fixed `throughput_at_*` columns the operational profile keeps. A
        // metric whose value is `None` for this tier (e.g. a tier that OOM'd
        // before producing a throughput reading) is skipped — never written as
        // a 0 placeholder. Best-effort: a score-point write failure must not
        // abort the suite (the durable tier row is already persisted above), so
        // it is logged and swallowed rather than `?`-propagated.
        let mut points: Vec<storage::ScorePoint> = Vec::new();
        if let Some(tp) = row.throughput_tok_per_sec {
            points.push(storage::ScorePoint {
                axis: "context_tokens".to_string(),
                x_value: row.context_tokens as f64,
                x_label: None,
                metric: "throughput_tok_per_sec".to_string(),
                value: Some(tp),
            });
        }
        if let Some(recall) = row.recall_score {
            points.push(storage::ScorePoint {
                axis: "context_tokens".to_string(),
                x_value: row.context_tokens as f64,
                x_label: None,
                metric: "recall_score".to_string(),
                value: Some(recall as f64),
            });
        }
        if let Err(e) = storage::insert_score_points(
            &pool,
            storage::ScorePointParent::Context(context_run_id),
            profile_id,
            &points,
        )
        .await
        {
            tracing::warn!("intake: failed to persist context score points: {e}");
        }

        summaries.push(TierSummary {
            context_tokens: tr.context_tokens as i32,
            throughput: tr.throughput_tok_per_sec,
            recall: tr.recall_score,
            oom: tr.oom,
        });

        // FIX2 (S125): stop escalating once this tier is the feasible ceiling —
        // an OOM, a TIMEOUT, or any hard failure. Previously only `oom` stopped
        // the loop, so a large model would time out on EVERY larger tier; a
        // timeout has `oom = false` but `error = Some(...)`, so we now key off
        // the broader ceiling predicate. (`stopped_on_oom` doubles as the
        // "stopped early" flag for the report.)
        if tier_hit_ceiling(tr.oom, tr.error.as_deref()) {
            stopped_on_oom = true;
            break;
        }
    }

    let op = derive_profile(&summaries);
    storage::insert_operational_profile(&pool, profile_id, &op).await?;

    // Restore prior hot model if we swapped (single-model path only).
    if manage_lifecycle {
        if let Some(prior) = &need_restore {
            if prior != model_name {
                restore_model(&client, prior, model_name).await;
            }
        }
    }

    Ok(ContextSuiteOutcome {
        tiers_run,
        stopped_on_oom,
        op,
        prior_hot,
        profile_id,
    })
}

/// Unload a model from VRAM (Ollama keep_alive:0). Best-effort.
async fn evict_model(client: &reqwest::Client, model: &str) {
    // BT-03: keep_alive:0 eviction is ollama-specific. Non-ollama backends are unloaded by
    // their own lifecycle (lemonade/vLLM idle-stop, Chord-managed) — skip rather than POST
    // an ollama route at a non-ollama URL.
    let backend = crate::intake::infer::resolve_backend(model);
    if backend.kind != "ollama" {
        return;
    }
    let base = backend.url;
    let _ = client
        .post(format!("{base}/api/generate"))
        .json(&serde_json::json!({ "model": model, "keep_alive": 0 }))
        .timeout(Duration::from_secs(60))
        .send()
        .await;
}

/// List Ollama chat models (excludes embedding models) for an auto fleet run.
pub async fn list_chat_models(client: &reqwest::Client) -> Vec<String> {
    let base = context::ollama_base();
    let resp = match client
        .get(format!("{base}/api/tags"))
        .timeout(Duration::from_secs(30))
        .send()
        .await
    {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    let val: serde_json::Value = match resp.json().await {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    val.get("models")
        .and_then(|m| m.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|m| m.get("name").and_then(|n| n.as_str()))
                .filter(|n| !n.to_lowercase().contains("embed"))
                .map(|n| n.to_string())
                .collect()
        })
        .unwrap_or_default()
}

/// One model's result in a fleet run.
pub struct FleetModelResult {
    pub model: String,
    pub outcome: Result<ContextSuiteOutcome, ToolError>,
}

/// Context-only fleet run with the simplified overnight lifecycle (superseded
/// by `run_fleet_suites`, kept for the context-only path / tests):
///   record hot → for each model: load → profile (no restore) → unload
///   → restore the daily-driver hot model ONCE at the very end.
/// Lumina is offline during this — that's the intended overnight behavior.
#[allow(dead_code)]
pub async fn run_fleet(models: &[String], tiers: &[usize]) -> Vec<FleetModelResult> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(900))
        .build()
        .expect("client");

    let prior_hot = current_hot_model(&client).await;
    let mut results = Vec::new();

    for model in models {
        // Bring this model hot (single-VRAM evicts the previous one); then
        // profile it without any per-model restore.
        if let Err(e) = load_model(&client, model).await {
            results.push(FleetModelResult { model: model.clone(), outcome: Err(e) });
            continue;
        }
        let outcome = run_context_suite(model, tiers, false).await;
        evict_model(&client, model).await;
        results.push(FleetModelResult { model: model.clone(), outcome });
    }

    // Restore the daily driver only now that the whole fleet is done.
    if let Some(prior) = &prior_hot {
        let _ = load_model(&client, prior).await;
    }
    results
}

/// Per-model fleet result with the suites that ran and a one-line summary.
pub struct FleetSuiteResult {
    pub model: String,
    pub suites: Vec<String>,
    pub summary: String,
    pub skipped: bool,
    /// S125 (SUITE-WALLCLOCK): names of the suites (if any) that hit their
    /// per-suite wall-clock cap for this model and were recorded as capped/
    /// partial. NON-empty here does NOT mean the model failed — the already-
    /// written rows persist and the remaining suites still ran; it only lets an
    /// operator see which suites were bounded. Empty for the common case.
    pub capped_suites: Vec<String>,
}

/// Outcome of the diffusion suite (MINT-DIFF-01) for the tool return summary.
pub struct DiffusionSuiteOutcome {
    pub profile_id: uuid::Uuid,
    pub use_cases_run: usize,
    pub avg_use_case_success: f64,
    pub avg_time_to_output_ms: f64,
    /// One-line-per-use-case summary, in [`crate::intake::newcats::diffusion::USE_CASES`] order.
    pub per_use_case: Vec<String>,
}

/// SUITE-STT: outcome of the speech-to-text suite for the tool return summary.
pub struct SttSuiteOutcome {
    pub profile_id: uuid::Uuid,
    pub clips_run: usize,
    /// Mean digit-normalized WER over the clips that transcribed (lower better).
    pub avg_wer: f64,
    /// Mean real-time factor over the clips whose audio duration was known.
    pub avg_rtf: f64,
    /// One-line-per-clip summary, in manifest order.
    pub per_clip: Vec<String>,
}

/// SUITE-STT: run the speech-to-text suite end-to-end against `model_name`.
///
/// Loads the STT corpus manifest from `INTAKE_CORPUS_DIR`
/// (`[{ "audio_file", "reference" }, ...]`, e.g. the bundled <host> corpus at
/// `/opt/ollama-models/mint-corpora/stt/`), then for each clip: reads the audio
/// bytes, derives the clip duration from its WAV header
/// ([`crate::intake::newcats::voice_transcription::wav_duration_ms`]), transcribes
/// it through [`crate::intake::infer::transcribe_with_metrics`] (the `openai`
/// backend arm → Chord's `/v1/audio/transcriptions` in front of faster-whisper),
/// and writes the digit-normalized WER / accuracy / latency / RTF rows via
/// [`crate::intake::newcats::voice_transcription::score_and_write`].
///
/// Creates its own `model_profiles` row (provider `"openai"`, since a whisper
/// model is served behind Chord's OpenAI-compatible route, not Ollama) — mirrors
/// [`run_diffusion_suite`]'s own-profile-row pattern. A per-clip read/transcribe
/// error is recorded and the clip skipped rather than aborting the whole suite —
/// the same "failure is still useful signal" convention as every other newcats
/// suite. A missing/unreadable corpus fails clean with the manifest's
/// `ToolError` before any profile row is created.
pub async fn run_stt_suite(model_name: &str) -> Result<SttSuiteOutcome, ToolError> {
    use crate::intake::assistant::{BackendTag, ModelId};
    use crate::intake::infer::transcribe_with_metrics;
    use crate::intake::newcats::text_similarity::word_error_rate_normalized;
    use crate::intake::newcats::voice_transcription::{
        self, wav_duration_ms, TranscriptionOutcome,
    };

    // Resolve + parse the corpus up front so a missing/broken corpus fails clean
    // before we create any DB rows.
    let dir = crate::intake::code::corpus_dir()?;
    let manifest = voice_transcription::load_manifest(&dir)?;

    let profile_id = create_profile_row_for_provider(model_name, "openai").await?;
    let pool = storage::get_pool().await?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(900))
        .build()
        .map_err(|e| ToolError::Http(e.to_string()))?;
    let model_id = ModelId::from(model_name);
    // FK fix (S125): `assistant_dimension_score.run_id` REFERENCES
    // `assistant_profile_run(id)`, NOT `model_profiles(id)`. Create the run
    // parent ONCE per driver invocation and score every row against it. The
    // `profile_id` above remains the catalog/model_profiles id (used in the
    // returned outcome + `model_id`), not the FK target.
    let run_id = crate::intake::assistant::schema::insert_run(&pool).await?;

    let mut per_clip = Vec::with_capacity(manifest.len());
    let mut wer_sum = 0.0;
    let mut rtf_sum = 0.0;
    let mut rtf_n = 0usize;
    let mut n = 0usize;

    for entry in &manifest {
        let audio_path = dir.join(&entry.audio_file);
        let audio_bytes = match std::fs::read(&audio_path) {
            Ok(b) => b,
            Err(e) => {
                per_clip.push(format!("{}: read error ({e})", entry.audio_file));
                continue;
            }
        };
        let audio_duration_ms = wav_duration_ms(&audio_bytes);
        let metrics = transcribe_with_metrics(
            &client,
            model_name,
            &audio_bytes,
            &entry.audio_file,
            Duration::from_secs(300),
        )
        .await;
        if let Some(err) = &metrics.error {
            per_clip.push(format!("{}: error ({err})", entry.audio_file));
            continue;
        }
        let backend_tag = metrics
            .hardware
            .as_deref()
            .and_then(BackendTag::parse)
            .unwrap_or(BackendTag::Gpu);
        let outcome = TranscriptionOutcome {
            transcript: metrics.transcript.clone(),
            latency_ms: metrics.latency_ms,
            audio_duration_ms,
        };

        let wer = word_error_rate_normalized(&outcome.transcript, &entry.reference);
        wer_sum += wer;
        let rtf = voice_transcription::real_time_factor(outcome.latency_ms, audio_duration_ms);
        if let Some(r) = rtf {
            rtf_sum += r;
            rtf_n += 1;
        }
        n += 1;

        voice_transcription::score_and_write(
            &pool,
            run_id,
            model_id.clone(),
            backend_tag,
            &entry.reference,
            &outcome,
        )
        .await?;

        per_clip.push(format!(
            "{}: wer={wer:.3} latency_ms={} rtf={}",
            entry.audio_file,
            outcome.latency_ms,
            rtf.map(|r| format!("{r:.2}")).unwrap_or_else(|| "n/a".into()),
        ));
    }

    Ok(SttSuiteOutcome {
        profile_id,
        clips_run: n,
        avg_wer: if n > 0 { wer_sum / n as f64 } else { 0.0 },
        avg_rtf: if rtf_n > 0 { rtf_sum / rtf_n as f64 } else { 0.0 },
        per_clip,
    })
}

/// Run the diffusion suite (MINT-DIFF-01) end-to-end against `model_name`: for
/// each entry in [`crate::intake::newcats::diffusion::USE_CASES`], run one
/// generation through [`crate::intake::infer::infer_with_metrics`] (which
/// routes a `kind == "daemon"`-tagged model onto the dgem daemon path, see
/// `infer::diffusion_infer`), derive a [`crate::intake::newcats::diffusion::DiffusionOutcome`]
/// from the normalized [`crate::intake::infer::InferMetrics`], and write both
/// the use-case QUALITY and PERFORMANCE rows via
/// [`crate::intake::newcats::diffusion::score_and_write`].
///
/// Creates its own `model_profiles` row (provider `"daemon"`, distinct from
/// the Ollama suites' `"ollama"`) since a diffusion model never goes through
/// [`run_context_suite`]. A per-use-case generation error is recorded (quality
/// `0.0`, performance rows from whatever timing is available) rather than
/// aborting the whole suite — matches every other `newcats` category's
/// "failure is still useful signal" convention.
pub async fn run_diffusion_suite(model_name: &str) -> Result<DiffusionSuiteOutcome, ToolError> {
    use crate::intake::assistant::{BackendTag, ModelId};
    use crate::intake::infer::infer_with_metrics;
    use crate::intake::newcats::diffusion::{self, DiffusionOutcome};

    let profile_id = create_profile_row_for_provider(model_name, "daemon").await?;
    let pool = storage::get_pool().await?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(900))
        .build()
        .map_err(|e| ToolError::Http(e.to_string()))?;
    let model_id = ModelId::from(model_name);
    // FK fix (S125): score rows FK to `assistant_profile_run(id)`, not
    // `model_profiles(id)`. Same class as the eight S125 suite drivers — create
    // the run parent once, score against it.
    let run_id = crate::intake::assistant::schema::insert_run(&pool).await?;

    let mut per_use_case = Vec::with_capacity(diffusion::USE_CASES.len());
    let mut quality_sum = 0.0;
    let mut time_sum = 0.0;
    let mut n = 0usize;

    for use_case in diffusion::USE_CASES {
        let metrics = infer_with_metrics(&client, model_name, use_case.prompt, Duration::from_secs(600)).await;
        let backend_tag = metrics
            .hardware
            .as_deref()
            .and_then(BackendTag::parse)
            .unwrap_or(BackendTag::Gpu);
        let outcome = DiffusionOutcome {
            output: metrics.response.clone(),
            time_to_output_ms: metrics.total_time_ms.unwrap_or(0) as i64,
            vram_peak_mb: metrics.vram_mb,
            blocks: metrics.blocks,
        };
        let quality = diffusion::quality_score(&outcome.output, use_case.reference);
        quality_sum += quality;
        time_sum += outcome.time_to_output_ms as f64;
        n += 1;

        diffusion::score_and_write(&pool, run_id, model_id.clone(), backend_tag, use_case, &outcome).await?;

        per_use_case.push(if let Some(err) = &metrics.error {
            format!("{}: error ({err})", use_case.label)
        } else {
            format!(
                "{}: quality={quality:.2} time_ms={} vram_mb={}",
                use_case.label,
                outcome.time_to_output_ms,
                outcome.vram_peak_mb.map(|v| v.to_string()).unwrap_or_else(|| "n/a".into()),
            )
        });
    }

    Ok(DiffusionSuiteOutcome {
        profile_id,
        use_cases_run: n,
        avg_use_case_success: if n > 0 { quality_sum / n as f64 } else { 0.0 },
        avg_time_to_output_ms: if n > 0 { time_sum / n as f64 } else { 0.0 },
        per_use_case,
    })
}

/// Outcome of the embedding-retrieval suite (SUITE-EMB, TERM #508) for the tool
/// return summary.
pub struct EmbeddingRetrievalSuiteOutcome {
    pub profile_id: uuid::Uuid,
    /// Human one-liner (metrics, or the skip reason for a non-embedding model).
    pub summary: String,
    /// True when the candidate is not an embedding model (clean skip, no rows).
    pub skipped: bool,
}

/// Run the embedding-retrieval suite (SUITE-EMB) end-to-end against `model_name`:
/// load the public (+ optional domain) corpus from `INTAKE_CORPUS_DIR`, embed
/// every doc/query through Chord's `/v1/embeddings` route via the production
/// [`crate::intake::assistant::dim6_embeddings::ChordEmbedder`] (which calls
/// [`crate::intake::infer::embed_with_metrics`]'s `openai_embed` arm — bearer from
/// the backend's `api_key_env`, never logged), score precision/recall/MRR/nDCG +
/// dimensionality + throughput + the public-vs-domain delta, and write every row
/// via [`crate::intake::newcats::embedding_retrieval::score_and_write`].
///
/// Creates its own `model_profiles` row (provider `"ollama"` — embedding models
/// resolve through the Ollama/OpenAI-compatible embeddings path, never the dgem
/// daemon). A candidate that is not an embedding model is a CLEAN SKIP (no rows),
/// never an error, matching every other suite's "failure is still signal" stance.
pub async fn run_embedding_retrieval_suite(
    model_name: &str,
) -> Result<EmbeddingRetrievalSuiteOutcome, ToolError> {
    use crate::intake::assistant::dim6_embeddings::ChordEmbedder;
    use crate::intake::assistant::{BackendTag, ModelId};
    use crate::intake::newcats::embedding_retrieval as er;

    let profile_id = create_profile_row(model_name).await?;
    let pool = storage::get_pool().await?;
    let (public, domain) = er::load_corpora()?;

    // Backend resolution (GPU vs CPU serve) happens inside the unified embed path;
    // the tag here keys the stored rows. Embedding serves are GPU-first in this
    // fleet, so GPU is the default attribution (a future refinement can thread the
    // observed `EmbedMetrics::hardware` through the embedder).
    let embedder = ChordEmbedder::new(ModelId::from(model_name), BackendTag::Gpu);
    // FK fix (S125): score rows FK to `assistant_profile_run(id)`, not
    // `model_profiles(id)`. Create the run parent once, then write against it.
    let run_id = crate::intake::assistant::schema::insert_run(&pool).await?;
    let summary = er::score_and_write(&pool, run_id, &embedder, &public, domain.as_ref()).await?;

    Ok(EmbeddingRetrievalSuiteOutcome {
        profile_id,
        summary: summary.line(),
        skipped: summary.skipped.is_some(),
    })
}

/// Per-scenario tool-routing inference timeout (shares the agent suite's env var
/// + 180s default, since it exercises the same corpus / tool-calling shape).
fn tool_routing_timeout() -> Duration {
    super::timeouts::env_timeout("INTAKE_AGENT_TIMEOUT_SEC", 180)
}

/// The advertised tool-catalog size used for the tool-routing suite: large
/// enough that decoy rejection + correct-tool@1 are a real discrimination (many
/// plausible wrong tools alongside the right one), but bounded so the suite stays
/// fast. The `agent` suite sweeps 10/50/100/200 bands to measure DEGRADATION;
/// the routing suite instead scores a single representative band per scenario.
const TOOL_ROUTING_BAND: usize = 50;

/// Outcome of the tool-routing suite (S125 SUITE-TOOL) for the tool summary.
pub struct ToolRoutingSuiteOutcome {
    pub profile_id: uuid::Uuid,
    pub scenarios_run: usize,
    pub rows_written: usize,
    /// Per-metric mean over the scenarios that scored it (`None` when none did).
    pub correct_tool_at_1: Option<f64>,
    pub parameter_validity: Option<f64>,
    pub decoy_rejection: Option<f64>,
    pub multi_step_success: Option<f64>,
    /// Scenarios skipped because inference errored (not scored, not fabricated).
    pub errored: usize,
}

/// Run the tool-routing suite (S125 SUITE-TOOL / TERM-511) against `model_name`,
/// reusing an existing `model_profiles` row (`profile_id`, as the `agent`/`code`
/// suites do). Generalizes the `agent` suite's tool-selection/multi-step path
/// into a first-class profiler that routes through Chord's OpenAI-compatible
/// `/v1/chat/completions` `tools` endpoint
/// ([`crate::intake::infer::tool_infer_with_metrics`]) and writes discrete
/// per-scenario `assistant_dimension_score` rows tagged `task_category =
/// "tool_routing"` via
/// [`crate::intake::newcats::tool_routing::score_and_write`].
///
/// Reuses [`crate::intake::agent`]'s scenario loader + tool-catalog builder +
/// multi-step scorer verbatim, so the corpus and catalog have one source of
/// truth and the legacy `agent` suite is untouched. Only the `tool_selection`
/// and `multi_step` scenario categories are routing-relevant; the rest
/// (instruction/hallucination/personality) stay with the `agent` suite and are
/// filtered out here. A per-scenario inference error is recorded and SKIPPED
/// (not scored `0.0`), matching every other suite's "a failed case is not a
/// fabricated zero" convention.
pub async fn run_tool_routing_suite(
    model_name: &str,
    profile_id: uuid::Uuid,
    limit: Option<usize>,
) -> Result<ToolRoutingSuiteOutcome, ToolError> {
    use crate::intake::agent;
    use crate::intake::assistant::{BackendTag, ModelId};
    use crate::intake::infer::tool_infer_with_metrics;
    use crate::intake::newcats::tool_routing::{self, RoutingOutcome};

    let dir = crate::intake::code::corpus_dir()?;
    let mut scenarios = agent::read_scenarios(&dir)?;
    scenarios.retain(|s| s.category == "tool_selection" || s.category == "multi_step");
    if let Some(n) = limit {
        scenarios.truncate(n);
    }
    if scenarios.is_empty() {
        return Err(ToolError::NotConfigured(
            "no tool_selection/multi_step scenarios found for tool_routing suite".into(),
        ));
    }

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(900))
        .build()
        .map_err(|e| ToolError::Http(format!("client build failed: {e}")))?;
    let pool = storage::get_pool().await?;
    let model_id = ModelId::from(model_name);
    let timeout = tool_routing_timeout();
    // FK fix (S125): the `profile_id` PARAM is a `model_profiles(id)`; score rows
    // FK to `assistant_profile_run(id)`. Create the run parent once per invocation
    // and score every scenario against it (profile_id stays for the outcome).
    let run_id = crate::intake::assistant::schema::insert_run(&pool).await?;

    // Per-metric tallies (sum, count) so the summary reports means without a
    // re-query. Order: correct_tool@1, param_validity, decoy_reject, multi_step.
    let mut tally: std::collections::BTreeMap<String, (f64, usize)> = std::collections::BTreeMap::new();
    let mut rows_written = 0usize;
    let mut errored = 0usize;

    for sc in &scenarios {
        let catalog = if sc.category == "multi_step" {
            agent::build_catalog(TOOL_ROUTING_BAND, &sc.expected_tools)
        } else {
            let required: Vec<String> = sc.expected_tool.iter().cloned().collect();
            agent::build_catalog(TOOL_ROUTING_BAND, &required)
        };

        let metrics = tool_infer_with_metrics(&client, model_name, &sc.prompt, &catalog, timeout).await;
        if metrics.error.is_some() {
            errored += 1;
            continue;
        }
        let backend_tag = metrics
            .hardware
            .as_deref()
            .and_then(BackendTag::parse)
            .unwrap_or(BackendTag::Gpu);
        let outcome = RoutingOutcome {
            tool_calls: metrics.tool_calls.clone(),
            error: None,
        };

        // Tally from the same pure rows we persist (one source of truth).
        for score in tool_routing::build_scores(model_id.clone(), backend_tag, sc, &outcome) {
            let e = tally.entry(score.metric.clone()).or_insert((0.0, 0));
            e.0 += score.value;
            e.1 += 1;
        }
        rows_written += tool_routing::score_and_write(&pool, run_id, model_id.clone(), backend_tag, sc, &outcome).await?;
    }

    let mean = |m: &str| tally.get(m).and_then(|(s, n)| if *n > 0 { Some(*s / *n as f64) } else { None });

    Ok(ToolRoutingSuiteOutcome {
        profile_id,
        scenarios_run: scenarios.len(),
        rows_written,
        correct_tool_at_1: mean(tool_routing::METRIC_CORRECT_TOOL),
        parameter_validity: mean(tool_routing::METRIC_PARAM_VALIDITY),
        decoy_rejection: mean(tool_routing::METRIC_DECOY_REJECT),
        multi_step_success: mean(tool_routing::METRIC_MULTI_STEP),
        errored,
    })
}

/// Outcome of the vision-QA suite (SUITE-VQA) for the tool return summary.
pub struct VisionQaSuiteOutcome {
    pub profile_id: uuid::Uuid,
    pub items_run: usize,
    /// Mean lenient-match accuracy in `[0.0, 1.0]`.
    pub accuracy: f64,
    /// Fraction of confident-but-wrong answers in `[0.0, 1.0]`.
    pub hallucination_rate: f64,
    pub avg_latency_ms: f64,
    /// One-line-per-item summary (manifest order).
    pub per_item: Vec<String>,
}

/// Run the vision-QA suite (SUITE-VQA) end-to-end against `model_name`: load the
/// image+question+reference-answer corpus from `INTAKE_CORPUS_DIR` (via
/// [`crate::intake::code::corpus_dir`], the unified resolver), and for each item
/// send the image (as a base64 `data:` URL image content part) + question to
/// Chord's `/v1/chat/completions` route through
/// [`crate::intake::infer::vision_infer_with_metrics`], derive a
/// [`crate::intake::newcats::image_parsing::VisionQaOutcome`] from the normalized
/// [`crate::intake::infer::InferMetrics`], and write the SUITE-VQA metric rows
/// (accuracy / caption similarity / hallucination / latency / VRAM) via
/// [`crate::intake::newcats::image_parsing::score_and_write_vqa`].
///
/// The VLM (e.g. `llava:7b`) runs on Ollama under Chord, so the profile row uses
/// the default `"ollama"` provider (unlike diffusion's `"daemon"`). A per-item
/// backend/transport error is recorded (empty answer ⇒ scored as a miss, latency
/// from whatever timing exists) rather than aborting the suite — the same
/// "failure is still useful signal" convention every other newcats suite uses.
/// An unreadable image is skipped with a note (no fabricated row).
pub async fn run_vision_qa_suite(model_name: &str) -> Result<VisionQaSuiteOutcome, ToolError> {
    use crate::intake::assistant::{BackendTag, ModelId};
    use crate::intake::infer::vision_infer_with_metrics;
    use crate::intake::newcats::image_parsing::{self, VisionQaOutcome};

    // Unified corpus resolver (DR-02): INTAKE_CORPUS_DIR points at the vision_qa
    // corpus dir (manifest.json + images). Missing var ⇒ clean NotConfigured.
    let corpus_dir = crate::intake::code::corpus_dir()?;
    let items = image_parsing::load_vision_qa_manifest(&corpus_dir)?;
    if items.is_empty() {
        return Err(ToolError::NotConfigured(
            "vision_qa manifest is empty (no items to profile)".into(),
        ));
    }

    let profile_id = create_profile_row(model_name).await?;
    let pool = storage::get_pool().await?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(900))
        .build()
        .map_err(|e| ToolError::Http(e.to_string()))?;
    let model_id = ModelId::from(model_name);
    // FK fix (S125): score rows FK to `assistant_profile_run(id)`, not
    // `model_profiles(id)`. Create the run parent once, score against it.
    let run_id = crate::intake::assistant::schema::insert_run(&pool).await?;

    let mut per_item = Vec::with_capacity(items.len());
    let mut acc_sum = 0.0;
    let mut hall_sum = 0.0;
    let mut lat_sum = 0.0;
    let mut n = 0usize;

    for item in &items {
        let img_path = corpus_dir.join(&item.image_file);
        let bytes = match std::fs::read(&img_path) {
            Ok(b) => b,
            Err(e) => {
                per_item.push(format!("{}: image unreadable ({e})", item.image_file));
                continue;
            }
        };
        let data_url = image_parsing::to_data_url(&item.image_file, &bytes);
        let metrics =
            vision_infer_with_metrics(&client, model_name, &item.question, &data_url, Duration::from_secs(600)).await;
        let backend_tag = metrics
            .hardware
            .as_deref()
            .and_then(BackendTag::parse)
            .unwrap_or(BackendTag::Gpu);
        let outcome = VisionQaOutcome {
            answer: metrics.response.clone(),
            latency_ms: metrics.total_time_ms.unwrap_or(0) as i64,
            vram_peak_mb: metrics.vram_mb,
        };

        let accurate = image_parsing::lenient_match(&outcome.answer, &item.answer);
        let hallucinated = image_parsing::is_hallucination(&outcome.answer, &item.answer);
        acc_sum += if accurate { 1.0 } else { 0.0 };
        hall_sum += if hallucinated { 1.0 } else { 0.0 };
        lat_sum += outcome.latency_ms as f64;
        n += 1;

        image_parsing::score_and_write_vqa(&pool, run_id, model_id.clone(), backend_tag, item, &outcome).await?;

        per_item.push(if let Some(err) = &metrics.error {
            format!("{}: error ({err})", item.image_file)
        } else {
            format!(
                "{}: acc={accurate} answer={:?} lat_ms={}",
                item.image_file, outcome.answer, outcome.latency_ms,
            )
        });
    }

    Ok(VisionQaSuiteOutcome {
        profile_id,
        items_run: n,
        accuracy: if n > 0 { acc_sum / n as f64 } else { 0.0 },
        hallucination_rate: if n > 0 { hall_sum / n as f64 } else { 0.0 },
        avg_latency_ms: if n > 0 { lat_sum / n as f64 } else { 0.0 },
        per_item,
    })
}

/// Outcome of the reranking suite (SUITE-RRK) for the tool return summary.
pub struct RerankingSuiteOutcome {
    pub profile_id: uuid::Uuid,
    pub queries_run: usize,
    pub avg_ndcg_uplift: f64,
    pub avg_reranked_ndcg: f64,
    pub avg_latency_ms: f64,
    /// One-line-per-query summary, in corpus order.
    pub per_query: Vec<String>,
}

/// Run the reranking suite (SUITE-RRK) end-to-end against `model_name`: load the
/// reranking corpus (`INTAKE_CORPUS_DIR/reranking.json`), and for each query run
/// one rerank through [`crate::intake::infer::rerank_with_metrics`] (which routes
/// an `openai`-tagged model onto Chord's `/v1/rerank`, backed by
/// bge-reranker-v2-m3), derive a
/// [`crate::intake::newcats::reranking::RerankOutcome`] from the normalized
/// [`crate::intake::infer::RerankMetrics`], and write the nDCG-uplift / nDCG /
/// latency rows via [`crate::intake::newcats::reranking::score_and_write`].
///
/// Creates its own `model_profiles` row (provider `"openai"`, since a reranker
/// runs on Chord's OpenAI-compatible route, not Ollama, and never goes through
/// [`run_context_suite`]). A per-query rerank error is skipped (recorded in the
/// summary) rather than aborting the whole suite — matching every other
/// `newcats` category's "failure is still useful signal" convention. A missing
/// corpus fails the whole suite up front (a `ToolError`), since without it there
/// is nothing to score.
pub async fn run_reranking_suite(model_name: &str) -> Result<RerankingSuiteOutcome, ToolError> {
    use crate::intake::assistant::{BackendTag, ModelId};
    use crate::intake::infer::rerank_with_metrics;
    use crate::intake::newcats::reranking::{self, RerankOutcome};

    let corpus = reranking::load_corpus()?;
    let profile_id = create_profile_row_for_provider(model_name, "openai").await?;
    let pool = storage::get_pool().await?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(900))
        .build()
        .map_err(|e| ToolError::Http(e.to_string()))?;
    let model_id = ModelId::from(model_name);
    // FK fix (S125): score rows FK to `assistant_profile_run(id)`, not
    // `model_profiles(id)`. Create the run parent once, score against it.
    let run_id = crate::intake::assistant::schema::insert_run(&pool).await?;

    let mut per_query = Vec::with_capacity(corpus.len());
    let mut uplift_sum = 0.0;
    let mut ndcg_sum = 0.0;
    let mut latency_sum = 0.0;
    let mut n = 0usize;

    for query in &corpus {
        let metrics = rerank_with_metrics(
            &client,
            model_name,
            &query.query,
            &query.passages,
            Duration::from_secs(120),
        )
        .await;
        let backend_tag = metrics
            .hardware
            .as_deref()
            .and_then(BackendTag::parse)
            .unwrap_or(BackendTag::Cpu);

        if let Some(err) = &metrics.error {
            per_query.push(format!("{}: error ({err})", query.query_id));
            continue;
        }

        let outcome = RerankOutcome {
            reranked_order: metrics.ranking.clone(),
            latency_ms: metrics.latency_ms,
        };
        let reranked_ndcg =
            reranking::ndcg_at_k(&outcome.reranked_order, &query.relevance, reranking::DEFAULT_K);
        let baseline_ndcg =
            reranking::ndcg_at_k(&query.baseline_order, &query.relevance, reranking::DEFAULT_K);
        let uplift = reranked_ndcg - baseline_ndcg;
        uplift_sum += uplift;
        ndcg_sum += reranked_ndcg;
        latency_sum += outcome.latency_ms as f64;
        n += 1;

        reranking::score_and_write(&pool, run_id, model_id.clone(), backend_tag, query, &outcome)
            .await?;

        per_query.push(format!(
            "{}: uplift={uplift:.3} ndcg={reranked_ndcg:.3} latency_ms={}",
            query.query_id, outcome.latency_ms,
        ));
    }

    Ok(RerankingSuiteOutcome {
        profile_id,
        queries_run: n,
        avg_ndcg_uplift: if n > 0 { uplift_sum / n as f64 } else { 0.0 },
        avg_reranked_ndcg: if n > 0 { ndcg_sum / n as f64 } else { 0.0 },
        avg_latency_ms: if n > 0 { latency_sum / n as f64 } else { 0.0 },
        per_query,
    })
}

/// Outcome of the image-generation suite (SUITE-IMG) for the tool return summary.
pub struct ImageGenSuiteOutcome {
    pub profile_id: uuid::Uuid,
    pub prompts_run: usize,
    pub success_count: usize,
    pub avg_time_to_image_ms: f64,
    /// One-line-per-prompt summary, in corpus order.
    pub per_prompt: Vec<String>,
}

/// Run the image-generation suite (SUITE-IMG) end-to-end against `model_name`:
/// for each prompt in the corpus ([`crate::intake::newcats::image_generation::load_prompts`],
/// `INTAKE_CORPUS_DIR/image_generation.json` with an in-source default set), run
/// one generation through [`crate::intake::infer::imagegen_with_metrics`] (which
/// routes an `openai`-kind model onto Chord's `/v1/images/generations` route —
/// sd-turbo diffusers behind Chord), derive a
/// [`crate::intake::newcats::image_generation::GenerationOutcome`] from the
/// normalized [`crate::intake::infer::ImageGenMetrics`], and write its success /
/// time-to-image / VRAM rows via
/// [`crate::intake::newcats::image_generation::score_and_write`].
///
/// Mirrors [`run_diffusion_suite`]: creates its own `model_profiles` row
/// (provider `"openai"`, since an image-generation backend never goes through
/// [`run_context_suite`]), and a per-prompt generation error is recorded
/// (`success = false`, whatever timing/VRAM is available) rather than aborting
/// the whole suite — the same "failure is still useful signal" convention every
/// other `newcats` category follows. CLIP prompt-adherence is left NOT MEASURED
/// (`clip_score = None`) — no CLIP scorer is wired on this box (scaffolded).
pub async fn run_image_generation_suite(model_name: &str) -> Result<ImageGenSuiteOutcome, ToolError> {
    use crate::intake::assistant::{BackendTag, ModelId};
    use crate::intake::infer::imagegen_with_metrics;
    use crate::intake::newcats::image_generation::{self, GenerationOutcome};

    let profile_id = create_profile_row_for_provider(model_name, "openai").await?;
    let pool = storage::get_pool().await?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(900))
        .build()
        .map_err(|e| ToolError::Http(e.to_string()))?;
    let model_id = ModelId::from(model_name);
    // FK fix (S125): score rows FK to `assistant_profile_run(id)`, not
    // `model_profiles(id)`. Create the run parent once, score against it.
    let run_id = crate::intake::assistant::schema::insert_run(&pool).await?;
    let prompts = image_generation::load_prompts();

    let mut per_prompt = Vec::with_capacity(prompts.len());
    let mut time_sum = 0.0;
    let mut success_count = 0usize;
    let mut n = 0usize;

    for p in &prompts {
        let metrics = imagegen_with_metrics(&client, model_name, &p.prompt, Duration::from_secs(600)).await;
        let backend_tag = metrics
            .hardware
            .as_deref()
            .and_then(BackendTag::parse)
            .unwrap_or(BackendTag::Gpu);
        let outcome = GenerationOutcome {
            success: metrics.success,
            time_to_image_ms: metrics.time_to_image_ms,
            // `None` VRAM is recorded as 0 in the row (the field is non-optional);
            // the metric doc notes a 0 here means "unreadable", not "measured 0".
            vram_peak_mb: metrics.vram_peak_mb.unwrap_or(0),
            failure_reason: metrics.error.clone(),
            // CLIP not measured on this box (no CLIP scorer wired) — scaffolded.
            clip_score: None,
        };
        time_sum += outcome.time_to_image_ms as f64;
        if outcome.success {
            success_count += 1;
        }
        n += 1;

        image_generation::score_and_write(&pool, run_id, model_id.clone(), backend_tag, &outcome).await?;

        per_prompt.push(if let Some(err) = &metrics.error {
            format!("{}: error ({err})", p.label)
        } else {
            format!(
                "{}: success={} time_ms={} vram_mb={}",
                p.label, outcome.success, outcome.time_to_image_ms, outcome.vram_peak_mb,
            )
        });
    }

    Ok(ImageGenSuiteOutcome {
        profile_id,
        prompts_run: n,
        success_count,
        avg_time_to_image_ms: if n > 0 { time_sum / n as f64 } else { 0.0 },
        per_prompt,
    })
}

/// Outcome of the document_parsing suite (SUITE-DOC) for the tool summary.
pub struct DocParseSuiteOutcome {
    pub profile_id: uuid::Uuid,
    pub cases_run: usize,
    pub avg_field_accuracy: f64,
    pub avg_latency_ms: f64,
    /// One line per corpus case, in manifest order.
    pub per_case: Vec<String>,
}

/// Run the document_parsing suite (SUITE-DOC) end-to-end against `model_name`:
/// load the corpus from `INTAKE_CORPUS_DIR/document_parsing/` (see
/// [`crate::intake::newcats::document_parsing::load_corpus`]), and for each case
/// POST its document bytes to Chord's `/v1/documents/parse` via
/// [`crate::intake::infer::docparse_with_metrics`] (the `openai` arm), derive an
/// [`crate::intake::newcats::document_parsing::ExtractionOutcome`] from the
/// normalized [`crate::intake::infer::DocParseMetrics`], and write the
/// field-accuracy / CER / WER / table-F1 rows via that module's `score_and_write`.
///
/// Creates its own `model_profiles` row (provider `"chord"`, since a doc-parse
/// model is served through Chord, not Ollama). A per-case parse error is
/// recorded (a zero-score row from whatever timing is available) rather than
/// aborting the whole suite — the same "failure is still useful signal"
/// convention every other `newcats` category uses. An unset `INTAKE_CORPUS_DIR`
/// (or missing manifest) returns `ToolError::NotConfigured` — the caller decides
/// whether that is a hard error or a clean skip.
pub async fn run_document_parsing_suite(model_name: &str) -> Result<DocParseSuiteOutcome, ToolError> {
    use crate::intake::assistant::{BackendTag, ModelId};
    use crate::intake::infer::docparse_with_metrics;
    use crate::intake::newcats::document_parsing::{self, ExtractionOutcome};

    let (corpus_dir, cases) = document_parsing::load_corpus()?;
    let profile_id = create_profile_row_for_provider(model_name, "chord").await?;
    let pool = storage::get_pool().await?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(900))
        .build()
        .map_err(|e| ToolError::Http(e.to_string()))?;
    let model_id = ModelId::from(model_name);
    // FK fix (S125): score rows FK to `assistant_profile_run(id)`, not
    // `model_profiles(id)`. Create the run parent once, score against it.
    let run_id = crate::intake::assistant::schema::insert_run(&pool).await?;

    let mut per_case = Vec::with_capacity(cases.len());
    let mut accuracy_sum = 0.0;
    let mut latency_sum = 0.0;
    let mut n = 0usize;

    for case in &cases {
        let path = corpus_dir.join(&case.file);
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) => {
                per_case.push(format!("{}: error reading {} ({e})", case.id, path.display()));
                continue;
            }
        };
        let metrics =
            docparse_with_metrics(&client, model_name, &bytes, &case.file, Duration::from_secs(600))
                .await;
        let backend_tag = metrics
            .hardware
            .as_deref()
            .and_then(BackendTag::parse)
            .unwrap_or(BackendTag::Gpu);
        let truth = case.ground_truth();
        let outcome = ExtractionOutcome {
            raw_output: String::new(),
            text: metrics.text.clone(),
            fields: metrics.fields.clone(),
            tables: metrics.tables.clone(),
            latency_ms: metrics.latency_ms,
            response_tokens: metrics.response_tokens,
        };
        let accuracy = document_parsing::score_field_accuracy(
            &truth.fields,
            &document_parsing::extracted_fields(&outcome),
        );
        accuracy_sum += accuracy;
        latency_sum += outcome.latency_ms as f64;
        n += 1;

        document_parsing::score_and_write(&pool, run_id, model_id.clone(), backend_tag, &truth, &outcome)
            .await?;

        per_case.push(if let Some(err) = &metrics.error {
            format!("{}: error ({err})", case.id)
        } else {
            format!(
                "{}: field_acc={accuracy:.2} latency_ms={} tables={}",
                case.id,
                outcome.latency_ms,
                outcome.tables.len(),
            )
        });
    }

    Ok(DocParseSuiteOutcome {
        profile_id,
        cases_run: n,
        avg_field_accuracy: if n > 0 { accuracy_sum / n as f64 } else { 0.0 },
        avg_latency_ms: if n > 0 { latency_sum / n as f64 } else { 0.0 },
        per_case,
    })
}

/// Outcome of the TTS suite (S125 SUITE-TTS) for the tool return summary.
pub struct TtsSuiteOutcome {
    pub profile_id: uuid::Uuid,
    pub cases_run: usize,
    pub avg_loopback_wer: f64,
    pub avg_rtf: Option<f64>,
    /// One-line-per-case summary, in [`crate::intake::newcats::tts::load_cases`] order.
    pub per_case: Vec<String>,
}

/// Run the TTS suite (S125 SUITE-TTS) end-to-end against `model_name`: for each
/// case in [`crate::intake::newcats::tts::load_cases`], synthesize speech through
/// Chord's `/v1/audio/speech` ([`crate::intake::infer::synthesize_with_metrics`]),
/// transcribe the produced audio back through the SHARED STT arm
/// ([`crate::intake::infer::transcribe_with_metrics`] — the SUITE-STT hand-rolled
/// multipart transcribe, deduped at integration), derive a
/// [`crate::intake::newcats::tts::TtsOutcome`], and write the intelligibility
/// (STT-loopback WER + MOS-proxy) and performance (synthesis_ms + RTF) rows via
/// [`crate::intake::newcats::tts::score_and_write`].
///
/// The STT loopback model is resolved from `TTS_LOOPBACK_STT_MODEL` (a model
/// name / registry key only — never a host/IP; defaults to `"faster-whisper"`),
/// and the synthesis voice from `TTS_VOICE` (default `"en_US-lessac-medium"`).
///
/// Creates its own `model_profiles` row (provider `"openai"`, matching the Chord
/// OpenAI-compatible backend kind these routes speak). A per-case failure is
/// recorded (skipped from the averages) rather than aborting the whole suite —
/// matches every other `newcats` category's "failure is still useful signal"
/// convention.
pub async fn run_tts_suite(model_name: &str) -> Result<TtsSuiteOutcome, ToolError> {
    use crate::intake::assistant::{BackendTag, ModelId};
    use crate::intake::infer::{synthesize_with_metrics, transcribe_with_metrics};
    use crate::intake::newcats::tts::{self, TtsOutcome};

    let profile_id = create_profile_row_for_provider(model_name, "openai").await?;
    let pool = storage::get_pool().await?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(900))
        .build()
        .map_err(|e| ToolError::Http(e.to_string()))?;
    let model_id = ModelId::from(model_name);

    // Loopback STT model + synthesis voice: names only, from env (no host/IP).
    let stt_model = std::env::var("TTS_LOOPBACK_STT_MODEL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "faster-whisper".to_string());
    let voice = std::env::var("TTS_VOICE")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "en_US-lessac-medium".to_string());

    // FK fix (S125): score rows FK to `assistant_profile_run(id)`, not
    // `model_profiles(id)`. Create the run parent once, score against it.
    let run_id = crate::intake::assistant::schema::insert_run(&pool).await?;

    let cases = tts::load_cases();
    let mut per_case = Vec::with_capacity(cases.len());
    let mut wer_sum = 0.0;
    let mut rtf_sum = 0.0;
    let mut rtf_n = 0usize;
    let mut n = 0usize;

    for case in &cases {
        // 1) Synthesize.
        let speech =
            synthesize_with_metrics(&client, model_name, &case.text, &voice, Duration::from_secs(120)).await;
        if let Some(err) = &speech.error {
            per_case.push(format!("{}: synth error ({err})", case.label));
            continue;
        }
        let backend_tag = speech
            .hardware
            .as_deref()
            .and_then(BackendTag::parse)
            .unwrap_or(BackendTag::Gpu);

        // 2) Transcribe the produced audio (STT loopback) — via the SHARED
        // SUITE-STT multipart transcribe (deduped). A `filename` is required by
        // that arm's multipart shape; the synthesized audio is WAV.
        let stt =
            transcribe_with_metrics(&client, &stt_model, &speech.audio, "tts-loopback.wav", Duration::from_secs(120)).await;
        if let Some(err) = &stt.error {
            per_case.push(format!("{}: stt-loopback error ({err})", case.label));
            continue;
        }

        // 3) Derive outcome (duration/MOS from the WAV) and score.
        let outcome = TtsOutcome::from_audio(stt.transcript.clone(), speech.latency_ms, &speech.audio);
        let wer = tts::loopback_wer(&outcome.loopback_transcript, &case.text);
        let rtf = tts::real_time_factor(outcome.synthesis_ms, outcome.audio_duration_s);
        wer_sum += wer;
        if let Some(r) = rtf {
            rtf_sum += r;
            rtf_n += 1;
        }
        n += 1;

        tts::score_and_write(&pool, run_id, model_id.clone(), backend_tag, case, &outcome).await?;

        per_case.push(format!(
            "{}: wer={wer:.2} synth_ms={} rtf={} mos={}",
            case.label,
            outcome.synthesis_ms,
            rtf.map(|r| format!("{r:.2}")).unwrap_or_else(|| "n/a".into()),
            outcome.mos_proxy.map(|v| format!("{v:.2}")).unwrap_or_else(|| "n/a".into()),
        ));
    }

    Ok(TtsSuiteOutcome {
        profile_id,
        cases_run: n,
        avg_loopback_wer: if n > 0 { wer_sum / n as f64 } else { 0.0 },
        avg_rtf: if rtf_n > 0 { Some(rtf_sum / rtf_n as f64) } else { None },
        per_case,
    })
}

/// Disk-usage percentage for a mount via `df`, or None if it can't be read.
fn disk_pct(mount: &str) -> Option<u8> {
    let out = std::process::Command::new("df")
        .args(["--output=pcent", mount])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .nth(1)
        .and_then(|l| l.trim().trim_end_matches('%').parse::<u8>().ok())
}

/// Pre-model disk-pressure gate. Returns `Some(reason)` when a critical mount is
/// at/over its threshold (root 85%, data 90%) so the caller skips the model
/// instead of failing mid-run — or silently corrupting rows — on a full disk.
fn disk_pressure() -> Option<String> {
    for (mount, thresh) in [("/", 85u8), ("/opt/chord-data", 90u8)] {
        if let Some(p) = disk_pct(mount) {
            if p >= thresh {
                return Some(format!("{mount} at {p}% (threshold {thresh}%)"));
            }
        }
    }
    None
}

/// BT-03: whether the fleet warm path should issue the Ollama `load_model`
/// pre-warm for a model of the given backend `kind`. ONLY true `ollama`-kind
/// models go through the Ollama control API; every other kind (`openai`/
/// `daemon`/`llama-server`) is a Chord-/registry-served model brought up by its
/// own [`crate::intake::lifecycle::ensure_up`] on the FIRST request, so the fleet
/// path must NOT send it to the Ollama load gate. Sending it there silently
/// skipped every Chord-served suite (VLM/rerank/stt/tts/doc/image/embedding) in a
/// fleet sweep — the exact BT-03 defect. Pure.
fn needs_ollama_warm(backend_kind: &str) -> bool {
    backend_kind == "ollama"
}

/// DR-01: run an async warm step with a bounded retry + fixed backoff before
/// giving up (the warm was previously one-shot). `attempts` is clamped to ≥1;
/// between failed attempts it sleeps `backoff` — pass `Duration::ZERO` in tests
/// to keep them clock-free (no `Instant`/wall-time flakiness). Returns the LAST
/// error if every attempt fails. Generic over the warm closure so it is
/// unit-testable with a mock that never touches the network.
async fn warm_with_backoff<F, Fut>(
    attempts: usize,
    backoff: Duration,
    mut warm: F,
) -> Result<(), ToolError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<(), ToolError>>,
{
    let attempts = attempts.max(1);
    let mut last: Option<ToolError> = None;
    for i in 0..attempts {
        match warm().await {
            Ok(()) => return Ok(()),
            Err(e) => {
                last = Some(e);
                if i + 1 < attempts && !backoff.is_zero() {
                    tokio::time::sleep(backoff).await;
                }
            }
        }
    }
    Err(last.unwrap_or_else(|| ToolError::Http("warm failed with no error captured".into())))
}

/// Fleet run that picks suites PER MODEL by purpose (or an explicit override),
/// with the overnight VRAM lifecycle. `resolve_suites` maps a model name to its
/// suite list; `resolve_langs` maps a model name to its code-suite languages;
/// `is_daemon` flags non-Ollama daemon models to skip.
///
/// For each model: load → run its suites (context first, sharing the profile id
/// with code/agent) → evict. The daily driver is restored once at the end.
#[allow(clippy::too_many_arguments)]
pub async fn run_fleet_suites(
    models: &[String],
    tiers: &[usize],
    resolve_suites: impl Fn(&str) -> Vec<String>,
    resolve_langs: impl Fn(&str) -> Vec<String>,
    is_daemon: impl Fn(&str) -> bool,
    run_code: impl Fn(String, Vec<String>, uuid::Uuid) -> futures_box::CodeFut,
    run_agent: impl Fn(String, uuid::Uuid) -> futures_box::AgentFut,
    mut on_progress: impl FnMut(usize, usize, &str, &str),
) -> Vec<FleetSuiteResult> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(900))
        .build()
        .expect("client");
    let prior_hot = current_hot_model(&client).await;
    let mut out = Vec::new();
    let total = models.len();

    for (i, model) in models.iter().enumerate() {
        let suites = resolve_suites(model);
        // BLD-ASYNC: report per-model progress BEFORE starting this model, so a
        // poller sees "which model is in flight" rather than only completed
        // counts. `on_progress` is caller-supplied (a no-op for the synchronous
        // path, a job-registry update for the async path) — never touches I/O
        // itself, so it can't turn a fast/pure sweep into a slow one.
        on_progress(i, total, model, &suites.join("+"));
        // DR-01: CONVERGENT sweep — when the caller's `resolve_suites` has already
        // pruned this model's settled (`run`/`non_viable`) cells (via
        // `catalog::pending_suites`) and nothing is left to run, record a clean
        // "converged" skip and move on rather than load a backend and run zero
        // suites. Its cells are left as-is so a later sweep re-checks them.
        if suites.is_empty() {
            out.push(FleetSuiteResult {
                model: model.clone(),
                suites,
                summary: "skipped: no pending suites (coverage already settled)".into(),
                skipped: true,
                capped_suites: Vec::new(),
            });
            continue;
        }
        // Self-heal: skip a model rather than fail the run on a full disk. The
        // cells stay `not_run`, so a later (post-cleanup) sweep RESUMES them.
        if let Some(reason) = disk_pressure() {
            out.push(FleetSuiteResult {
                model: model.clone(),
                suites: suites.clone(),
                summary: format!("skipped: disk pressure — {reason} (left not_run, resumable)"),
                skipped: true,
                capped_suites: Vec::new(),
            });
            continue;
        }
        if is_daemon(model) {
            out.push(FleetSuiteResult {
                model: model.clone(),
                suites,
                summary: "skipped: non-Ollama daemon model (DiffusionGemma/dgem)".into(),
                skipped: true,
                capped_suites: Vec::new(),
            });
            continue;
        }
        // BT-03: resolve the backend ONCE and gate the Ollama pre-warm on its
        // kind. Non-`ollama` (openai/daemon/llama-server) models are Chord-/
        // registry-served and brought up by their own lifecycle on first request
        // — they must NOT go through the Ollama `load_model` gate (that silently
        // skipped every Chord-served suite in a fleet sweep). Only `ollama`-kind
        // models are warmed here; all suites (stt/tts/vqa/rerank/img/doc/embedding)
        // then reach their backend uniformly via the unified dispatch below,
        // replacing the old per-suite "before-the-gate" hacks.
        let backend_kind = crate::intake::infer::resolve_backend(model).kind;
        if needs_ollama_warm(&backend_kind) {
            // DR-01: bounded retry + short backoff before marking a model
            // unavailable (was one-shot). A warm failure caused by disk pressure
            // is a clean, RESUMABLE skip (cells left `not_run`), never a hard
            // abort of the whole sweep.
            let warm =
                warm_with_backoff(3, Duration::from_secs(2), || load_model(&client, model)).await;
            if let Err(e) = warm {
                let summary = match disk_pressure() {
                    Some(reason) => format!(
                        "skipped: disk pressure during warm — {reason} (left not_run, resumable)"
                    ),
                    None => format!("load failed after retries: {e}"),
                };
                out.push(FleetSuiteResult {
                    model: model.clone(),
                    suites,
                    summary,
                    skipped: true,
                    capped_suites: Vec::new(),
                });
                continue;
            }
        }

        let mut profile_id: Option<uuid::Uuid> = None;
        let mut parts: Vec<String> = Vec::new();

        // S125 (SUITE-WALLCLOCK): a per-model, per-suite hard wall-clock ceiling,
        // scaled by model size (huge models get 20/40 min; small models are
        // uncapped → `None`). Computed ONCE here and applied to EACH suite
        // dispatch below via `with_suite_cap`. On a cap the suite future is
        // dropped (its incremental rows persist), a warning is logged, a
        // "…-capped…" note is recorded, and the suite name is collected in
        // `capped_suites` — the model is NEVER marked failed, and the loop
        // continues to the next suite. `cap == None` awaits normally (no wrapper).
        let cap = suite_wallclock_cap_for(model);
        let cap_secs = cap.map(|c| c.as_secs()).unwrap_or(0);
        let mut capped_suites: Vec<String> = Vec::new();

        // The context suite already has internal tier bounds; it is wrapped in the
        // SAME cap for uniformity (belt-and-suspenders) — a 20 min cap will not
        // trigger for a properly tier-capped context suite, but it guards a
        // pathological case.
        if suites.iter().any(|s| s == "context") {
            match with_suite_cap(cap, run_context_suite(model, tiers, false)).await {
                Some(Ok(o)) => {
                    profile_id = Some(o.profile_id);
                    parts.push(format!(
                        "context: safe_ctx={}, tier={}",
                        o.op.max_context_safe.map(|v| v.to_string()).unwrap_or_else(|| "n/a".into()),
                        o.op.overall_tier.as_deref().unwrap_or("n/a"),
                    ));
                }
                Some(Err(e)) => parts.push(format!("context: error {e}")),
                None => {
                    tracing::warn!("intake: suite context wall-clock-capped at {cap_secs}s for {model}; partial results kept, moving on");
                    parts.push(format!("context: wall-clock-capped at {cap_secs}s (partial)"));
                    capped_suites.push("context".to_string());
                }
            }
        }
        let needs = suites.iter().any(|s| s == "code" || s == "agent" || s == "tool_routing");
        if needs && profile_id.is_none() {
            match create_profile_row(model).await {
                Ok(id) => profile_id = Some(id),
                Err(e) => parts.push(format!("profile: error {e}")),
            }
        }
        if suites.iter().any(|s| s == "code") {
            if let Some(id) = profile_id {
                match with_suite_cap(cap, run_code(model.clone(), resolve_langs(model), id)).await {
                    Some(Ok(s)) => parts.push(format!("code: {s}")),
                    Some(Err(e)) => parts.push(format!("code: error {e}")),
                    None => {
                        tracing::warn!("intake: suite code wall-clock-capped at {cap_secs}s for {model}; partial results kept, moving on");
                        parts.push(format!("code: wall-clock-capped at {cap_secs}s (partial)"));
                        capped_suites.push("code".to_string());
                    }
                }
            }
        }
        if suites.iter().any(|s| s == "agent") {
            if let Some(id) = profile_id {
                match with_suite_cap(cap, run_agent(model.clone(), id)).await {
                    Some(Ok(s)) => parts.push(format!("agent: {s}")),
                    Some(Err(e)) => parts.push(format!("agent: error {e}")),
                    None => {
                        tracing::warn!("intake: suite agent wall-clock-capped at {cap_secs}s for {model}; partial results kept, moving on");
                        parts.push(format!("agent: wall-clock-capped at {cap_secs}s (partial)"));
                        capped_suites.push("agent".to_string());
                    }
                }
            }
        }
        // SUITE-EMB (TERM #508): embedding models profile IR retrieval quality
        // instead of context/code/agent. The driver creates its own profile row
        // (like the diffusion suite) and cleanly skips a non-embedding candidate.
        if suites.iter().any(|s| s == "embedding_retrieval") {
            match with_suite_cap(cap, run_embedding_retrieval_suite(model)).await {
                Some(Ok(o)) => parts.push(format!("embedding_retrieval: {}", o.summary)),
                Some(Err(e)) => parts.push(format!("embedding_retrieval: error {e}")),
                None => {
                    tracing::warn!("intake: suite embedding_retrieval wall-clock-capped at {cap_secs}s for {model}; partial results kept, moving on");
                    parts.push(format!("embedding_retrieval: wall-clock-capped at {cap_secs}s (partial)"));
                    capped_suites.push("embedding_retrieval".to_string());
                }
            }
        }
        // S125 SUITE-TOOL: the tool-routing suite is self-contained (it resolves
        // its own backend + corpus and reuses the shared profile row), so it is
        // dispatched directly here rather than via an injected closure like
        // code/agent. Requested only when a model's resolved suites include it.
        if suites.iter().any(|s| s == "tool_routing") {
            if let Some(id) = profile_id {
                match with_suite_cap(cap, run_tool_routing_suite(model, id, None)).await {
                    Some(Ok(o)) => parts.push(format!(
                        "tool_routing: {} scenarios ({} rows), correct@1={}",
                        o.scenarios_run,
                        o.rows_written,
                        o.correct_tool_at_1.map(|v| format!("{:.0}%", v * 100.0)).unwrap_or_else(|| "n/a".into()),
                    )),
                    Some(Err(e)) => parts.push(format!("tool_routing: error {e}")),
                    None => {
                        tracing::warn!("intake: suite tool_routing wall-clock-capped at {cap_secs}s for {model}; partial results kept, moving on");
                        parts.push(format!("tool_routing: wall-clock-capped at {cap_secs}s (partial)"));
                        capped_suites.push("tool_routing".to_string());
                    }
                }
            }
        }

        // SUITE-VQA: the vision-QA suite runs its own corpus + image chat-route
        // path and creates its own profile row (like the single-model MCP path),
        // so it is dispatched by name here and does not share the context/code/
        // agent profile_id. A missing corpus / backend is a clean per-model note,
        // never a panic that aborts the fleet sweep.
        if suites.iter().any(|s| s == "vision_qa") {
            match with_suite_cap(cap, run_vision_qa_suite(model)).await {
                Some(Ok(v)) => parts.push(format!(
                    "vision_qa: {} items, acc={:.2}, halluc={:.2}",
                    v.items_run, v.accuracy, v.hallucination_rate
                )),
                Some(Err(e)) => parts.push(format!("vision_qa: error {e}")),
                None => {
                    tracing::warn!("intake: suite vision_qa wall-clock-capped at {cap_secs}s for {model}; partial results kept, moving on");
                    parts.push(format!("vision_qa: wall-clock-capped at {cap_secs}s (partial)"));
                    capped_suites.push("vision_qa".to_string());
                }
            }
        }

        // SUITE-RRK: reranking self-manages its own profile row (provider
        // "openai", Chord's /v1/rerank), independent of the Ollama-shared
        // profile_id above — a reranker never goes through the context/code/
        // agent path. A per-query error is folded into the returned summary; an
        // error here (e.g. missing corpus) degrades this model's line only.
        if suites.iter().any(|s| s == "reranking") {
            match with_suite_cap(cap, run_reranking_suite(model)).await {
                Some(Ok(o)) => parts.push(format!(
                    "reranking: uplift={:.3} ndcg={:.3} queries={}",
                    o.avg_ndcg_uplift, o.avg_reranked_ndcg, o.queries_run
                )),
                Some(Err(e)) => parts.push(format!("reranking: error {e}")),
                None => {
                    tracing::warn!("intake: suite reranking wall-clock-capped at {cap_secs}s for {model}; partial results kept, moving on");
                    parts.push(format!("reranking: wall-clock-capped at {cap_secs}s (partial)"));
                    capped_suites.push("reranking".to_string());
                }
            }
        }
        // S125 SUITE-STT: whisper/ASR models are served behind Chord's
        // `/v1/audio/transcriptions` route (a non-`ollama` `openai`-kind backend).
        // With the backend-aware warm gate above they are no longer force-loaded
        // via Ollama, so the suite is now dispatched here in the unified section
        // like every other Chord-served suite (replacing the old before-the-gate
        // early return). Self-contained — creates its own `openai`-provider row.
        if suites.iter().any(|s| s == "stt") {
            match with_suite_cap(cap, run_stt_suite(model)).await {
                Some(Ok(o)) => parts.push(format!(
                    "stt: clips={} avg_wer={:.3} avg_rtf={:.2}",
                    o.clips_run, o.avg_wer, o.avg_rtf
                )),
                Some(Err(e)) => parts.push(format!("stt: error {e}")),
                None => {
                    tracing::warn!("intake: suite stt wall-clock-capped at {cap_secs}s for {model}; partial results kept, moving on");
                    parts.push(format!("stt: wall-clock-capped at {cap_secs}s (partial)"));
                    capped_suites.push("stt".to_string());
                }
            }
        }
        // SUITE-IMG: image-generation suite. Self-contained (its own profile row
        // + `openai` backend via Chord's `/v1/images/generations`), so it does
        // not share the Ollama `profile_id` above. BT-03: with the backend-aware
        // warm gate, an `openai`-kind image backend (sd-turbo) is NO LONGER
        // skipped at an Ollama load gate — it reaches this dispatch in a fleet
        // sweep and runs via its own Chord-served path.
        if suites.iter().any(|s| s == "image_generation") {
            match with_suite_cap(cap, run_image_generation_suite(model)).await {
                Some(Ok(o)) => parts.push(format!(
                    "image_generation: {}/{} ok, avg_time_ms={:.0}",
                    o.success_count, o.prompts_run, o.avg_time_to_image_ms
                )),
                Some(Err(e)) => parts.push(format!("image_generation: error {e}")),
                None => {
                    tracing::warn!("intake: suite image_generation wall-clock-capped at {cap_secs}s for {model}; partial results kept, moving on");
                    parts.push(format!("image_generation: wall-clock-capped at {cap_secs}s (partial)"));
                    capped_suites.push("image_generation".to_string());
                }
            }
        }
        // SUITE-DOC: document_parsing goes through Chord's `/v1/documents/parse`
        // (not the Ollama serve loaded above), and owns its own profile row, so
        // it dispatches directly here (like the diffusion suite) rather than via
        // an injected closure. A NotConfigured corpus is recorded, not fatal.
        if suites.iter().any(|s| s == "document_parsing") {
            match with_suite_cap(cap, run_document_parsing_suite(model)).await {
                Some(Ok(o)) => parts.push(format!(
                    "document_parsing: cases={} avg_field_acc={:.2}",
                    o.cases_run, o.avg_field_accuracy
                )),
                Some(Err(e)) => parts.push(format!("document_parsing: error {e}")),
                None => {
                    tracing::warn!("intake: suite document_parsing wall-clock-capped at {cap_secs}s for {model}; partial results kept, moving on");
                    parts.push(format!("document_parsing: wall-clock-capped at {cap_secs}s (partial)"));
                    capped_suites.push("document_parsing".to_string());
                }
            }
        }
        // S125 SUITE-TTS: self-contained driver (creates its own `openai`-provider
        // profile row via `run_tts_suite`, like the diffusion suite), so it does not
        // share the context/code/agent `profile_id` above.
        if suites.iter().any(|s| s == "tts") {
            match with_suite_cap(cap, run_tts_suite(model)).await {
                Some(Ok(o)) => parts.push(format!(
                    "tts: cases={} avg_wer={:.2} avg_rtf={}",
                    o.cases_run,
                    o.avg_loopback_wer,
                    o.avg_rtf.map(|r| format!("{r:.2}")).unwrap_or_else(|| "n/a".into()),
                )),
                Some(Err(e)) => parts.push(format!("tts: error {e}")),
                None => {
                    tracing::warn!("intake: suite tts wall-clock-capped at {cap_secs}s for {model}; partial results kept, moving on");
                    parts.push(format!("tts: wall-clock-capped at {cap_secs}s (partial)"));
                    capped_suites.push("tts".to_string());
                }
            }
        }

        // S125: surface which suites (if any) were wall-clock-capped in the
        // one-line summary too, so it is visible without inspecting the struct.
        if !capped_suites.is_empty() {
            parts.push(format!("[wall-clock-capped: {}]", capped_suites.join(",")));
        }

        evict_model(&client, model).await;
        out.push(FleetSuiteResult {
            model: model.clone(),
            suites,
            summary: parts.join("; "),
            skipped: false,
            capped_suites,
        });
    }

    if let Some(prior) = &prior_hot {
        let _ = load_model(&client, prior).await;
    }
    // Final progress tick: everything done, no model currently in flight.
    on_progress(total, total, "", "");
    out
}

/// Boxed-future type aliases so closures returning the suite drivers can be
/// passed as `impl Fn`. Kept in a submodule to keep the signatures readable.
pub mod futures_box {
    use crate::error::ToolError;
    use std::future::Future;
    use std::pin::Pin;
    pub type CodeFut = Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send>>;
    pub type AgentFut = Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(ctx: i32, tp: f64, recall: i32, oom: bool) -> TierSummary {
        TierSummary { context_tokens: ctx, throughput: Some(tp), recall: Some(recall), oom }
    }

    #[test]
    fn recommended_timeouts_from_throughput() {
        // 16000 tokens at 200 tok/s → 80s + 10 = 90 chat; 360 build; 900 deep.
        let (chat, build, deep) = recommended_timeouts(16000, Some(200.0));
        assert_eq!(chat, 90);
        assert_eq!(build, 360);
        assert_eq!(deep, 900);
    }

    #[test]
    fn recommended_timeouts_fallback_when_no_throughput() {
        let (chat, build, deep) = recommended_timeouts(16000, None);
        assert_eq!(chat, 30);
        assert_eq!(build, 120);
        assert_eq!(deep, 300);
        let (chat0, _, _) = recommended_timeouts(16000, Some(0.0));
        assert_eq!(chat0, 30);
    }

    #[test]
    fn derive_profile_safe_absolute_degradation() {
        // 3/3 up to 16k, drops to 1 at 32k, OOM at 64k.
        let tiers = vec![
            t(2000, 300.0, 3, false),
            t(8000, 250.0, 3, false),
            t(16000, 200.0, 3, false),
            t(32000, 150.0, 1, false),
            t(64000, 100.0, 0, true),
        ];
        let op = derive_profile(&tiers);
        assert_eq!(op.max_context_safe, Some(16000));
        assert_eq!(op.max_context_absolute, Some(32000)); // 64k OOM'd
        assert_eq!(op.quality_degradation_point, Some(32000));
        assert_eq!(op.throughput_at_2k, Some(300.0));
        assert_eq!(op.throughput_at_16k, Some(200.0));
        assert_eq!(op.throughput_at_32k, Some(150.0));
        // Timeout based on degradation tier (32000 @ 150 tok/s) = ceil(213.3)+10 = 224.
        assert_eq!(op.recommended_timeout_chat_sec, Some(224));
        assert_eq!(op.overall_tier.as_deref(), Some("standard"));
    }

    #[test]
    fn derive_profile_no_degradation_uses_absolute() {
        let tiers = vec![
            t(2000, 300.0, 3, false),
            t(8000, 280.0, 3, false),
        ];
        let op = derive_profile(&tiers);
        assert_eq!(op.max_context_safe, Some(8000));
        assert_eq!(op.max_context_absolute, Some(8000));
        assert_eq!(op.quality_degradation_point, None);
        // No 16k/32k/64k measured.
        assert_eq!(op.throughput_at_16k, None);
        assert!(op.recommended_timeout_chat_sec.is_some());
    }

    #[test]
    fn derive_profile_all_oom_is_review_only() {
        let tiers = vec![TierSummary {
            context_tokens: 2000,
            throughput: None,
            recall: None,
            oom: true,
        }];
        let op = derive_profile(&tiers);
        assert_eq!(op.max_context_safe, None);
        assert_eq!(op.max_context_absolute, None);
        assert_eq!(op.overall_tier.as_deref(), Some("review-only"));
    }

    #[test]
    fn classify_tier_thresholds() {
        assert_eq!(classify_tier(Some(96000)), "deep");
        assert_eq!(classify_tier(Some(16000)), "standard");
        assert_eq!(classify_tier(Some(2000)), "blitz");
        assert_eq!(classify_tier(Some(0)), "review-only");
        assert_eq!(classify_tier(None), "review-only");
    }

    #[test]
    fn is_hot_matches_exact_and_prefix() {
        let loaded = vec![("gpt-oss:20b".to_string(), 1u64)];
        assert!(is_hot(&loaded, "gpt-oss:20b"));
        assert!(is_hot(&loaded, "gpt-oss"));
        assert!(!is_hot(&loaded, "qwen3:8b"));
        assert!(!is_hot(&[], "anything"));
    }

    #[test]
    fn smoke_and_full_tier_lists() {
        assert_eq!(SMOKE_TIERS, [2000, 8000, 16000]);
        assert_eq!(FULL_TIERS.len(), 9);
        assert_eq!(FULL_TIERS[0], 2000);
        assert_eq!(FULL_TIERS[8], 128000);
    }

    // ---- FIX2 (S125): size-based tier cap + stop-on-timeout ------------

    /// Parameter-scale parsing pulls the trailing `<N>b` token (fractional
    /// rounds down), ignores a version tag not followed by `b`, and returns
    /// None when there is no `<N>b` token.
    #[test]
    fn parse_param_scale_from_name() {
        assert_eq!(parse_param_scale_b("gpt-oss:120b"), Some(120));
        assert_eq!(parse_param_scale_b("qwen3-coder:30b"), Some(30));
        assert_eq!(parse_param_scale_b("deepseek-r1:14b"), Some(14));
        assert_eq!(parse_param_scale_b("llama3:8b"), Some(8));
        assert_eq!(parse_param_scale_b("gemma2:2.6b"), Some(2)); // rounds down
        // A `2.5` version tag NOT followed by `b` must be ignored; the real
        // `32b` size wins.
        assert_eq!(parse_param_scale_b("qwen2.5-coder:32b"), Some(32));
        // No `<N>b` token at all.
        assert_eq!(parse_param_scale_b("nomic-embed-text:latest"), None);
        assert_eq!(parse_param_scale_b("sd-turbo"), None);
    }

    /// The size→cap mapping: a 120b model is capped at 16k (32k+ excluded);
    /// a mid model at 32k/64k; a small/unknown model keeps the FULL ladder.
    #[test]
    fn cap_from_scale_mapping() {
        assert_eq!(cap_from_scale(Some(120)), 16000);
        assert_eq!(cap_from_scale(Some(100)), 16000);
        assert_eq!(cap_from_scale(Some(70)), 32000);
        assert_eq!(cap_from_scale(Some(30)), 32000);
        assert_eq!(cap_from_scale(Some(14)), 64000);
        assert_eq!(cap_from_scale(Some(13)), 64000);
        assert_eq!(cap_from_scale(Some(8)), 128000);
        assert_eq!(cap_from_scale(Some(7)), 128000);
        assert_eq!(cap_from_scale(None), 128000);
    }

    /// A "120b"-named model's capped tier ladder excludes 32k and above.
    #[test]
    fn tier_cap_for_120b_excludes_32k_plus() {
        let cap = cap_from_scale(parse_param_scale_b("gpt-oss:120b"));
        let tiers = cap_tiers(&FULL_TIERS, cap);
        assert_eq!(tiers, vec![2000, 4000, 8000, 16000]);
        assert!(!tiers.iter().any(|&t| t >= 32000));
    }

    /// A small model keeps the FULL ladder unchanged.
    #[test]
    fn tier_cap_for_small_model_keeps_full() {
        let cap = cap_from_scale(parse_param_scale_b("llama3:8b"));
        let tiers = cap_tiers(&FULL_TIERS, cap);
        assert_eq!(tiers, FULL_TIERS.to_vec());
    }

    /// The cap always leaves at least one (the smallest) tier, even when the
    /// cap is below every tier.
    #[test]
    fn tier_cap_never_empties() {
        let tiers = cap_tiers(&FULL_TIERS, 1);
        assert_eq!(tiers, vec![2000]);
    }

    /// S125 (SUITE-WALLCLOCK): the size→cap mapping and the env-override
    /// precedence, exercised purely via `suite_wallclock_cap_core` (no env):
    /// >=100b → 20 min, >=30b → 40 min, else None; a positive env override wins
    /// over ANY size; a `0`/`None` override falls through to the size mapping.
    #[test]
    fn suite_wallclock_cap_core_mapping_and_override() {
        use std::time::Duration;
        // Size mapping (no override).
        assert_eq!(suite_wallclock_cap_core(None, Some(120)), Some(Duration::from_secs(20 * 60)));
        assert_eq!(suite_wallclock_cap_core(None, Some(100)), Some(Duration::from_secs(20 * 60)));
        assert_eq!(suite_wallclock_cap_core(None, Some(70)), Some(Duration::from_secs(40 * 60)));
        assert_eq!(suite_wallclock_cap_core(None, Some(32)), Some(Duration::from_secs(40 * 60)));
        assert_eq!(suite_wallclock_cap_core(None, Some(30)), Some(Duration::from_secs(40 * 60)));
        assert_eq!(suite_wallclock_cap_core(None, Some(13)), None);
        assert_eq!(suite_wallclock_cap_core(None, Some(8)), None);
        assert_eq!(suite_wallclock_cap_core(None, None), None);
        // Positive env override WINS over the size mapping (incl. an 8b that the
        // size mapping would leave uncapped, and a 120b it would cap at 20 min).
        assert_eq!(suite_wallclock_cap_core(Some(600), Some(8)), Some(Duration::from_secs(600)));
        assert_eq!(suite_wallclock_cap_core(Some(600), Some(120)), Some(Duration::from_secs(600)));
        // A `0` override is ignored → fall through to the size mapping.
        assert_eq!(suite_wallclock_cap_core(Some(0), Some(120)), Some(Duration::from_secs(20 * 60)));
        assert_eq!(suite_wallclock_cap_core(Some(0), Some(8)), None);
    }

    /// S125: end-to-end through the model NAME → cap path
    /// (`suite_wallclock_cap_for` parses the `<N>b` size AND reads the env
    /// override). Kept as ONE test so the single global env var
    /// `INTAKE_SUITE_WALLCLOCK_CAP_SEC` (read only by this function) is
    /// manipulated serially and can't race a sibling test.
    /// - No override → size mapping: 120b → 20 min, 70b/32b → 40 min, 8b/embed → None.
    /// - `=600` → forces 600s for EVERY model (wins over the size mapping).
    #[test]
    fn suite_wallclock_cap_for_by_name_and_env() {
        use std::time::Duration;
        // Deterministic size mapping (ensure no override lingering).
        std::env::remove_var("INTAKE_SUITE_WALLCLOCK_CAP_SEC");
        assert_eq!(suite_wallclock_cap_for("gpt-oss:120b"), Some(Duration::from_secs(20 * 60)));
        assert_eq!(suite_wallclock_cap_for("llama3:70b"), Some(Duration::from_secs(40 * 60)));
        assert_eq!(suite_wallclock_cap_for("qwen2.5-coder:32b"), Some(Duration::from_secs(40 * 60)));
        assert_eq!(suite_wallclock_cap_for("llama3:8b"), None);
        assert_eq!(suite_wallclock_cap_for("nomic-embed-text:latest"), None);

        // Env override wins — even for an 8b the size mapping leaves uncapped, and
        // even over a huge model's size-derived cap.
        std::env::set_var("INTAKE_SUITE_WALLCLOCK_CAP_SEC", "600");
        assert_eq!(suite_wallclock_cap_for("llama3:8b"), Some(Duration::from_secs(600)));
        assert_eq!(suite_wallclock_cap_for("gpt-oss:120b"), Some(Duration::from_secs(600)));

        // Cleared → back to the size mapping.
        std::env::remove_var("INTAKE_SUITE_WALLCLOCK_CAP_SEC");
        assert_eq!(suite_wallclock_cap_for("llama3:8b"), None);
    }

    /// S125 driver-level: a suite future that sleeps far beyond the cap is aborted
    /// by `with_suite_cap` (recorded as capped/`None`), and the per-model loop
    /// CONTINUES to the following suite — no whole-model failure. Uses a paused
    /// clock so the timeout fires deterministically with no real waiting.
    #[tokio::test(start_paused = true)]
    async fn with_suite_cap_aborts_slow_suite_and_continues() {
        use crate::error::ToolError;
        use std::time::Duration;
        use tokio::time::sleep;

        let cap = Some(Duration::from_secs(10));
        // Mirror the run_fleet_suites per-suite loop bookkeeping.
        let mut parts: Vec<String> = Vec::new();
        let mut capped: Vec<String> = Vec::new();

        // Suite A completes within the cap.
        match with_suite_cap(cap, async {
            sleep(Duration::from_secs(1)).await;
            Ok::<i32, ToolError>(1)
        })
        .await
        {
            Some(Ok(v)) => parts.push(format!("a: {v}")),
            Some(Err(e)) => parts.push(format!("a: error {e}")),
            None => capped.push("a".to_string()),
        }
        // Suite B hangs FAR beyond the cap → aborted (None), not an error.
        match with_suite_cap(cap, async {
            sleep(Duration::from_secs(10_000)).await;
            Ok::<i32, ToolError>(2)
        })
        .await
        {
            Some(Ok(v)) => parts.push(format!("b: {v}")),
            Some(Err(e)) => parts.push(format!("b: error {e}")),
            None => capped.push("b".to_string()),
        }
        // Suite C — AFTER the capped one — still runs → the loop continued.
        match with_suite_cap(cap, async {
            sleep(Duration::from_secs(2)).await;
            Ok::<i32, ToolError>(3)
        })
        .await
        {
            Some(Ok(v)) => parts.push(format!("c: {v}")),
            Some(Err(e)) => parts.push(format!("c: error {e}")),
            None => capped.push("c".to_string()),
        }

        // The slow suite was capped; the surrounding suites completed; the loop
        // never bailed out (no whole-model failure).
        assert_eq!(parts, vec!["a: 1", "c: 3"]);
        assert_eq!(capped, vec!["b"]);
    }

    /// S125: `with_suite_cap(None, …)` awaits the future normally (no timeout
    /// wrapper) and yields its output.
    #[tokio::test]
    async fn with_suite_cap_none_awaits_normally() {
        use crate::error::ToolError;
        let out = with_suite_cap(None, async { Ok::<i32, ToolError>(42) }).await;
        match out {
            Some(Ok(v)) => assert_eq!(v, 42),
            other => panic!("expected Some(Ok(42)), got a different variant: {}", other.is_some()),
        }
    }

    /// The escalation stop predicate fires ONLY on a genuine capacity limit —
    /// OOM or a TIMEOUT — never on a clean tier or a transient connect/body/parse
    /// error (those must not wrongly cap the model at that tier).
    #[test]
    fn tier_ceiling_stops_on_oom_and_timeout_only() {
        // Clean tier: keep escalating.
        assert!(!tier_hit_ceiling(false, None));
        // OOM tier (a real capacity limit, incl. HTTP 500/503 which run_tier
        // flags oom=true): stop.
        assert!(tier_hit_ceiling(true, None));
        // Genuine TIMEOUT (oom=false), exactly as run_tier records it: stop.
        assert!(tier_hit_ceiling(
            false,
            Some("run_tier: inference request failed (timed out); is_timeout=true is_connect=false is_body=false")
        ));
        // Transient CONNECT error: NOT a ceiling -> keep escalating.
        assert!(!tier_hit_ceiling(
            false,
            Some("run_tier: inference request failed (connection refused); is_timeout=false is_connect=true is_body=false")
        ));
        // Transient BODY/parse error: NOT a ceiling either.
        assert!(!tier_hit_ceiling(false, Some("response parse error: expected value")));
    }

    /// Simulate the escalation loop over a sequence of tier outcomes: it must
    /// STOP at the first ceiling tier (here a timeout at 32k) and never attempt
    /// 48k/64k/... — bounding a big model to ~one timeout.
    #[test]
    fn escalation_stops_after_timeout_tier() {
        // (oom, error) per tier, mirroring TierResult's fields.
        let outcomes: Vec<(usize, bool, Option<&str>)> = vec![
            (2000, false, None),
            (4000, false, None),
            (8000, false, None),
            (16000, false, None),
            (32000, false, Some("... timed out ...")), // ceiling
            (48000, false, None),                       // must NOT be reached
            (64000, false, None),                       // must NOT be reached
        ];
        let mut attempted = Vec::new();
        for (tier, oom, err) in &outcomes {
            attempted.push(*tier);
            if tier_hit_ceiling(*oom, *err) {
                break;
            }
        }
        assert_eq!(attempted, vec![2000, 4000, 8000, 16000, 32000]);
        assert!(!attempted.iter().any(|&t| t >= 48000));
    }

    /// A TRANSIENT connect error mid-ladder must NOT halt escalation: the loop
    /// records that tier's failure and keeps going, stopping only at the later
    /// genuine timeout. (Guards the codex re-review fix — an 8k network blip must
    /// not cap the model at 8k.)
    #[test]
    fn escalation_continues_past_transient_connect_error() {
        let outcomes: Vec<(usize, bool, Option<&str>)> = vec![
            (2000, false, None),
            // Transient connect blip: recorded, but NOT a ceiling.
            (
                8000,
                false,
                Some("inference request failed (connection refused); is_timeout=false is_connect=true"),
            ),
            (16000, false, None),
            (32000, false, Some("... timed out ...")), // real ceiling
            (48000, false, None),                       // must NOT be reached
        ];
        let mut attempted = Vec::new();
        for (tier, oom, err) in &outcomes {
            attempted.push(*tier);
            if tier_hit_ceiling(*oom, *err) {
                break;
            }
        }
        // The 8k connect blip did NOT halt; escalation continued and stopped only
        // at the 32k timeout.
        assert_eq!(attempted, vec![2000, 8000, 16000, 32000]);
        assert!(!attempted.iter().any(|&t| t >= 48000));
    }

    // BT-03 (TASK #15): the fleet warm gate is backend-aware — only ollama-kind
    // models are pre-warmed via the Ollama control API; every other kind is
    // Chord-/registry-served and must NOT be sent to `load_model`.
    #[test]
    fn fleet_warm_gate_only_ollama() {
        assert!(needs_ollama_warm("ollama"));
        assert!(!needs_ollama_warm("openai"));
        assert!(!needs_ollama_warm("daemon"));
        assert!(!needs_ollama_warm("llama-server"));
    }

    // BT-03: an `openai`-kind model resolved from a registry is NOT sent to the
    // Ollama warm gate; an `ollama`-kind model still is. Exercises the real
    // `resolve_backend_at` core against a fixture registry (no env, no network).
    #[test]
    fn openai_model_not_sent_to_ollama_warm() {
        use crate::intake::infer::resolve_backend_at;
        let reg = r#"{
            "models": {
                "sd-turbo": {"backend": "chord"},
                "qwen3:8b": {"backend": "ollama"}
            },
            "backends": {
                "chord":  {"url": "http://chord.invalid", "kind": "openai"},
                "ollama": {"url": "http://ollama.invalid", "kind": "ollama"}
            }
        }"#;
        let path = std::env::temp_dir().join(format!("mint-reg-{}.json", uuid::Uuid::new_v4()));
        std::fs::write(&path, reg).unwrap();
        let p = path.to_str().unwrap();

        let openai = resolve_backend_at("sd-turbo", p, "http://fallback.invalid", None);
        assert_eq!(openai.kind, "openai");
        assert!(!needs_ollama_warm(&openai.kind), "openai model must skip ollama warm");

        let ollama = resolve_backend_at("qwen3:8b", p, "http://fallback.invalid", None);
        assert_eq!(ollama.kind, "ollama");
        assert!(needs_ollama_warm(&ollama.kind), "ollama model must be warmed");

        let _ = std::fs::remove_file(&path);
    }

    // DR-01: bounded retry succeeds on a later attempt (mock warm, ZERO backoff so
    // the test is clock-free — no Instant/wall-time flakiness).
    #[tokio::test]
    async fn warm_retries_then_succeeds_on_second_attempt() {
        use std::cell::Cell;
        let calls = Cell::new(0usize);
        let r = warm_with_backoff(3, Duration::ZERO, || {
            calls.set(calls.get() + 1);
            let n = calls.get();
            async move {
                if n < 2 {
                    Err(ToolError::Http("transient warm fail".into()))
                } else {
                    Ok(())
                }
            }
        })
        .await;
        assert!(r.is_ok());
        assert_eq!(calls.get(), 2, "should stop retrying once it succeeds");
    }

    // DR-01: gives up after the bounded number of attempts, returning the error.
    #[tokio::test]
    async fn warm_gives_up_after_bounded_attempts() {
        use std::cell::Cell;
        let calls = Cell::new(0usize);
        let r = warm_with_backoff(3, Duration::ZERO, || {
            calls.set(calls.get() + 1);
            async { Err::<(), _>(ToolError::Http("always fails".into())) }
        })
        .await;
        assert!(r.is_err());
        assert_eq!(calls.get(), 3, "should attempt exactly the bound, then give up");
    }

    /// Regression test for the S125 suite-driver FK bug: the eight (nine incl.
    /// diffusion) new-category suite drivers were passing the `model_profiles`
    /// id returned by `create_profile_row_for_provider` straight into a suite's
    /// `score_and_write(..)` as its `run_id`. But `assistant_dimension_score`
    /// has `run_id UUID NOT NULL REFERENCES assistant_profile_run(id)`, so every
    /// live `model_intake` on a real Postgres died with
    /// `violates foreign key constraint "assistant_dimension_score_run_id_fkey"`.
    /// The unit tests missed it because they exercised pure scoring, never the
    /// FK-enforcing write. This test drives the REAL write path against a live
    /// Postgres two ways:
    ///
    ///   * POSITIVE — a `run_id` obtained THE WAY THE DRIVERS NOW DO IT
    ///     (`assistant::schema::insert_run`, an `assistant_profile_run` parent)
    ///     makes `reranking::score_and_write` (→ `insert_dimension_score_with_category`)
    ///     SUCCEED and land rows.
    ///   * NEGATIVE — feeding the `model_profiles` id (the OLD, broken argument)
    ///     as `run_id` reproduces the exact FK violation, proving the constraint
    ///     is enforced and that the regression, if reintroduced, would be caught.
    ///
    /// Gated on a reachable Postgres (same convention as the `assistant::schema`
    /// regression tests): skips (passes trivially) when neither
    /// `INTAKE_DATABASE_URL` nor `DATABASE_URL` is configured, so it stays green
    /// with no live DB while running for real whenever one is available. NOTE:
    /// this is the real guard for this bug class — a live pg is required to
    /// actually exercise the FK; with no DB the test only skips.
    #[tokio::test]
    async fn suite_driver_run_id_is_assistant_profile_run_fk_safe() {
        use crate::intake::assistant::schema::{insert_run, migrate};
        use crate::intake::assistant::{BackendTag, ModelId};
        use crate::intake::newcats::reranking::{self, RerankOutcome, RerankQuery};

        let pool = match storage::get_pool().await {
            Ok(p) => p,
            Err(_) => {
                eprintln!(
                    "skipping suite_driver_run_id_is_assistant_profile_run_fk_safe: \
                     no INTAKE_DATABASE_URL/DATABASE_URL configured"
                );
                return;
            }
        };
        if migrate(&pool).await.is_err() {
            eprintln!(
                "skipping suite_driver_run_id_is_assistant_profile_run_fk_safe: \
                 migrate() failed (DB unreachable or not provisioned)"
            );
            return;
        }

        // Reproduce a driver's exact preamble: a `model_profiles` row (catalog
        // id) via the shared helper, and a SEPARATE `assistant_profile_run`
        // parent for the scores. These are DIFFERENT tables — the whole bug was
        // conflating the two ids.
        let model_name = format!("s125-fk-regress-{}", uuid::Uuid::new_v4());
        let profile_id = create_profile_row_for_provider(&model_name, "openai")
            .await
            .expect("create_profile_row_for_provider (model_profiles id)");
        let run_id = insert_run(&pool).await.expect("insert_run (assistant_profile_run id)");
        assert_ne!(profile_id, run_id, "the two ids must not collide");

        let model_id = ModelId::from(model_name.as_str());
        let query = RerankQuery {
            query_id: "q1".to_string(),
            query: "what is the capital".to_string(),
            passages: vec!["irrelevant".to_string(), "relevant".to_string()],
            relevance: vec![0.0, 1.0],
            baseline_order: vec![0, 1],
        };
        let outcome = RerankOutcome { reranked_order: vec![1, 0], latency_ms: 12 };

        // POSITIVE: the fixed driver contract — score against the
        // assistant_profile_run id — must write cleanly (no FK violation).
        reranking::score_and_write(&pool, run_id, model_id.clone(), BackendTag::Gpu, &query, &outcome)
            .await
            .expect("score_and_write with an assistant_profile_run run_id must succeed");

        let written: i64 =
            sqlx::query_scalar("SELECT count(*) FROM assistant_dimension_score WHERE run_id = $1")
                .bind(run_id)
                .fetch_one(&pool)
                .await
                .expect("count written rows");
        assert!(written > 0, "the suite write path must have persisted at least one score row");

        // NEGATIVE: the OLD, broken argument — the model_profiles id as run_id —
        // must be REJECTED by the FK, reproducing the exact production failure.
        let err = reranking::score_and_write(
            &pool,
            profile_id,
            model_id.clone(),
            BackendTag::Gpu,
            &query,
            &outcome,
        )
        .await
        .expect_err("passing a model_profiles id as run_id MUST violate the assistant_profile_run FK");
        let msg = err.to_string().to_lowercase();
        assert!(
            msg.contains("foreign key") || msg.contains("assistant_dimension_score_run_id_fkey"),
            "the failure must be the run_id FK violation (the reported prod bug), got: {msg}"
        );

        // Cleanup: rows are scoped to this run/model, so this only removes what
        // this test inserted.
        let _ = sqlx::query("DELETE FROM assistant_dimension_score WHERE run_id = $1")
            .bind(run_id)
            .execute(&pool)
            .await;
        let _ = sqlx::query("DELETE FROM assistant_profile_run WHERE id = $1")
            .bind(run_id)
            .execute(&pool)
            .await;
        let _ = sqlx::query("DELETE FROM model_profiles WHERE id = $1")
            .bind(profile_id)
            .execute(&pool)
            .await;
    }
}
