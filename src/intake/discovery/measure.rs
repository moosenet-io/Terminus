//! Ask-4 MEASURE step: backfill brochure candidates' `size_b` (and, when cheap,
//! `vram_footprint_gb`) from HuggingFace model METADATA — WITHOUT downloading any
//! weights.
//!
//! ## Why this exists — the gap it closes
//! DISC-06's discovery refresh (`refresh.rs`) records CANDIDATES from HF's public
//! *listing* API, which exposes popularity but NO parameter count, so every new
//! `model_discovery_candidate` row lands with `size_b = NULL`. The Ask-4 selector's
//! ≥7B size gate then drops every one of the ~850 candidates, and the whole
//! pull→test→promote loop selects nothing. This module is the missing "measure"
//! stage between discovery (list) and DISC-08 (fetch): for each size-`NULL`
//! candidate it fetches the repo's model-info metadata, derives `size_b` from the
//! `safetensors.total` parameter count (documented fallbacks below), and writes it
//! back via [`upsert_candidate`] — which `COALESCE`-protects already-measured rows,
//! so a real value is never clobbered by a later NULL re-observation (memory
//! `mint_brochure_curator_fixed`).
//!
//! ## Metadata only — never a weight download
//! Every HF call here is [`HfHubClient::get_model_info`], the PUBLIC per-model
//! endpoint (`GET /api/models/{repo}?blobs=true`). It reads the model card's
//! `safetensors` block and `siblings[]` file sizes — it NEVER pulls a `.safetensors`
//! blob. Pulling weights onto cold storage remains DISC-08's separate, authenticated
//! concern. No bearer token / `HF_TOKEN` is required or read: public model-info is
//! anonymous-readable, matching `hf_client`'s deliberate no-credential trust boundary
//! (see that module's "Public listing vs. DISC-08's authenticated fetch" doc). The
//! per-run fetch count is bounded by `INTAKE_DISCOVERY_MEASURE_MAX`
//! (`config::intake_discovery_measure_max`, default 50) so a one-shot backfill of a
//! large brochure spaces its calls across runs rather than hammering HF.
//!
//! ## Pure vs. impure split (testability)
//! The load-bearing logic — deriving `size_b`/`vram_footprint_gb` from a JSON blob
//! ([`measure_from_model_info`]) and choosing which rows to measure
//! ([`select_unmeasured`]) — is pure and fully unit-tested here with sample JSON;
//! only [`measure_brochure`] touches HF + Postgres (DB-gated at runtime, same
//! convention as `upsert.rs`).

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::config;
use crate::error::ToolError;
use crate::intake::discovery::hf_client::{HfHubClient, HfListError};
use crate::intake::discovery::schema::DiscoveryCandidate;
use crate::intake::discovery::storage::read_brochure;
use crate::intake::discovery::upsert::upsert_candidate;
use crate::intake::storage as intake_storage;
use crate::registry::ToolRegistry;
use crate::tool::{RustTool, ToolOutput};

/// Assumed bytes-per-parameter for the weight-byte-sum fallback size estimate.
/// Modern HF weight repos are overwhelmingly published at 16-bit precision
/// (`F16`/`BF16` → 2 bytes/param), so `param_count ≈ total_weight_bytes / 2`.
/// This is ONLY a fallback when the authoritative `safetensors.total` param count
/// is absent — an 8-bit or 4-bit-quantized repo would over-estimate here, which is
/// why it ranks below the exact param count.
const BYTES_PER_PARAM_FP16: f64 = 2.0;

/// One billion — params → size_b (billions) and bytes → GB divisor base.
const BILLION: f64 = 1e9;

/// The measured metadata derived from one repo's HF model-info blob. Either field
/// is `None` when the metadata didn't support deriving it (a repo with no
/// `safetensors` block, no parseable size, and no per-file byte sizes yields
/// `{ None, None }` — the caller then leaves that candidate's columns NULL).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MeasuredMetadata {
    /// Model size in BILLIONS of parameters (the brochure's `size_b`).
    pub size_b: Option<f64>,
    /// Rough on-disk / VRAM footprint in GB (weight-file bytes / 1e9), when the
    /// `?blobs=true` response carried per-file `size`s. Loading overhead
    /// (KV-cache, activations) is NOT modelled — this is the weights-at-rest
    /// footprint only, an under-estimate of true inference VRAM.
    pub vram_footprint_gb: Option<f64>,
}

/// Derive `size_b`/`vram_footprint_gb` from one repo's HF model-info JSON.
///
/// PURE — no IO. `repo_id` is used only for the last-resort id-token fallback.
///
/// ### `size_b` derivation precedence (documented, most→least authoritative)
/// 1. **`safetensors.total`** — HF's exact total parameter count for the repo's
///    safetensors weights. `total / 1e9`. This is the primary, trusted signal.
/// 2. **weight-file byte sum** — sum of every `*.safetensors` sibling's `size`
///    (present with `?blobs=true`) ÷ [`BYTES_PER_PARAM_FP16`] ÷ 1e9. A rough
///    estimate used only when (1) is absent; assumes 16-bit weights.
/// 3. **repo-id token** — an `NNb`/`NNB` size token in the repo id
///    (e.g. `Qwen/Qwen3-8B` → 8.0, `.../Model-1.5b` → 1.5), taken verbatim as
///    billions. Last resort: naming is a convention, not a guarantee.
///
/// The first rule that yields a positive value wins; if none do, `size_b` is
/// `None` and the candidate stays unmeasured (measure is retried next run).
pub fn measure_from_model_info(info: &Value, repo_id: &str) -> MeasuredMetadata {
    let weight_bytes = weight_file_bytes(info);

    let size_b = size_b_from_safetensors_total(info)
        .or_else(|| size_b_from_weight_bytes(weight_bytes))
        .or_else(|| size_b_from_repo_id(repo_id));

    let vram_footprint_gb = weight_bytes.filter(|&b| b > 0).map(|b| b as f64 / BILLION);

    MeasuredMetadata {
        size_b,
        vram_footprint_gb,
    }
}

/// Rule 1: `safetensors.total` (or, if only the per-dtype `parameters` map is
/// present, the sum of its values) → billions of params. Ignores a zero/negative
/// or non-numeric value (treated as absent so the next rule runs).
fn size_b_from_safetensors_total(info: &Value) -> Option<f64> {
    let st = info.get("safetensors")?;
    // Preferred: the explicit `total`.
    if let Some(total) = st.get("total").and_then(json_as_f64) {
        if total > 0.0 {
            return Some(total / BILLION);
        }
    }
    // Fallback within rule 1: sum the per-dtype `parameters` map
    // (e.g. {"BF16": 8030261248}) when `total` is missing.
    if let Some(params) = st.get("parameters").and_then(Value::as_object) {
        let sum: f64 = params
            .values()
            .filter_map(json_as_f64)
            .filter(|v| *v > 0.0)
            .sum();
        if sum > 0.0 {
            return Some(sum / BILLION);
        }
    }
    None
}

/// Rule 2: total `*.safetensors` byte size ÷ bytes-per-param ÷ 1e9.
fn size_b_from_weight_bytes(weight_bytes: Option<u64>) -> Option<f64> {
    match weight_bytes {
        Some(b) if b > 0 => Some(b as f64 / BYTES_PER_PARAM_FP16 / BILLION),
        _ => None,
    }
}

/// Rule 3: parse an `NNb`/`NNB` (optionally decimal, e.g. `1.5b`) size token out
/// of the repo id. Scans dash/slash/underscore-delimited chunks and returns the
/// first that looks like `<number>b`. Case-insensitive on the `b`. Returns `None`
/// when no chunk matches (naming is only a last-resort hint).
fn size_b_from_repo_id(repo_id: &str) -> Option<f64> {
    // Try a DECIMAL token first (e.g. "1.5b"): the '.'-split below would break
    // "1.5b" into "1" and "5b" and spuriously match the "5b" fragment as 5.0, so
    // the whole-string decimal scan must win before any chunk match.
    if let Some(v) = parse_decimal_b_token(repo_id) {
        return Some(v);
    }
    for chunk in repo_id.split(|c| c == '-' || c == '/' || c == '_' || c == '.') {
        if let Some(v) = parse_size_token(chunk) {
            return Some(v);
        }
    }
    None
}

/// Parse a single chunk that is exactly `<digits>b`/`<digits>B` (no decimal point,
/// since '.' is a split delimiter above) → the number as billions. Rejects a bare
/// number, a chunk with other letters ("bert"), or an out-of-range value.
fn parse_size_token(chunk: &str) -> Option<f64> {
    let lower = chunk.to_ascii_lowercase();
    let digits = lower.strip_suffix('b')?;
    if digits.is_empty() || !digits.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let n: f64 = digits.parse().ok()?;
    // Sanity bound: a plausible model size in billions of params. Rejects e.g. a
    // spurious "500b" hash-like token while allowing today's largest open models.
    if (0.0..=2000.0).contains(&n) && n > 0.0 {
        Some(n)
    } else {
        None
    }
}

/// Handle a decimal size token like `1.5b`/`0.5B` anywhere in the id (the '.'  is
/// a split delimiter in [`size_b_from_repo_id`], so those need this dedicated
/// scan). Finds `<int>.<frac>b` and returns it as billions.
fn parse_decimal_b_token(repo_id: &str) -> Option<f64> {
    let lower = repo_id.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let start = i;
            let mut saw_dot = false;
            while i < bytes.len() && (bytes[i].is_ascii_digit() || (bytes[i] == b'.' && !saw_dot)) {
                if bytes[i] == b'.' {
                    saw_dot = true;
                }
                i += 1;
            }
            if saw_dot && i < bytes.len() && bytes[i] == b'b' {
                if let Ok(n) = lower[start..i].parse::<f64>() {
                    if (0.0..=2000.0).contains(&n) && n > 0.0 {
                        return Some(n);
                    }
                }
            }
        } else {
            i += 1;
        }
    }
    None
}

/// Sum the byte `size` of every `*.safetensors` sibling in the model-info blob.
/// Returns `None` when the response carries no sibling sizes at all (no
/// `?blobs=true`, or a repo with no safetensors weights) — distinct from
/// `Some(0)`. Siblings without a numeric `size` (or non-`.safetensors` files) are
/// simply not counted.
fn weight_file_bytes(info: &Value) -> Option<u64> {
    let siblings = info.get("siblings").and_then(Value::as_array)?;
    let mut total: u64 = 0;
    let mut saw_any_size = false;
    for s in siblings {
        let name = s.get("rfilename").and_then(Value::as_str).unwrap_or("");
        if !name.ends_with(".safetensors") {
            continue;
        }
        if let Some(sz) = s.get("size").and_then(json_as_u64) {
            total = total.saturating_add(sz);
            saw_any_size = true;
        }
    }
    if saw_any_size {
        Some(total)
    } else {
        None
    }
}

/// Read a JSON number as `f64` whether it deserialized as int or float.
fn json_as_f64(v: &Value) -> Option<f64> {
    v.as_f64()
        .or_else(|| v.as_u64().map(|u| u as f64))
        .or_else(|| v.as_i64().map(|i| i as f64))
}

/// Read a JSON number as `u64` (bytes are non-negative integers on HF's API).
fn json_as_u64(v: &Value) -> Option<u64> {
    v.as_u64()
        .or_else(|| v.as_f64().filter(|f| *f >= 0.0).map(|f| f as u64))
}

/// Resolve the effective per-run cap from the optional `max` MCP arg and the
/// configured `ceiling` (`INTAKE_DISCOVERY_MEASURE_MAX`). PURE — no IO.
///
/// A caller-supplied `max` may only LOWER the cap: it is clamped to `ceiling`
/// (`min(requested, ceiling)`), never allowed above it, so the advertised
/// outbound-HF bound always holds. An absent `max` uses `ceiling` directly. A
/// present-but-not-a-positive-integer `max` is a clean [`ToolError::InvalidArgument`].
fn effective_cap(max_arg: Option<&Value>, ceiling: usize) -> Result<usize, ToolError> {
    match max_arg {
        None => Ok(ceiling),
        Some(v) => {
            let requested = v
                .as_u64()
                .filter(|n| *n > 0)
                .map(|n| n as usize)
                .ok_or_else(|| {
                    ToolError::InvalidArgument("'max' must be a positive integer".into())
                })?;
            Ok(requested.min(ceiling))
        }
    }
}

/// PURE selection: the candidates that still need measuring (`size_b` is `None`),
/// capped at `cap`. Preserves input order (`read_brochure` returns rows ordered by
/// `model_name`, so a capped run is deterministic and, across runs, walks the
/// backlog stably). A `cap` of 0 selects nothing.
pub fn select_unmeasured(
    candidates: &[DiscoveryCandidate],
    cap: usize,
) -> Vec<&DiscoveryCandidate> {
    candidates
        .iter()
        .filter(|c| c.size_b.is_none())
        .take(cap)
        .collect()
}

/// Outcome of one measure pass, surfaced in the tool result.
pub struct MeasureOutcome {
    /// How many size-NULL candidates existed before this pass (the remaining backlog).
    pub unmeasured_total: usize,
    /// How many this pass attempted (`min(unmeasured_total, cap)`).
    pub attempted: usize,
    /// How many got a non-NULL `size_b` written back.
    pub measured: usize,
    /// Attempted but HF metadata yielded no size (left NULL, retried next run).
    pub unresolved: usize,
    /// `(hf_repo, error)` — a per-candidate HF/DB failure that did NOT abort the pass.
    pub errors: Vec<(String, String)>,
}

/// Run one measure pass over the brochure: select up to `cap` size-`NULL`
/// candidates, fetch each repo's HF model-info metadata, derive `size_b`
/// (+ optional `vram_footprint_gb`), and write it back via [`upsert_candidate`].
///
/// - Idempotent: already-measured rows (`size_b IS NOT NULL`) are never re-fetched.
/// - Fail-soft per candidate: a repo with no safetensors metadata, a 404, or a
///   transient HF error is logged and skipped (its `size_b` stays NULL for a later
///   run) — one bad repo never aborts the pass.
/// - Metadata only: no weight bytes are ever downloaded.
/// - When `dry_run` is true, candidates are selected + fetched + derived but NOTHING
///   is written (preview of what WOULD be measured). `pool` is `None` iff `dry_run`.
pub async fn measure_brochure(
    pool: Option<&sqlx::PgPool>,
    client: &HfHubClient,
    all_candidates: &[DiscoveryCandidate],
    cap: usize,
) -> MeasureOutcome {
    let unmeasured_total = all_candidates.iter().filter(|c| c.size_b.is_none()).count();
    let targets = select_unmeasured(all_candidates, cap);
    let attempted = targets.len();
    let mut measured = 0usize;
    let mut unresolved = 0usize;
    let mut errors = Vec::new();

    for cand in targets {
        match client.get_model_info(&cand.hf_repo).await {
            Ok(info) => {
                let m = measure_from_model_info(&info, &cand.hf_repo);
                match m.size_b {
                    Some(size_b) if size_b > 0.0 => {
                        match pool {
                            // dry-run: count what WOULD be measured, write nothing.
                            None => measured += 1,
                            Some(p) => {
                                // Carry the existing row forward with ONLY the
                                // measured fields populated. upsert_candidate
                                // COALESCE-protects size_b/vram/gfx1151 and never
                                // touches `status`, so this writes the new size_b
                                // (non-NULL wins) without disturbing lifecycle
                                // state or a prior measurement.
                                let mut updated = cand.clone();
                                updated.size_b = Some(size_b);
                                if m.vram_footprint_gb.is_some() {
                                    updated.vram_footprint_gb = m.vram_footprint_gb;
                                }
                                match upsert_candidate(p, &updated).await {
                                    Ok(()) => measured += 1,
                                    Err(e) => errors.push((cand.hf_repo.clone(), e.to_string())),
                                }
                            }
                        }
                    }
                    _ => {
                        unresolved += 1;
                        tracing::warn!(
                            hf_repo = %cand.hf_repo,
                            "measure: HF model-info exposed no usable size_b (no safetensors \
                             param count / weight bytes / id token) — leaving size_b NULL, \
                             will retry next run"
                        );
                    }
                }
            }
            Err(e) => {
                // A 404 (gated/removed repo) or transient error is fail-soft:
                // record it and move on, never abort the whole pass.
                let detail = match &e {
                    HfListError::Failed { status, .. } => format!("HTTP {status}"),
                    HfListError::Unreachable { .. } => "unreachable".to_string(),
                };
                tracing::warn!(hf_repo = %cand.hf_repo, "measure: HF model-info fetch failed ({detail}) — skipping");
                errors.push((cand.hf_repo.clone(), e.to_string()));
            }
        }
    }

    MeasureOutcome {
        unmeasured_total,
        attempted,
        measured,
        unresolved,
        errors,
    }
}

/// Ask-4 MCP tool: `model_discovery_measure` — the MEASURE step. Backfills
/// size-`NULL` brochure candidates' `size_b`/`vram_footprint_gb` from HF model
/// metadata so the selector's size gate can actually see them. Distinct from
/// `model_discovery_refresh` (which lists new candidates but leaves size NULL).
pub struct ModelDiscoveryMeasure;

impl ModelDiscoveryMeasure {
    async fn run(&self, args: Value) -> Result<Value, ToolError> {
        let dry_run = args
            .get("dry_run")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        // `max` arg may lower the per-run cap for this pass, but NEVER raise it
        // above the configured `INTAKE_DISCOVERY_MEASURE_MAX` ceiling: that env
        // value is the advertised hard bound on outbound HF model-info calls per
        // run, so a caller-supplied `max` is clamped down to it (a larger `max`
        // is silently capped, not honored). This keeps the bound the tool
        // description + config.rs comment promise actually true.
        let ceiling = config::intake_discovery_measure_max();
        let cap = effective_cap(args.get("max"), ceiling)?;

        let pool = intake_storage::get_pool().await?;
        let all = read_brochure(&pool).await?;
        let client = HfHubClient::new();
        let pass_pool = if dry_run { None } else { Some(&pool) };
        let outcome = measure_brochure(pass_pool, &client, &all, cap).await;

        Ok(json!({
            "dry_run": dry_run,
            "cap": cap,
            "unmeasured_total": outcome.unmeasured_total,
            "attempted": outcome.attempted,
            "measured": outcome.measured,
            "unresolved": outcome.unresolved,
            "remaining_after": outcome.unmeasured_total.saturating_sub(outcome.measured),
            "errors": outcome.errors.iter().map(|(k, v)| json!({"where": k, "error": v})).collect::<Vec<_>>(),
            "note": if dry_run {
                "dry run — nothing was written; counts reflect what WOULD be measured"
            } else {
                "brochure measured; size-NULL candidates now carry size_b where HF metadata allowed"
            },
        }))
    }
}

#[async_trait]
impl RustTool for ModelDiscoveryMeasure {
    fn name(&self) -> &str {
        "model_discovery_measure"
    }

    fn description(&self) -> &str {
        "Measure brochure candidates' size (Ask-4 MEASURE step): for up to \
         INTAKE_DISCOVERY_MEASURE_MAX (default 50) candidates whose size_b is still \
         NULL, fetch the HuggingFace model-info METADATA (public /api/models/{repo}, \
         NO token, NO weight download) and derive size_b (billions of params from \
         safetensors.total; fallbacks: weight-file byte sum, then an NNb repo-id \
         token) plus a rough vram_footprint_gb from weight-file bytes. Writes back \
         via the COALESCE-protected upsert (never clobbers an existing measurement, \
         never touches lifecycle status). This unblocks the selector's >=7B size gate, \
         which drops every size-NULL candidate. Idempotent + fail-soft: already-measured \
         rows are skipped; a repo with no safetensors metadata (or a 404) is left NULL \
         and retried next run. Args (all optional): 'max' (positive int; LOWER the \
         per-run cap for this pass — clamped to INTAKE_DISCOVERY_MEASURE_MAX, never \
         above it), 'dry_run' (bool; select+fetch+derive but write nothing). Returns \
         per-pass counts (unmeasured_total, attempted, measured, unresolved, \
         remaining_after) + any per-candidate errors."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "max": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Lower the per-run measure cap for this pass. Clamped to INTAKE_DISCOVERY_MEASURE_MAX (default 50) — a larger value is capped down, never raised above it, so the outbound HF model-info bound always holds."
                },
                "dry_run": {
                    "type": "boolean",
                    "description": "If true, select + fetch + derive sizes but write nothing to the brochure (preview)."
                }
            }
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let v = self.run(args).await?;
        Ok(serde_json::to_string_pretty(&v).unwrap_or_else(|_| v.to_string()))
    }

    async fn execute_structured(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let v = self.run(args).await?;
        let text = serde_json::to_string_pretty(&v).unwrap_or_else(|_| v.to_string());
        Ok(ToolOutput {
            text,
            structured: Some(v),
        })
    }
}

/// Register the Ask-4 measure tool on the CORE registry (wired into
/// `crate::intake::discovery::register` alongside the read + refresh tools).
pub fn register(registry: &mut ToolRegistry) {
    registry.register_or_replace(Box::new(ModelDiscoveryMeasure));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intake::discovery::schema::{CandidateStatus, FleetCategory};

    fn candidate(repo: &str, size_b: Option<f64>) -> DiscoveryCandidate {
        let now = chrono::Utc::now();
        DiscoveryCandidate {
            model_name: repo.to_string(),
            hf_repo: repo.to_string(),
            category: FleetCategory::Assistant,
            status: CandidateStatus::Discovered,
            modality: None,
            gfx1151_class: "unknown".to_string(),
            size_b,
            vram_footprint_gb: None,
            discovery_source: "huggingface_hub".to_string(),
            discovery_score: Some(1.0),
            discovered_at: now,
            last_seen_at: now,
            fetched_at: None,
            marked_for_fleet_at: None,
            evicted_at: None,
            retained_profile: None,
            rationale: None,
        }
    }

    // ---- size_b derivation: rule 1 (safetensors.total) ----

    #[test]
    fn size_b_from_safetensors_total_is_params_over_1e9() {
        let info = json!({ "safetensors": { "total": 8_030_261_248u64 } });
        let m = measure_from_model_info(&info, "org/whatever");
        // 8.03e9 params → ~8.03 B.
        let got = m.size_b.expect("size_b");
        assert!((got - 8.030_261_248).abs() < 1e-6, "got {got}");
    }

    #[test]
    fn size_b_falls_back_to_summing_parameters_map_when_total_missing() {
        let info = json!({ "safetensors": { "parameters": { "BF16": 7_000_000_000u64, "F32": 241_000u64 } } });
        let m = measure_from_model_info(&info, "org/whatever");
        let got = m.size_b.expect("size_b");
        assert!((got - 7.000_241).abs() < 1e-3, "got {got}");
    }

    #[test]
    fn safetensors_total_wins_over_id_token() {
        // Repo id says 7B but safetensors says ~13B — the exact param count wins.
        let info = json!({ "safetensors": { "total": 13_000_000_000u64 } });
        let m = measure_from_model_info(&info, "org/Model-7B");
        assert!((m.size_b.unwrap() - 13.0).abs() < 1e-6);
    }

    // ---- size_b derivation: rule 2 (weight-byte sum) ----

    #[test]
    fn size_b_falls_back_to_weight_bytes_when_no_safetensors_block() {
        // 14e9 bytes of fp16 weights → ~7B params (14e9 / 2 / 1e9).
        let info = json!({
            "siblings": [
                { "rfilename": "model-00001-of-00002.safetensors", "size": 7_000_000_000u64 },
                { "rfilename": "model-00002-of-00002.safetensors", "size": 7_000_000_000u64 },
                { "rfilename": "config.json", "size": 1234u64 },
            ]
        });
        let m = measure_from_model_info(&info, "org/no-safetensors-meta");
        let got = m.size_b.expect("size_b via byte sum");
        assert!((got - 7.0).abs() < 1e-6, "got {got}");
        // vram footprint = total weight bytes / 1e9 = 14 GB.
        assert!((m.vram_footprint_gb.unwrap() - 14.0).abs() < 1e-6);
    }

    #[test]
    fn weight_bytes_ignores_non_safetensors_files() {
        let info = json!({
            "siblings": [
                { "rfilename": "pytorch_model.bin", "size": 999_000_000_000u64 },
                { "rfilename": "model.safetensors", "size": 2_000_000_000u64 },
            ]
        });
        // Only the .safetensors 2e9 bytes count → 1B params, 2 GB footprint.
        let m = measure_from_model_info(&info, "org/m");
        assert!((m.size_b.unwrap() - 1.0).abs() < 1e-6);
        assert!((m.vram_footprint_gb.unwrap() - 2.0).abs() < 1e-6);
    }

    // ---- size_b derivation: rule 3 (repo-id token) ----

    #[test]
    fn size_b_falls_back_to_repo_id_token() {
        let m = measure_from_model_info(&json!({}), "Qwen/Qwen3-8B");
        assert_eq!(m.size_b, Some(8.0));
        assert!(m.vram_footprint_gb.is_none(), "no bytes → no footprint");
    }

    #[test]
    fn size_b_repo_id_token_handles_decimal_and_case() {
        assert_eq!(
            measure_from_model_info(&json!({}), "org/Tiny-1.5b-chat").size_b,
            Some(1.5)
        );
        assert_eq!(
            measure_from_model_info(&json!({}), "org/Model-32B-Instruct").size_b,
            Some(32.0)
        );
        assert_eq!(
            measure_from_model_info(&json!({}), "org/Half-0.5B").size_b,
            Some(0.5)
        );
    }

    #[test]
    fn size_b_repo_id_token_ignores_non_size_words() {
        // "bert" ends in 'b'-ish but isn't <digits>b; no numeric token at all.
        assert_eq!(
            measure_from_model_info(&json!({}), "google/bert-base-uncased").size_b,
            None
        );
    }

    // ---- missing / malformed metadata → None, never panic ----

    #[test]
    fn no_metadata_and_no_id_token_yields_none() {
        let m = measure_from_model_info(&json!({}), "org/nameless");
        assert_eq!(m.size_b, None);
        assert_eq!(m.vram_footprint_gb, None);
    }

    #[test]
    fn zero_or_negative_total_is_treated_as_absent() {
        let info = json!({ "safetensors": { "total": 0 } });
        // Falls through to id token (none here) → None, not Some(0).
        assert_eq!(measure_from_model_info(&info, "org/zero").size_b, None);
    }

    #[test]
    fn malformed_shapes_do_not_panic() {
        // safetensors is a string, siblings is a number, total is a string.
        let weird = json!({ "safetensors": "nope", "siblings": 5 });
        assert_eq!(measure_from_model_info(&weird, "org/x").size_b, None);
        let weird2 =
            json!({ "safetensors": { "total": "eight-billion" }, "siblings": [ 1, 2, 3 ] });
        assert_eq!(measure_from_model_info(&weird2, "org/y").size_b, None);
        // Sibling missing size / missing rfilename: no panic, no bytes counted.
        let weird3 =
            json!({ "siblings": [ { "rfilename": "model.safetensors" }, { "size": 10 } ] });
        assert_eq!(weight_file_bytes(&weird3), None);
    }

    #[test]
    fn siblings_without_any_sizes_is_none_not_zero() {
        // No ?blobs=true → siblings carry names but no sizes.
        let info = json!({ "siblings": [ { "rfilename": "model.safetensors" } ] });
        assert_eq!(weight_file_bytes(&info), None);
        assert_eq!(measure_from_model_info(&info, "org/unsized").size_b, None);
    }

    // ---- effective_cap: `max` may only lower, never raise, the ceiling ----

    #[test]
    fn effective_cap_clamps_max_down_to_ceiling_never_up() {
        // Absent `max` → the ceiling itself.
        assert_eq!(effective_cap(None, 50).unwrap(), 50);
        // A smaller `max` lowers the cap.
        assert_eq!(effective_cap(Some(&json!(10)), 50).unwrap(), 10);
        // A larger `max` is clamped DOWN to the ceiling (the bug this fixes).
        assert_eq!(effective_cap(Some(&json!(9999)), 50).unwrap(), 50);
        // Equal is fine.
        assert_eq!(effective_cap(Some(&json!(50)), 50).unwrap(), 50);
    }

    #[test]
    fn effective_cap_rejects_non_positive_or_non_integer_max() {
        assert!(effective_cap(Some(&json!(0)), 50).is_err());
        assert!(effective_cap(Some(&json!(-3)), 50).is_err());
        assert!(effective_cap(Some(&json!("lots")), 50).is_err());
    }

    // ---- select_unmeasured: only size-NULL rows, cap respected ----

    #[test]
    fn select_unmeasured_picks_only_null_size_rows() {
        let cands = vec![
            candidate("a/measured", Some(7.0)),
            candidate("b/null", None),
            candidate("c/measured", Some(13.0)),
            candidate("d/null", None),
        ];
        let picked = select_unmeasured(&cands, 10);
        let repos: Vec<&str> = picked.iter().map(|c| c.hf_repo.as_str()).collect();
        assert_eq!(repos, vec!["b/null", "d/null"], "only size-NULL rows");
    }

    #[test]
    fn select_unmeasured_respects_cap_and_order() {
        let cands = vec![
            candidate("a/null", None),
            candidate("b/null", None),
            candidate("c/null", None),
        ];
        let picked = select_unmeasured(&cands, 2);
        assert_eq!(picked.len(), 2, "cap of 2 honored");
        assert_eq!(picked[0].hf_repo, "a/null");
        assert_eq!(picked[1].hf_repo, "b/null");
        // A cap of 0 selects nothing.
        assert!(select_unmeasured(&cands, 0).is_empty());
    }

    #[test]
    fn select_unmeasured_empty_when_all_measured() {
        let cands = vec![candidate("a", Some(7.0)), candidate("b", Some(8.0))];
        assert!(select_unmeasured(&cands, 50).is_empty());
    }

    // ---- measure_brochure dry-run: no write, correct counts, COALESCE intent ----

    #[tokio::test]
    async fn measure_brochure_dry_run_counts_without_writing() {
        use httpmock::prelude::*;
        let server = MockServer::start();
        // Two size-NULL candidates; the mock serves safetensors metadata for both.
        let _mock = server.mock(|when, then| {
            when.method(GET).path_contains("/api/models/");
            then.status(200)
                .json_body(json!({ "safetensors": { "total": 8_000_000_000u64 } }));
        });
        let client = HfHubClient::with_base_url(server.base_url());
        let cands = vec![
            candidate("org/a-null", None),
            candidate("org/b-measured", Some(7.0)),
            candidate("org/c-null", None),
        ];
        // pool = None (dry run) → nothing written, but derivations still counted.
        let outcome = measure_brochure(None, &client, &cands, 50).await;
        assert_eq!(outcome.unmeasured_total, 2, "two size-NULL rows");
        assert_eq!(outcome.attempted, 2);
        assert_eq!(outcome.measured, 2, "both derived a size in dry-run");
        assert_eq!(outcome.unresolved, 0);
        assert!(outcome.errors.is_empty());
    }

    #[tokio::test]
    async fn measure_brochure_is_fail_soft_on_404_and_unresolved() {
        use httpmock::prelude::*;
        let server = MockServer::start();
        // a-null → 404 (fail-soft error), b-null → metadata with no size (unresolved).
        let _m404 = server.mock(|when, then| {
            when.method(GET).path_contains("/api/models/org/a-null");
            then.status(404).body("not found");
        });
        let _mempty = server.mock(|when, then| {
            when.method(GET).path_contains("/api/models/org/b-null");
            then.status(200).json_body(json!({ "siblings": [] }));
        });
        let client = HfHubClient::with_base_url(server.base_url());
        let cands = vec![candidate("org/a-null", None), candidate("org/b-null", None)];
        let outcome = measure_brochure(None, &client, &cands, 50).await;
        assert_eq!(outcome.attempted, 2);
        assert_eq!(outcome.measured, 0, "neither yielded a size");
        assert_eq!(outcome.unresolved, 1, "b-null: metadata but no size");
        assert_eq!(outcome.errors.len(), 1, "a-null: 404 recorded, not fatal");
        assert!(outcome.errors[0].0.contains("a-null"));
    }

    #[tokio::test]
    async fn measure_brochure_cap_bounds_fetches() {
        use httpmock::prelude::*;
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path_contains("/api/models/");
            then.status(200)
                .json_body(json!({ "safetensors": { "total": 8_000_000_000u64 } }));
        });
        let client = HfHubClient::with_base_url(server.base_url());
        let cands = vec![
            candidate("org/a-null", None),
            candidate("org/b-null", None),
            candidate("org/c-null", None),
        ];
        // Cap of 1 → only ONE HF fetch, even though three rows need measuring.
        let outcome = measure_brochure(None, &client, &cands, 1).await;
        assert_eq!(
            outcome.unmeasured_total, 3,
            "backlog still reflects all three"
        );
        assert_eq!(outcome.attempted, 1, "cap bounds the pass to one");
        assert_eq!(outcome.measured, 1);
        mock.assert_hits(1);
    }
}
