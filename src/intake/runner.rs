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

        if tr.oom {
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

        diffusion::score_and_write(&pool, profile_id, model_id.clone(), backend_tag, use_case, &outcome).await?;

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
    let summary = er::score_and_write(&pool, profile_id, &embedder, &public, domain.as_ref()).await?;

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
        rows_written += tool_routing::score_and_write(&pool, profile_id, model_id.clone(), backend_tag, sc, &outcome).await?;
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

        image_parsing::score_and_write_vqa(&pool, profile_id, model_id.clone(), backend_tag, item, &outcome).await?;

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

        reranking::score_and_write(&pool, profile_id, model_id.clone(), backend_tag, query, &outcome)
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

        image_generation::score_and_write(&pool, profile_id, model_id.clone(), backend_tag, &outcome).await?;

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

        document_parsing::score_and_write(&pool, profile_id, model_id.clone(), backend_tag, &truth, &outcome)
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
        // Self-heal: skip a model rather than fail the run on a full disk.
        if let Some(reason) = disk_pressure() {
            out.push(FleetSuiteResult {
                model: model.clone(),
                suites: suites.clone(),
                summary: format!("skipped: disk pressure — {reason}"),
                skipped: true,
            });
            continue;
        }
        if is_daemon(model) {
            out.push(FleetSuiteResult {
                model: model.clone(),
                suites,
                summary: "skipped: non-Ollama daemon model (DiffusionGemma/dgem)".into(),
                skipped: true,
            });
            continue;
        }
        if let Err(e) = load_model(&client, model).await {
            out.push(FleetSuiteResult {
                model: model.clone(),
                suites,
                summary: format!("load failed: {e}"),
                skipped: true,
            });
            continue;
        }

        let mut profile_id: Option<uuid::Uuid> = None;
        let mut parts: Vec<String> = Vec::new();

        if suites.iter().any(|s| s == "context") {
            match run_context_suite(model, tiers, false).await {
                Ok(o) => {
                    profile_id = Some(o.profile_id);
                    parts.push(format!(
                        "context: safe_ctx={}, tier={}",
                        o.op.max_context_safe.map(|v| v.to_string()).unwrap_or_else(|| "n/a".into()),
                        o.op.overall_tier.as_deref().unwrap_or("n/a"),
                    ));
                }
                Err(e) => parts.push(format!("context: error {e}")),
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
                match run_code(model.clone(), resolve_langs(model), id).await {
                    Ok(s) => parts.push(format!("code: {s}")),
                    Err(e) => parts.push(format!("code: error {e}")),
                }
            }
        }
        if suites.iter().any(|s| s == "agent") {
            if let Some(id) = profile_id {
                match run_agent(model.clone(), id).await {
                    Ok(s) => parts.push(format!("agent: {s}")),
                    Err(e) => parts.push(format!("agent: error {e}")),
                }
            }
        }
        // SUITE-EMB (TERM #508): embedding models profile IR retrieval quality
        // instead of context/code/agent. The driver creates its own profile row
        // (like the diffusion suite) and cleanly skips a non-embedding candidate.
        if suites.iter().any(|s| s == "embedding_retrieval") {
            match run_embedding_retrieval_suite(model).await {
                Ok(o) => parts.push(format!("embedding_retrieval: {}", o.summary)),
                Err(e) => parts.push(format!("embedding_retrieval: error {e}")),
            }
        }
        // S125 SUITE-TOOL: the tool-routing suite is self-contained (it resolves
        // its own backend + corpus and reuses the shared profile row), so it is
        // dispatched directly here rather than via an injected closure like
        // code/agent. Requested only when a model's resolved suites include it.
        if suites.iter().any(|s| s == "tool_routing") {
            if let Some(id) = profile_id {
                match run_tool_routing_suite(model, id, None).await {
                    Ok(o) => parts.push(format!(
                        "tool_routing: {} scenarios ({} rows), correct@1={}",
                        o.scenarios_run,
                        o.rows_written,
                        o.correct_tool_at_1.map(|v| format!("{:.0}%", v * 100.0)).unwrap_or_else(|| "n/a".into()),
                    )),
                    Err(e) => parts.push(format!("tool_routing: error {e}")),
                }
            }
        }

        // SUITE-VQA: the vision-QA suite runs its own corpus + image chat-route
        // path and creates its own profile row (like the single-model MCP path),
        // so it is dispatched by name here and does not share the context/code/
        // agent profile_id. A missing corpus / backend is a clean per-model note,
        // never a panic that aborts the fleet sweep.
        if suites.iter().any(|s| s == "vision_qa") {
            match run_vision_qa_suite(model).await {
                Ok(v) => parts.push(format!(
                    "vision_qa: {} items, acc={:.2}, halluc={:.2}",
                    v.items_run, v.accuracy, v.hallucination_rate
                )),
                Err(e) => parts.push(format!("vision_qa: error {e}")),
            }
        }

        // SUITE-RRK: reranking self-manages its own profile row (provider
        // "openai", Chord's /v1/rerank), independent of the Ollama-shared
        // profile_id above — a reranker never goes through the context/code/
        // agent path. A per-query error is folded into the returned summary; an
        // error here (e.g. missing corpus) degrades this model's line only.
        if suites.iter().any(|s| s == "reranking") {
            match run_reranking_suite(model).await {
                Ok(o) => parts.push(format!(
                    "reranking: uplift={:.3} ndcg={:.3} queries={}",
                    o.avg_ndcg_uplift, o.avg_reranked_ndcg, o.queries_run
                )),
                Err(e) => parts.push(format!("reranking: error {e}")),
            }
        }
        // SUITE-IMG: image-generation suite. Self-contained (its own profile row
        // + `openai` backend via Chord's `/v1/images/generations`), so it does
        // not share the Ollama `profile_id` above. Note: this fleet path first
        // `load_model`s each model via the Ollama control API, so an `openai`-kind
        // image backend (sd-turbo) reached here would typically be skipped at that
        // gate — the direct single-model tool path is the primary entry point for
        // now; this branch keeps the suite wired into the fleet driver for when an
        // image model is Ollama-loadable / the load gate is relaxed.
        if suites.iter().any(|s| s == "image_generation") {
            match run_image_generation_suite(model).await {
                Ok(o) => parts.push(format!(
                    "image_generation: {}/{} ok, avg_time_ms={:.0}",
                    o.success_count, o.prompts_run, o.avg_time_to_image_ms
                )),
                Err(e) => parts.push(format!("image_generation: error {e}")),
            }
        }
        // SUITE-DOC: document_parsing goes through Chord's `/v1/documents/parse`
        // (not the Ollama serve loaded above), and owns its own profile row, so
        // it dispatches directly here (like the diffusion suite) rather than via
        // an injected closure. A NotConfigured corpus is recorded, not fatal.
        if suites.iter().any(|s| s == "document_parsing") {
            match run_document_parsing_suite(model).await {
                Ok(o) => parts.push(format!(
                    "document_parsing: cases={} avg_field_acc={:.2}",
                    o.cases_run, o.avg_field_accuracy
                )),
                Err(e) => parts.push(format!("document_parsing: error {e}")),
            }
        }

        evict_model(&client, model).await;
        out.push(FleetSuiteResult {
            model: model.clone(),
            suites,
            summary: parts.join("; "),
            skipped: false,
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
}
