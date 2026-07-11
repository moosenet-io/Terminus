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
    let base = context::ollama_base();
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
    let base = context::ollama_base();
    // Evict the model we just profiled.
    let _ = client
        .post(format!("{base}/api/generate"))
        .json(&serde_json::json!({ "model": evict, "keep_alive": 0 }))
        .timeout(Duration::from_secs(60))
        .send()
        .await;
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
    let pool = storage::get_pool().await?;
    storage::insert_model_profile(&pool, model_name, "ollama", None, None).await
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
    let base = context::ollama_base();
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
) -> Vec<FleetSuiteResult> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(900))
        .build()
        .expect("client");
    let prior_hot = current_hot_model(&client).await;
    let mut out = Vec::new();

    for model in models {
        let suites = resolve_suites(model);
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
        let needs = suites.iter().any(|s| s == "code" || s == "agent");
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
