//! CONST-21: the Models/MINT read API — `/api/terminus/models*` +
//! `/api/terminus/mint/*` (spec `docs/constellation/CONST-GUI-SPEC.md` §8, the
//! ground truth in `docs/constellation/CONST-GUI-audit.md` §5).
//!
//! ## What this is
//! A read-only aggregation surface over the intake Postgres data this crate's
//! MCP tools (`model_intake*`, `model_fleet_catalog*`, `model_discovery_*`,
//! `model_advisor_*`) already own — there was previously **no HTTP/JSON
//! surface** for any of it (audit §5). This module adds one, entirely by
//! REUSING the existing read layer (`crate::intake::{storage, catalog,
//! discovery}`, `crate::model_advisor`) — it never opens a second database
//! pool or connection (every query here goes through
//! `crate::intake::storage::get_pool`, the SAME pool helper the MCP tools
//! use) and never calls out to another MCP tool over the wire.
//!
//! ## Contract with `constellation-web`
//! Every handler here: (1) sits behind `crate::constellation::auth::
//! require_session` (wired in `crate::constellation::mod::protected_router`,
//! not in this module — this module has no auth logic of its own); (2) passes
//! its JSON body through `crate::constellation::mask::mask_response` before
//! responding, exactly like every other `/api/*` handler in this crate; (3) is
//! a plain read-only `GET`, so there is nothing for
//! `crate::constellation::audit` to record (that module only ever records
//! mutating methods, and this module registers none).
//!
//! ## Graceful degradation (empty, not error)
//! Mirroring `crate::constellation::proxy`'s "a down backend is a successful
//! 200, not a 500" philosophy: an unconfigured/unreachable intake Postgres, an
//! un-migrated table, or an unknown filter value never surfaces as a `5xx` —
//! every list endpoint degrades to its empty-array shape (`total: 0`, `models:
//! []`, etc.) and every summary endpoint degrades to its zeroed shape. The ONE
//! deliberate exception is `GET /api/terminus/models/{name}`, which reports a
//! real `404` — but ONLY when the requested name is absent from every source
//! this module can see (spec §8's "absent sources are null, never 404 unless
//! the name is unknown everywhere").
//!
//! ## Contracts-to-confirm (spec §8 / audit §5) — resolved for THIS build
//! 1. `model_profiles.profile_date`: per the audit's pinned finding, the live
//!    intake DB carries this column out-of-band (this checkout's own `CREATE
//!    TABLE model_profiles` in `src/intake/assistant/schema.rs` has no such
//!    column, only `created_at`). `storage::read_latest_operational_profile_
//!    for_model` orders by `COALESCE(mp.profile_date, mp.created_at) DESC`
//!    per that finding. The sanctioned read-only `pg_*` tool was tried
//!    against the live intake DB this build session and connected to a
//!    database reporting ZERO tables, so this could not be independently
//!    re-confirmed this session — implemented per the audit's finding
//!    regardless, and SAFE either way: a host whose `model_profiles` truly
//!    lacks `profile_date` gets a missing-column error, which that
//!    function's read degrades to `None` for (never a hard failure), rather
//!    than falling back to silently wrong ordering.
//! 2. `quant` is treated as `Option<String>` everywhere in this module (list,
//!    detail, matrix) — never unwrapped.
//! 3. Code/agent catalog cells legitimately read `not_run` until
//!    `INTAKE_CORPUS_V2_DIR` is provisioned — `/api/terminus/mint/matrix`
//!    reports exactly what's persisted; no synthetic backfill.
//! 4. The brochure `category` filter validates against the FULL 8-value
//!    [`crate::intake::discovery::schema::FleetCategory::ALL`] (not the
//!    MCP-tool reader's 7-value drift) — see [`valid_category_or_400`].
//! 5. Epoch scoping follows [`crate::intake::EpochSelector`] verbatim:
//!    `epoch` absent ⇒ `Current`, `epoch=all` ⇒ `All`, else ⇒ `Only(value)` —
//!    see [`epoch_selector_from_query`].

use axum::extract::{Path, Query};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use sqlx::PgPool;
use std::collections::{BTreeMap, BTreeSet};

use crate::constellation::mask::mask_response;
use crate::intake::catalog::{self, CatalogQuery, StoredCatalogCard};
use crate::intake::discovery::schema::{DiscoveryCandidate, FleetCategory};
use crate::intake::{discovery, storage, EpochSelector};

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// The 8 assistant-suite dimension constants (spec §7.2-C1 / audit §5),
/// matching each `dimN_*.rs`/`fleet.rs` module's own `pub const DIMENSION` —
/// listed here as a fixed, ordered array (not re-exported from 7 separate
/// modules) because the radar's axis ORDER is itself part of the contract
/// (spec §7.2-C1: "8 axes … in fixed order").
const ASSISTANT_DIMENSIONS: [&str; 8] = [
    "conversation_depth",
    "tool_chaining",
    "memory_integration",
    "personality_latent",
    "personality_prompted",
    "embeddings",
    "yarn_context_depth",
    "fleet_membership",
];

/// Min–max normalize `v` into `lo..=hi` → `0.0..=1.0` (spec §7.2-C1: the
/// capability radar's fleet-wide per-dimension normalization). A degenerate
/// range (`lo == hi`, e.g. a dimension with only one distinct fleet value)
/// normalizes to the midpoint `0.5` rather than dividing by zero — there is
/// no meaningful "where in the range" answer when there IS no range. Pure —
/// unit-tested directly (`mint_dimensions` calls this via its `normalize`
/// closure, which only adds the fleet-wide range lookup).
fn normalize_min_max(lo: f64, hi: f64, v: f64) -> f64 {
    if (hi - lo).abs() > f64::EPSILON {
        (v - lo) / (hi - lo)
    } else {
        0.5
    }
}

/// √-scale `vram_gb` into an 8–24px point-size for the C4 Pareto scatter
/// (spec §7.2-C4: "point size = vram_gb (√-scaled, 8–24px)") — computed
/// server-side (over the fleet-wide min/max of the RETURNED rows) so the
/// browser never re-derives the pixel encoding from a raw GB figure itself.
/// `None`/non-positive `vram_gb` floors to `8.0` (the smallest mark, "unknown
/// footprint" reads as "small" rather than crashing or omitting the point).
/// A degenerate fleet-wide range (every row the same VRAM, or only one row)
/// maps to the midpoint `16.0`. Pure — unit-tested on fixtures.
fn pareto_point_size_px(vram_gb: Option<f64>, min_vram_gb: f64, max_vram_gb: f64) -> f64 {
    const FLOOR_PX: f64 = 8.0;
    const CEIL_PX: f64 = 24.0;
    let Some(v) = vram_gb.filter(|v| *v > 0.0) else {
        return FLOOR_PX;
    };
    if min_vram_gb <= 0.0 || (max_vram_gb.sqrt() - min_vram_gb.sqrt()).abs() < f64::EPSILON {
        return (FLOOR_PX + CEIL_PX) / 2.0;
    }
    let t = ((v.sqrt() - min_vram_gb.sqrt()) / (max_vram_gb.sqrt() - min_vram_gb.sqrt())).clamp(0.0, 1.0);
    FLOOR_PX + t * (CEIL_PX - FLOOR_PX)
}

/// Fold a fleet-wide (class, total-count) ranking down to the top `n` class
/// names plus everything else — spec §7.2-C6 / §8's "top5 + other" contract
/// (13 known `failure_class` values exceeds both the class ceiling and the
/// 6-slot brand palette). Ties break by class name for determinism. Pure —
/// unit-tested on fixtures (`mint_failures` calls this over the live
/// per-class totals it computes from `read_failure_class_counts`).
fn top_n_classes<'a>(class_totals: &BTreeMap<&'a str, i64>, n: usize) -> BTreeSet<&'a str> {
    let mut ranked: Vec<(&str, i64)> = class_totals.iter().map(|(c, n)| (*c, *n)).collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    ranked.into_iter().take(n).map(|(c, _)| c).collect()
}

/// Connect the shared intake pool, degrading to `None` on ANY failure
/// (unconfigured, unreachable, auth failure, …) — every caller in this module
/// treats `None` as "no data available", never as an error to propagate (see
/// the module-level "graceful degradation" doc).
async fn pool_or_none() -> Option<PgPool> {
    storage::get_pool().await.ok()
}

/// `total` + `limit`/`offset`, clamped per spec §8 ("`limit` default 50, max
/// 500 … `offset`"). A non-numeric or missing `limit` defaults to 50; a
/// negative or absurd value clamps into `[1, 500]`. `offset` clamps to `>= 0`.
fn paginate(limit: Option<i64>, offset: Option<i64>) -> (i64, i64) {
    let limit = limit.unwrap_or(50).clamp(1, 500);
    let offset = offset.unwrap_or(0).max(0);
    (limit, offset)
}

/// Parse the `epoch` query param per [`EpochSelector`]'s contract (spec §8
/// contract-to-confirm #5): absent ⇒ `Current`, `"all"` ⇒ `All`, else ⇒
/// `Only(value)`.
fn epoch_selector_from_query(epoch: Option<&str>) -> EpochSelector {
    match epoch {
        None => EpochSelector::Current,
        Some(e) if e.eq_ignore_ascii_case("all") => EpochSelector::All,
        Some(e) => EpochSelector::Only(e.to_string()),
    }
}

/// Split a comma-separated `models=` query value into a trimmed, non-empty
/// `Vec<String>` (`None`/empty ⇒ empty vec, meaning "every model" to every
/// handler below).
fn split_models(models: Option<&str>) -> Vec<String> {
    models
        .map(|s| {
            s.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Mask + wrap `body` as a `200 application/json` response — the one shape
/// every handler in this module returns (bar `model_detail`'s 404 branch).
fn json_ok(body: Value) -> Response {
    let masked = mask_response(body);
    (StatusCode::OK, [("content-type", "application/json")], masked.to_string()).into_response()
}

fn json_status(status: StatusCode, body: Value) -> Response {
    let masked = mask_response(body);
    (status, [("content-type", "application/json")], masked.to_string()).into_response()
}

/// Read the persisted fleet catalog, degrading `NotConfigured`/any other DB
/// error to an empty list (never propagated to the caller — see the
/// module-level degradation doc). Only used by handlers that fold catalog data
/// into a best-effort view; a handler that needs to DISTINGUISH "no catalog
/// configured" from "empty catalog" would call `storage::read_fleet_catalog`
/// directly, but none of this module's endpoints need that distinction.
async fn catalog_or_empty(pool: &PgPool) -> Vec<StoredCatalogCard> {
    storage::read_fleet_catalog(pool).await.unwrap_or_default()
}

/// Read the brochure, degrading `NotConfigured`/any other DB error to an empty
/// list (same rationale as [`catalog_or_empty`]).
async fn brochure_or_empty(pool: &PgPool) -> Vec<DiscoveryCandidate> {
    discovery::storage::read_brochure(pool).await.unwrap_or_default()
}

// ---------------------------------------------------------------------------
// GET /api/terminus/models
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Default)]
pub struct ModelsListQuery {
    scope: Option<String>,
    q: Option<String>,
    category: Option<String>,
    status: Option<String>,
    serving: Option<bool>,
    limit: Option<i64>,
    offset: Option<i64>,
}

/// One unified model-list entry — the join of the fleet catalog, the
/// discovery brochure, the advisor matrix, and the `serving_profile`
/// keep-warm set, per spec §8's `GET /api/terminus/models` response sketch.
struct ModelListEntry {
    model_name: String,
    family: Option<String>,
    params_b: Option<f64>,
    quant: Option<String>,
    category: Option<&'static str>,
    brochure_status: Option<&'static str>,
    in_current_fleet: bool,
    discovery_score: Option<f64>,
    vram_gb: Option<f64>,
    size_b: Option<f64>,
    serving_now: bool,
    coverage_coder: bool,
    coverage_assistant: bool,
    coverage_serving: bool,
    coverage_agent: bool,
    best_pass_rate: Option<f64>,
    last_run_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl ModelListEntry {
    fn to_json(&self) -> Value {
        json!({
            "model_name": self.model_name,
            "family": self.family,
            "params_b": self.params_b,
            "quant": self.quant,
            "category": self.category,
            "brochure_status": self.brochure_status,
            "in_current_fleet": self.in_current_fleet,
            "discovery_score": self.discovery_score,
            "vram_gb": self.vram_gb,
            "size_b": self.size_b,
            "serving_now": self.serving_now,
            "coverage": {
                "coder": self.coverage_coder,
                "assistant": self.coverage_assistant,
                "serving": self.coverage_serving,
                "agent": self.coverage_agent,
            },
            "best_pass_rate": self.best_pass_rate,
            "last_run_at": self.last_run_at,
        })
    }
}

/// `GET /api/terminus/models?scope=&q=&category=&status=&serving=&limit=&offset=`
/// (spec §8). Joins the fleet catalog ⋈ discovery brochure ⋈ serving keep-warm
/// set ⋈ the static advisor matrix, entirely in-process (fleet-scale data:
/// ~57 catalog models + 608 brochure candidates — an in-memory join and
/// filter is simpler and just as correct as pushing this join into SQL, and
/// keeps every filter's semantics in one place to unit-test without a DB).
pub async fn list_models(Query(q): Query<ModelsListQuery>) -> Response {
    let (limit, offset) = paginate(q.limit, q.offset);
    let scope = q.scope.as_deref().unwrap_or("all");

    if let Some(category) = q.category.as_deref() {
        if FleetCategory::from_str(category).is_err() {
            return json_status(
                StatusCode::BAD_REQUEST,
                json!({"error": format!(
                    "unrecognized category '{category}' (expected one of: {})",
                    FleetCategory::ALL.iter().map(|c| c.as_str()).collect::<Vec<_>>().join(", ")
                )}),
            );
        }
    }

    let Some(pool) = pool_or_none().await else {
        return json_ok(json!({"total": 0, "refreshed_at": Value::Null, "models": []}));
    };

    let cards = catalog_or_empty(&pool).await;
    let brochure = brochure_or_empty(&pool).await;
    let keep_warm = storage::read_keep_warm_model_ids(&pool).await.unwrap_or_default();
    let matrix = crate::model_advisor::load_matrix();

    let refreshed_at = cards.iter().map(|c| c.refreshed_at).max();

    let cards_by_name: BTreeMap<&str, &StoredCatalogCard> =
        cards.iter().map(|c| (c.model_name.as_str(), c)).collect();
    let brochure_by_name: BTreeMap<&str, &DiscoveryCandidate> =
        brochure.iter().map(|c| (c.model_name.as_str(), c)).collect();

    let names: BTreeSet<&str> = match scope {
        "fleet" => cards
            .iter()
            .filter(|c| c.in_current_fleet)
            .map(|c| c.model_name.as_str())
            .collect(),
        "brochure" => brochure.iter().map(|c| c.model_name.as_str()).collect(),
        _ => cards
            .iter()
            .map(|c| c.model_name.as_str())
            .chain(brochure.iter().map(|c| c.model_name.as_str()))
            .collect(),
    };

    let mut entries: Vec<ModelListEntry> = names
        .into_iter()
        .map(|name| {
            let card = cards_by_name.get(name).copied();
            let cand = brochure_by_name.get(name).copied();
            let matrix_entry = matrix.get(name);

            let vram_gb = card
                .and_then(|c| c.serving_json.as_ref())
                .and_then(|j| j.get("vram_gb"))
                .and_then(Value::as_f64)
                .or_else(|| cand.and_then(|c| c.vram_footprint_gb));

            let (mut coder, mut assistant, mut serving, mut agent) = (false, false, false, false);
            let mut best_pass_rate: Option<f64> = None;
            let mut last_run_at: Option<chrono::DateTime<chrono::Utc>> = None;
            if let Some(card) = card {
                for cell in &card.cells {
                    if cell.status == "not_run" {
                        continue;
                    }
                    match cell.test_type.as_str() {
                        "coder" => coder = true,
                        "assistant" => assistant = true,
                        "serving" => serving = true,
                        "agent" => agent = true,
                        _ => {}
                    }
                    if let Some(pr) = cell.pass_rate {
                        best_pass_rate = Some(best_pass_rate.map_or(pr, |b: f64| b.max(pr)));
                    }
                    if let Some(t) = cell.last_run_at {
                        last_run_at = Some(last_run_at.map_or(t, |l: chrono::DateTime<chrono::Utc>| l.max(t)));
                    }
                }
            }

            ModelListEntry {
                model_name: name.to_string(),
                family: matrix_entry.map(|m| m.family.clone()).filter(|f| !f.is_empty()),
                params_b: matrix_entry.and_then(|m| m.params_b),
                quant: card.and_then(|c| c.quant.clone()),
                category: cand.map(|c| c.category.as_str()),
                brochure_status: cand.map(|c| c.status.as_str()),
                in_current_fleet: card.map(|c| c.in_current_fleet).unwrap_or(false),
                discovery_score: cand.and_then(|c| c.discovery_score),
                vram_gb,
                size_b: cand.and_then(|c| c.size_b).or_else(|| matrix_entry.and_then(|m| m.params_b)),
                serving_now: keep_warm.contains(name),
                coverage_coder: coder,
                coverage_assistant: assistant,
                coverage_serving: serving,
                coverage_agent: agent,
                best_pass_rate,
                last_run_at,
            }
        })
        .filter(|e| {
            if let Some(q) = &q.q {
                let q = q.to_lowercase();
                let hit = e.model_name.to_lowercase().contains(&q)
                    || e.family.as_deref().unwrap_or("").to_lowercase().contains(&q);
                if !hit {
                    return false;
                }
            }
            if let Some(cat) = &q.category {
                if e.category != Some(cat.as_str()) {
                    return false;
                }
            }
            if let Some(status) = &q.status {
                if e.brochure_status != Some(status.as_str()) {
                    return false;
                }
            }
            if let Some(serving) = q.serving {
                if e.serving_now != serving {
                    return false;
                }
            }
            true
        })
        .collect();

    entries.sort_by(|a, b| a.model_name.cmp(&b.model_name));

    let total = entries.len() as i64;
    let page: Vec<Value> = entries
        .into_iter()
        .skip(offset as usize)
        .take(limit as usize)
        .map(|e| e.to_json())
        .collect();

    json_ok(json!({"total": total, "refreshed_at": refreshed_at, "models": page}))
}

// ---------------------------------------------------------------------------
// GET /api/terminus/models/{name}
// ---------------------------------------------------------------------------

/// `GET /api/terminus/models/{name}` — `name` is axum's normal percent-decoded
/// path segment (spec §8: "name is the full HF-repo-id/registry key,
/// URL-encoded" — an HF repo id's `/` arrives pre-encoded as `%2F`, which
/// axum's `Path<String>` extractor decodes back to `/` within this ONE
/// segment, so a plain `:name` route — not a wildcard — is correct here).
/// Every source is independently optional; a `404` is returned ONLY when
/// EVERY source has nothing for `name` (spec §8's contract).
pub async fn model_detail(Path(name): Path<String>) -> Response {
    let matrix = crate::model_advisor::load_matrix();
    let identity = matrix.get(&name).map(|m| {
        json!({
            "family": m.family,
            "params_b": m.params_b,
            "active_b": m.active_b,
            "architecture": m.architecture,
            "quants": m.quants.iter().map(|(k, v)| {
                (k.clone(), json!({"vram_gb": v.vram_gb, "quality_penalty": v.quality_penalty}))
            }).collect::<Map<String, Value>>(),
            "quality": m.quality,
            "best_for": m.best_for,
            "avoid_for": m.avoid_for,
            "ollama_name": m.ollama_name,
            "notes": m.notes,
        })
    });

    let Some(pool) = pool_or_none().await else {
        return if identity.is_some() {
            json_ok(json!({
                "identity": identity,
                "brochure": Value::Null,
                "serving": [],
                "operational": Value::Null,
                "catalog": Value::Null,
                "note": "intake database unreachable — only static advisor-matrix identity available",
            }))
        } else {
            json_status(StatusCode::NOT_FOUND, json!({"error": "model not found", "model": name}))
        };
    };

    let brochure = brochure_or_empty(&pool).await.into_iter().find(|c| c.model_name == name);
    let cards = catalog_or_empty(&pool).await;
    let (filtered, _note) = catalog::filter_cards(
        &cards,
        &CatalogQuery { model: Some(name.clone()), status: None, test_type: None },
    );
    let card = filtered.into_iter().next();
    let serving_rows = storage::read_serving_profiles_for_model(&pool, &name).await.unwrap_or_default();
    let operational = storage::read_latest_operational_profile_for_model(&pool, &name)
        .await
        .unwrap_or(None);

    if identity.is_none() && brochure.is_none() && card.is_none() && serving_rows.is_empty() && operational.is_none() {
        return json_status(StatusCode::NOT_FOUND, json!({"error": "model not found", "model": name}));
    }

    let brochure_json = brochure.map(|c| {
        json!({
            "hf_repo": c.hf_repo,
            "category": c.category.as_str(),
            "status": c.status.as_str(),
            "gfx1151_class": c.gfx1151_class,
            "size_b": c.size_b,
            "vram_footprint_gb": c.vram_footprint_gb,
            "discovery_source": c.discovery_source,
            "discovery_score": c.discovery_score,
            "discovered_at": c.discovered_at,
            "last_seen_at": c.last_seen_at,
            "fetched_at": c.fetched_at,
            "marked_for_fleet_at": c.marked_for_fleet_at,
            "evicted_at": c.evicted_at,
            "rationale": c.rationale,
        })
    });

    let catalog_json = card.as_ref().map(|card| {
        json!({
            "card": {
                "model_name": card.model_name,
                "quant": card.quant,
                "in_current_fleet": card.in_current_fleet,
                "serving": card.serving_json,
                "not_run_count": card.not_run_count,
                "stale_count": card.stale_count,
                "refreshed_at": card.refreshed_at,
            },
            "cells": card.cells.iter().map(|c| json!({
                "test_type": c.test_type,
                "task_category": c.task_category,
                "quant": c.quant,
                "status": c.status,
                "pass_rate": c.pass_rate,
                "n_samples": c.n_samples,
                "score_stddev": c.score_stddev,
                "low_confidence": c.low_confidence,
                "last_run_at": c.last_run_at,
                "harness_version": c.harness_version,
            })).collect::<Vec<_>>(),
        })
    });

    let serving_json: Vec<Value> = serving_rows
        .iter()
        .map(|s| {
            json!({
                "backend_tag": s.backend_tag,
                "best_runtime": s.best_runtime,
                "tok_s": s.tok_s,
                "vram_or_ram_peak_gb": s.vram_or_ram_peak_gb,
                "cold_load_s": s.cold_load_s,
                "keep_warm": s.keep_warm,
                "fallback_runtime": s.fallback_runtime,
                "exclusion_reason": s.exclusion_reason,
                "recheck_trigger": s.recheck_trigger,
                "provenance": s.provenance,
                "updated_at": s.updated_at,
            })
        })
        .collect();

    let operational_json = operational.map(|op| {
        json!({
            "max_context_safe": op.max_context_safe,
            "max_context_absolute": op.max_context_absolute,
            "quality_degradation_point": op.quality_degradation_point,
            "throughput_at_2k": op.throughput_at_2k,
            "throughput_at_8k": op.throughput_at_8k,
            "throughput_at_16k": op.throughput_at_16k,
            "throughput_at_32k": op.throughput_at_32k,
            "throughput_at_64k": op.throughput_at_64k,
            "recommended_timeout_chat_sec": op.recommended_timeout_chat_sec,
            "recommended_timeout_build_sec": op.recommended_timeout_build_sec,
            "recommended_timeout_deep_sec": op.recommended_timeout_deep_sec,
            "overall_tier": op.overall_tier,
        })
    });

    json_ok(json!({
        "identity": identity,
        "brochure": brochure_json,
        "serving": serving_json,
        "operational": operational_json,
        "catalog": catalog_json,
    }))
}

// ---------------------------------------------------------------------------
// GET /api/terminus/mint/summary
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Default)]
pub struct EpochQuery {
    epoch: Option<String>,
}

/// `GET /api/terminus/mint/summary?epoch=` — the C0 overview tile payload.
pub async fn mint_summary(Query(q): Query<EpochQuery>) -> Response {
    let epoch = epoch_selector_from_query(q.epoch.as_deref());

    let Some(pool) = pool_or_none().await else {
        return json_ok(json!({
            "models_profiled": 0,
            "runs": {"code": 0, "context": 0, "agent": 0, "total": 0},
            "fleet_best_model": Value::Null,
            "gpu_hours": 0.0,
            "epoch": crate::intake::current_epoch(),
            "became_current_at": Value::Null,
        }));
    };

    let models_profiled = storage::read_models_profiled_count(&pool).await.unwrap_or(0);
    let (code, context, agent) = storage::read_run_counts(&pool, &epoch).await.unwrap_or((0, 0, 0));
    let best = storage::read_best_model_by_pass_hat_3(&pool).await.unwrap_or(None);
    let gpu_hours = storage::read_gpu_hours(&pool, &epoch).await.unwrap_or(0.0);
    let became_current_at = match epoch.epoch() {
        Some(e) => storage::read_epoch_marker(&pool, e).await.unwrap_or(None).map(|m| m.became_current_at),
        None => None,
    };

    json_ok(json!({
        "models_profiled": models_profiled,
        "runs": {"code": code, "context": context, "agent": agent, "total": code + context + agent},
        "fleet_best_model": best.map(|(model, pass_hat_3)| json!({"model": model, "pass_hat_3": pass_hat_3})),
        "gpu_hours": gpu_hours,
        "epoch": crate::intake::current_epoch(),
        "became_current_at": became_current_at,
    }))
}

// ---------------------------------------------------------------------------
// GET /api/terminus/mint/dimensions
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Default)]
pub struct DimensionsQuery {
    models: Option<String>,
    epoch: Option<String>,
}

/// `GET /api/terminus/mint/dimensions?models=&epoch=` — the C1 capability
/// radar (spec §7.2-C1 / §8). Normalizes each dimension min–max ACROSS THE
/// WHOLE FLEET (never just the requested `models`, so the axis scale doesn't
/// shift depending on the selection), and reports the fleet median per
/// dimension as a reference series.
pub async fn mint_dimensions(Query(q): Query<DimensionsQuery>) -> Response {
    let epoch = epoch_selector_from_query(q.epoch.as_deref());
    let requested = split_models(q.models.as_deref());

    let Some(pool) = pool_or_none().await else {
        return json_ok(json!({"dimensions": ASSISTANT_DIMENSIONS, "models": [], "fleet_median": []}));
    };

    let rollup = storage::read_assistant_dimension_rollup(&pool, &epoch).await.unwrap_or_default();

    // Fleet-wide min/max per dimension, for normalization.
    let mut ranges: BTreeMap<&str, (f64, f64)> = BTreeMap::new();
    for row in &rollup {
        let Some(dim) = ASSISTANT_DIMENSIONS.iter().copied().find(|d| *d == row.dimension) else {
            continue;
        };
        let entry = ranges.entry(dim).or_insert((row.mean_value, row.mean_value));
        entry.0 = entry.0.min(row.mean_value);
        entry.1 = entry.1.max(row.mean_value);
    }
    let normalize = |dim: &str, v: f64| -> f64 {
        match ranges.get(dim) {
            Some((lo, hi)) => normalize_min_max(*lo, *hi, v),
            None => 0.5, // no fleet-wide range at all for this dimension (no data)
        }
    };

    let models_present: BTreeSet<&str> = rollup.iter().map(|r| r.model_id.as_str()).collect();
    let selected: Vec<&str> = if requested.is_empty() {
        models_present.iter().take(2).copied().collect()
    } else {
        requested.iter().map(String::as_str).filter(|m| models_present.contains(m)).collect()
    };

    let models_json: Vec<Value> = selected
        .iter()
        .map(|model_id| {
            let scores: Vec<Value> = ASSISTANT_DIMENSIONS
                .iter()
                .copied()
                .map(|dim| {
                    match rollup.iter().find(|r| r.model_id == *model_id && r.dimension == dim) {
                        Some(r) => json!({
                            "dimension": dim,
                            "norm": normalize(dim, r.mean_value),
                            "raw": r.mean_value,
                            "metric": "value",
                            "std_dev": r.mean_std_dev,
                            "n": r.n,
                            "low_confidence": r.any_low_confidence,
                        }),
                        None => json!({
                            "dimension": dim,
                            "norm": Value::Null,
                            "raw": Value::Null,
                            "metric": "value",
                            "std_dev": Value::Null,
                            "n": 0,
                            "low_confidence": true,
                        }),
                    }
                })
                .collect();
            json!({"model_id": model_id, "scores": scores})
        })
        .collect();

    let fleet_median: Vec<Value> = ASSISTANT_DIMENSIONS
        .iter()
        .copied()
        .map(|dim| {
            let mut values: Vec<f64> = rollup
                .iter()
                .filter(|r| r.dimension == dim)
                .map(|r| normalize(dim, r.mean_value))
                .collect();
            values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let median = if values.is_empty() {
                None
            } else {
                let mid = values.len() / 2;
                Some(if values.len() % 2 == 0 {
                    (values[mid - 1] + values[mid]) / 2.0
                } else {
                    values[mid]
                })
            };
            json!({"dimension": dim, "norm": median})
        })
        .collect();

    json_ok(json!({"dimensions": ASSISTANT_DIMENSIONS, "models": models_json, "fleet_median": fleet_median}))
}

// ---------------------------------------------------------------------------
// GET /api/terminus/mint/matrix
// ---------------------------------------------------------------------------

/// `GET /api/terminus/mint/matrix?epoch=` — the C2 coverage heatmap (spec
/// §7.2-C2 / §8), sourced straight from the persisted fleet catalog cells.
pub async fn mint_matrix(Query(q): Query<EpochQuery>) -> Response {
    let epoch = epoch_selector_from_query(q.epoch.as_deref());

    let Some(pool) = pool_or_none().await else {
        return json_ok(json!({"models": [], "columns": [], "cells": []}));
    };
    let cards = catalog_or_empty(&pool).await;

    let epoch_value = epoch.epoch();
    let mut models: BTreeSet<String> = BTreeSet::new();
    let mut columns: BTreeSet<(String, String)> = BTreeSet::new();
    let mut cells: Vec<Value> = Vec::new();

    for card in &cards {
        models.insert(card.model_name.clone());
        for cell in &card.cells {
            // Epoch-scope the cell: a `not_run`/`non_viable` cell carries no
            // `harness_version` and is always shown regardless of scope (there
            // is no epoch-specific "not run" — the gap is epoch-independent);
            // a `run`/`stale` cell is scoped like every other epoch-partitioned
            // read here.
            if let Some(want) = epoch_value {
                if let Some(have) = &cell.harness_version {
                    if have != want {
                        continue;
                    }
                }
            }
            columns.insert((cell.test_type.clone(), cell.task_category.clone()));
            cells.push(json!({
                "model": card.model_name,
                "col": {"test_type": cell.test_type, "task_category": cell.task_category},
                "status": cell.status,
                "pass_rate": cell.pass_rate,
                "n_samples": cell.n_samples,
                "score_stddev": cell.score_stddev,
                "low_confidence": cell.low_confidence,
                "last_run_at": cell.last_run_at,
                "harness_version": cell.harness_version,
            }));
        }
    }

    let columns_json: Vec<Value> = columns
        .into_iter()
        .map(|(test_type, task_category)| json!({"test_type": test_type, "task_category": task_category}))
        .collect();

    json_ok(json!({"models": models, "columns": columns_json, "cells": cells}))
}

// ---------------------------------------------------------------------------
// GET /api/terminus/mint/runs
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Default)]
pub struct RunsQuery {
    suite: Option<String>,
    model: Option<String>,
    task_category: Option<String>,
    language: Option<String>,
    failure_class: Option<String>,
    epoch: Option<String>,
    limit: Option<i64>,
    offset: Option<i64>,
}

/// `GET /api/terminus/mint/runs?suite=code|context|agent&…` — paged raw run
/// rows feeding the table views + drill-downs (spec §8). `context`/`agent`
/// accept only `model` (those tables have no `task_category`/`language`/
/// `failure_class`/`harness_version` columns — see the doc on
/// `storage::read_context_runs_page`/`read_agent_runs_page`); an unrecognized
/// `suite` is a `400`.
///
/// `epoch` semantics per suite (review-cycle-2 fix — the cycle-1 panel caught
/// `epoch` being silently ignored for `context`/`agent`): only
/// `code_profile_runs` carries an epoch column (`harness_version`);
/// `context_profile_runs`/`agent_profile_runs` are structurally epoch-less
/// (see `src/intake/assistant/schema.rs`). Rather than silently ignore an
/// explicit epoch filter on an epoch-less suite, the handler rejects it with
/// a `400` naming the reason; `epoch` absent or `epoch=all` proceed (both are
/// satisfiable — "no epoch constraint").
pub async fn mint_runs(Query(q): Query<RunsQuery>) -> Response {
    let (limit, offset) = paginate(q.limit, q.offset);
    let suite = q.suite.as_deref().unwrap_or("code");

    // Validate the suite enum BEFORE the DB check so an unrecognized suite is a
    // 400 even when no DB is configured (otherwise it would silently degrade to
    // an empty 200 — see mint_runs_rejects_unknown_suite_with_400).
    if !matches!(suite, "code" | "context" | "agent") {
        return json_status(
            StatusCode::BAD_REQUEST,
            json!({"error": format!("unrecognized suite '{suite}' (expected one of: code, context, agent)")}),
        );
    }

    // Explicit specific-epoch filter on an epoch-less suite: honest 400, never
    // a silently-unfiltered page (validated pre-DB for the same reason as the
    // suite check above). Parsed through `epoch_selector_from_query` so the
    // selector contract (case-insensitive `all`, absent = Current) is resolved
    // in exactly one place — only a concrete `Only(_)` epoch is rejected
    // (cycle-2 review fix: a literal `e != "all"` check wrongly 400'd
    // `epoch=ALL`, contradicting the tested selector semantics).
    if matches!(suite, "context" | "agent") {
        if let EpochSelector::Only(_) = epoch_selector_from_query(q.epoch.as_deref()) {
            return json_status(
                StatusCode::BAD_REQUEST,
                json!({"error": format!(
                    "suite '{suite}' is not epoch-partitioned (its runs table has no epoch column); \
                     omit `epoch` or pass `epoch=all`"
                )}),
            );
        }
    }

    let Some(pool) = pool_or_none().await else {
        return json_ok(json!({"total": 0, "runs": []}));
    };

    match suite {
        "code" => {
            let filter = storage::CodeRunFilter {
                model: q.model.clone(),
                task_category: q.task_category.clone(),
                language: q.language.clone(),
                failure_class: q.failure_class.clone(),
                epoch: epoch_selector_from_query(q.epoch.as_deref()),
            };
            let (rows, total) = storage::read_code_runs_page(&pool, &filter, limit, offset)
                .await
                .unwrap_or((Vec::new(), 0));
            json_ok(json!({"total": total, "runs": rows}))
        }
        "context" => {
            let (rows, total) = storage::read_context_runs_page(&pool, q.model.as_deref(), limit, offset)
                .await
                .unwrap_or((Vec::new(), 0));
            json_ok(json!({"total": total, "runs": rows}))
        }
        "agent" => {
            let (rows, total) = storage::read_agent_runs_page(&pool, q.model.as_deref(), limit, offset)
                .await
                .unwrap_or((Vec::new(), 0));
            json_ok(json!({"total": total, "runs": rows}))
        }
        other => json_status(
            StatusCode::BAD_REQUEST,
            json!({"error": format!("unrecognized suite '{other}' (expected one of: code, context, agent)")}),
        ),
    }
}

// ---------------------------------------------------------------------------
// GET /api/terminus/mint/box
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Default)]
pub struct BoxQuery {
    metric: Option<String>,
    model: Option<String>,
    task_category: Option<String>,
    language: Option<String>,
    failure_class: Option<String>,
    epoch: Option<String>,
}

/// Server-side five-number summary for one model's values, plus outliers
/// beyond 1.5×IQR (spec §7.2-C3's box-plot contract). A model with fewer than
/// 5 samples reports `n < 5` (the caller — `mint_box` — flags it so a
/// consumer can render the documented beeswarm-strip fallback instead of a
/// 3-point box that would lie).
///
/// Takes a plain `&[f64]` slice (the caller's original, UNSORTED order —
/// e.g. `model_rows` in `mint_box`, which parallels `run_id`/`case_id`/
/// `failure_class` by position) and returns outlier indices into THAT SAME
/// original order — never into an internally-sorted copy. (Fixed a real bug,
/// caught in review: an earlier version sorted `values` in place and returned
/// indices into the sorted array, which `mint_box` then used to index into
/// the still-unsorted `model_rows`, silently attaching the wrong run's
/// `run_id`/`case_id`/`failure_class` to each reported outlier. This version
/// sorts a separate `(original_index, value)` pairing and maps every output
/// — quartiles AND outlier indices — back through that pairing, so the
/// function's `usize` outputs are always safe to index the caller's original
/// slice with directly; see `quartiles_outlier_indices_map_back_to_original_order`.)
fn quartiles(values: &[f64]) -> (f64, f64, f64, f64, f64, Vec<usize>) {
    let mut indexed: Vec<(usize, f64)> = values.iter().copied().enumerate().collect();
    indexed.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    let sorted: Vec<f64> = indexed.iter().map(|(_, v)| *v).collect();
    let n = sorted.len();
    let percentile = |p: f64| -> f64 {
        if n == 0 {
            return 0.0;
        }
        if n == 1 {
            return sorted[0];
        }
        let rank = p * (n as f64 - 1.0);
        let lo = rank.floor() as usize;
        let hi = rank.ceil() as usize;
        if lo == hi {
            sorted[lo]
        } else {
            sorted[lo] + (sorted[hi] - sorted[lo]) * (rank - lo as f64)
        }
    };
    let q1 = percentile(0.25);
    let median = percentile(0.5);
    let q3 = percentile(0.75);
    let iqr = q3 - q1;
    let (lo_fence, hi_fence) = (q1 - 1.5 * iqr, q3 + 1.5 * iqr);
    // `orig_idx` here is the position in the CALLER'S original (unsorted)
    // slice — this is the whole point of carrying `indexed` through instead
    // of filtering over `sorted` directly.
    let outlier_idx: Vec<usize> = indexed
        .iter()
        .filter(|(_, v)| *v < lo_fence || *v > hi_fence)
        .map(|(orig_idx, _)| *orig_idx)
        .collect();
    let min = sorted.first().copied().unwrap_or(0.0);
    let max = sorted.last().copied().unwrap_or(0.0);
    (min, q1, median, q3, max, outlier_idx)
}

/// `GET /api/terminus/mint/box?metric=total_time_ms|code_quality_score&…` —
/// server-side quartiles per model (spec §8: "5,721 rows never ship raw to
/// the browser"). An unrecognized `metric` is a `400` (this value is spliced
/// into SQL as a trusted column name — see
/// `storage::read_code_run_values_for_box`'s doc — so it MUST be validated
/// against this allowlist before it ever reaches that function).
pub async fn mint_box(Query(q): Query<BoxQuery>) -> Response {
    let metric = q.metric.as_deref().unwrap_or("total_time_ms");
    if metric != "total_time_ms" && metric != "code_quality_score" {
        return json_status(
            StatusCode::BAD_REQUEST,
            json!({"error": format!(
                "unrecognized metric '{metric}' (expected one of: total_time_ms, code_quality_score)"
            )}),
        );
    }

    let Some(pool) = pool_or_none().await else {
        return json_ok(json!({"groups": []}));
    };

    let filter = storage::CodeRunFilter {
        model: q.model.clone(),
        task_category: q.task_category.clone(),
        language: q.language.clone(),
        failure_class: q.failure_class.clone(),
        epoch: epoch_selector_from_query(q.epoch.as_deref()),
    };
    let rows = storage::read_code_run_values_for_box(&pool, metric, &filter).await.unwrap_or_default();

    let mut by_model: BTreeMap<&str, Vec<&storage::BoxMetricRow>> = BTreeMap::new();
    for row in &rows {
        by_model.entry(row.model.as_str()).or_default().push(row);
    }

    let groups: Vec<Value> = by_model
        .into_iter()
        .map(|(model, model_rows)| {
            let values: Vec<f64> = model_rows.iter().map(|r| r.value).collect();
            let n = values.len();
            let (min, q1, median, q3, max, outlier_idx) = quartiles(&values);
            let outliers: Vec<Value> = outlier_idx
                .into_iter()
                .filter_map(|i| model_rows.get(i))
                .map(|r| {
                    json!({
                        "run_id": r.run_id,
                        "value": r.value,
                        "case_id": r.case_id,
                        "failure_class": r.failure_class,
                    })
                })
                .collect();
            json!({
                "model": model,
                "min": min, "q1": q1, "median": median, "q3": q3, "max": max,
                "n": n,
                "low_n": n < 5,
                "outliers": outliers,
            })
        })
        .collect();

    json_ok(json!({"groups": groups}))
}

// ---------------------------------------------------------------------------
// GET /api/terminus/mint/language-stats
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Default)]
pub struct LanguageStatsQuery {
    language: Option<String>,
    epoch: Option<String>,
}

/// `GET /api/terminus/mint/language-stats?language=&epoch=` — the C4 Pareto
/// scatter's rows (spec §7.2-C4 / §8). `epoch` is honored (review fix — see
/// `storage::read_language_stats`'s doc for why the earlier version silently
/// ignored it: it read a pre-aggregated, non-epoch-partitioned matview
/// directly; this now recomputes the same rollup live, epoch-scoped). Each
/// row additionally carries a server-computed `point_size_px`
/// ([`pareto_point_size_px`]) — the C4 scatter's √-scaled 8–24px point-size
/// encoding of `vram_gb`, computed over the fleet-wide min/max of THIS
/// response's own rows (spec §8: "5,721 rows never ship raw to the browser"
/// — the same "compute the derived chart encoding server-side" principle
/// `mint_box`'s quartiles already follow).
pub async fn mint_language_stats(Query(q): Query<LanguageStatsQuery>) -> Response {
    let epoch = epoch_selector_from_query(q.epoch.as_deref());

    let Some(pool) = pool_or_none().await else {
        return json_ok(json!({"rows": []}));
    };
    let rows = storage::read_language_stats(&pool, q.language.as_deref(), &epoch).await.unwrap_or_default();

    let vram_values: Vec<f64> = rows.iter().filter_map(|r| r.vram_gb).filter(|v| *v > 0.0).collect();
    let min_vram = vram_values.iter().copied().fold(f64::INFINITY, f64::min);
    let max_vram = vram_values.iter().copied().fold(f64::NEG_INFINITY, f64::max);

    let rows_json: Vec<Value> = rows
        .iter()
        .map(|r| {
            let mut v = serde_json::to_value(r).unwrap_or(Value::Null);
            if let Some(obj) = v.as_object_mut() {
                obj.insert(
                    "point_size_px".to_string(),
                    json!(pareto_point_size_px(r.vram_gb, min_vram, max_vram)),
                );
            }
            v
        })
        .collect();

    json_ok(json!({"rows": rows_json}))
}

// ---------------------------------------------------------------------------
// GET /api/terminus/mint/failures
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Default)]
pub struct FailuresQuery {
    epoch: Option<String>,
    task_category: Option<String>,
}

/// `GET /api/terminus/mint/failures?epoch=&task_category=` — the C6
/// failure-class bars (spec §7.2-C6 / §8): top-5 classes fleet-wide + an
/// "other" fold (§8's own wording; the ceiling matches the 6-slot brand
/// palette with room for the "other" bucket).
pub async fn mint_failures(Query(q): Query<FailuresQuery>) -> Response {
    let epoch = epoch_selector_from_query(q.epoch.as_deref());

    let Some(pool) = pool_or_none().await else {
        return json_ok(json!({"classes": [], "models": []}));
    };
    let rows = storage::read_failure_class_counts(&pool, &epoch, q.task_category.as_deref())
        .await
        .unwrap_or_default();

    let mut class_totals: BTreeMap<&str, i64> = BTreeMap::new();
    for (_, class, n) in &rows {
        *class_totals.entry(class.as_str()).or_insert(0) += n;
    }
    let top5 = top_n_classes(&class_totals, 5);
    // Preserve rank order (highest total first) for the `classes` list the
    // chart's legend/series order follows; `top5` above is just the
    // membership test used per-row below.
    let mut ranked: Vec<(&str, i64)> = class_totals.into_iter().filter(|(c, _)| top5.contains(c)).collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    let classes: Vec<&str> = ranked.into_iter().map(|(c, _)| c).chain(std::iter::once("other")).collect();

    let mut by_model: BTreeMap<&str, BTreeMap<&str, i64>> = BTreeMap::new();
    let mut total_by_model: BTreeMap<&str, i64> = BTreeMap::new();
    for (model, class, n) in &rows {
        let bucket = if top5.contains(class.as_str()) { class.as_str() } else { "other" };
        *by_model.entry(model.as_str()).or_default().entry(bucket).or_insert(0) += n;
        *total_by_model.entry(model.as_str()).or_insert(0) += n;
    }

    let models: Vec<Value> = by_model
        .into_iter()
        .map(|(model, counts)| {
            json!({
                "model": model,
                "counts": counts,
                "total_runs": total_by_model.get(model).copied().unwrap_or(0),
            })
        })
        .collect();

    json_ok(json!({"classes": classes, "models": models}))
}

// ---------------------------------------------------------------------------
// GET /api/terminus/mint/context-profiles
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Default)]
pub struct ContextProfilesQuery {
    models: Option<String>,
}

/// `GET /api/terminus/mint/context-profiles?models=` — the C7 context
/// degradation lines (spec §7.2-C7 / §8): per-model tier arrays + each
/// model's `max_context_safe` marker.
pub async fn mint_context_profiles(Query(q): Query<ContextProfilesQuery>) -> Response {
    let models = split_models(q.models.as_deref());

    let Some(pool) = pool_or_none().await else {
        return json_ok(json!({"models": []}));
    };
    let rows = storage::read_context_profiles(&pool, &models).await.unwrap_or_default();

    let mut by_model: BTreeMap<&str, (Option<i32>, Vec<Value>)> = BTreeMap::new();
    for row in &rows {
        let entry = by_model.entry(row.model.as_str()).or_insert((row.max_context_safe, Vec::new()));
        entry.0 = row.max_context_safe;
        entry.1.push(json!({
            "context_tokens": row.context_tokens,
            "throughput_tok_per_sec": row.throughput_tok_per_sec,
            "ttft_ms": row.ttft_ms,
            "recall_score": row.recall_score,
            "memory_usage_mb": row.memory_usage_mb,
            "oom": row.oom,
        }));
    }

    let models_json: Vec<Value> = by_model
        .into_iter()
        .map(|(model, (max_context_safe, tiers))| {
            json!({"model": model, "max_context_safe": max_context_safe, "tiers": tiers})
        })
        .collect();

    json_ok(json!({"models": models_json}))
}

// ---------------------------------------------------------------------------
// GET /api/terminus/mint/activity
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Default)]
pub struct ActivityQuery {
    range: Option<String>,
}

/// Parse the `range` param (`"30d"`/`"90d"`/`"all"`, spec §7.2-C8) into a day
/// count. `"all"` is a generously large window (10 years) rather than a
/// separate no-filter code path — the underlying read always filters by a
/// window, so "all" is just a very wide one. An unrecognized value falls back
/// to the 30-day default rather than erroring (a display-range param is not
/// worth a 400).
fn range_days(range: Option<&str>) -> i64 {
    match range {
        Some("90d") => 90,
        Some("all") => 3650,
        _ => 30,
    }
}

/// `GET /api/terminus/mint/activity?range=` — the C8 sweep-activity
/// time-series (spec §7.2-C8 / §8): runs/day by suite + epoch markers.
pub async fn mint_activity(Query(q): Query<ActivityQuery>) -> Response {
    let days = range_days(q.range.as_deref());

    let Some(pool) = pool_or_none().await else {
        return json_ok(json!({"days": [], "epochs": []}));
    };
    let day_counts = storage::read_activity_histogram(&pool, days).await.unwrap_or_default();

    // Every known epoch marker in the window is out of scope for a targeted
    // per-epoch read (there is no "list all markers" helper yet, and the
    // epoch timeline is small) — surface the CURRENT epoch's marker, which is
    // the one the chart's vertical hairline convention (spec §7.2-C8) most
    // needs; a future item can extend this to every historical marker if the
    // UI needs the full timeline.
    let current_marker = storage::read_epoch_marker(&pool, crate::intake::current_epoch())
        .await
        .unwrap_or(None);
    let epochs: Vec<Value> = current_marker
        .into_iter()
        .map(|m| json!({"epoch": m.epoch, "became_current_at": m.became_current_at, "note": m.note}))
        .collect();

    json_ok(json!({"days": day_counts, "epochs": epochs}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use axum::routing::get;
    use axum::Router;
    use tower::ServiceExt;

    fn test_router() -> Router {
        Router::new()
            .route("/api/terminus/models", get(list_models))
            .route("/api/terminus/models/:name", get(model_detail))
            .route("/api/terminus/mint/summary", get(mint_summary))
            .route("/api/terminus/mint/dimensions", get(mint_dimensions))
            .route("/api/terminus/mint/matrix", get(mint_matrix))
            .route("/api/terminus/mint/runs", get(mint_runs))
            .route("/api/terminus/mint/box", get(mint_box))
            .route("/api/terminus/mint/language-stats", get(mint_language_stats))
            .route("/api/terminus/mint/failures", get(mint_failures))
            .route("/api/terminus/mint/context-profiles", get(mint_context_profiles))
            .route("/api/terminus/mint/activity", get(mint_activity))
    }

    async fn get_json(router: Router, path: &str) -> (StatusCode, Value) {
        let req = Request::builder().method("GET").uri(path).body(Body::empty()).unwrap();
        let resp = router.oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, value)
    }

    /// Without `INTAKE_DATABASE_URL`/`DATABASE_URL` set, every list endpoint
    /// degrades to its empty shape with a `200` — never an error — matching
    /// the module's "empty-DB degradation to empty arrays, not errors" test
    /// plan line.
    fn clear_db_env() {
        std::env::remove_var("INTAKE_DATABASE_URL");
        std::env::remove_var("DATABASE_URL");
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn models_list_degrades_to_empty_without_a_configured_db() {
        clear_db_env();
        let (status, body) = get_json(test_router(), "/api/terminus/models").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["total"], 0);
        assert_eq!(body["models"], json!([]));
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn model_detail_404s_when_unknown_everywhere() {
        clear_db_env();
        let (status, body) =
            get_json(test_router(), "/api/terminus/models/this-model-does-not-exist-anywhere").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"], "model not found");
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn model_detail_returns_advisor_identity_even_without_a_db() {
        clear_db_env();
        let matrix = crate::model_advisor::load_matrix();
        let Some(name) = matrix.keys().next().cloned() else {
            // The bundled matrix YAML is empty in this environment — nothing to
            // assert against; still a pass (not every build env ships the
            // full advisor data set).
            return;
        };
        let (status, body) = get_json(test_router(), &format!("/api/terminus/models/{name}")).await;
        assert_eq!(status, StatusCode::OK);
        assert!(!body["identity"].is_null());
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn mint_summary_degrades_to_zeroed_shape_without_a_configured_db() {
        clear_db_env();
        let (status, body) = get_json(test_router(), "/api/terminus/mint/summary").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["models_profiled"], 0);
        assert_eq!(body["runs"]["total"], 0);
        assert_eq!(body["epoch"], crate::intake::current_epoch());
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn mint_dimensions_degrades_to_empty_without_a_configured_db() {
        clear_db_env();
        let (status, body) = get_json(test_router(), "/api/terminus/mint/dimensions").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["dimensions"].as_array().unwrap().len(), 8);
        assert_eq!(body["models"], json!([]));
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn mint_matrix_degrades_to_empty_without_a_configured_db() {
        clear_db_env();
        let (status, body) = get_json(test_router(), "/api/terminus/mint/matrix").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["cells"], json!([]));
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn mint_runs_rejects_unknown_suite_with_400() {
        clear_db_env();
        let (status, _body) = get_json(test_router(), "/api/terminus/mint/runs?suite=bogus").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    /// Review-cycle-2 fix: `context`/`agent` runs tables are structurally
    /// epoch-less, so an explicit specific-epoch filter is an honest `400`
    /// (never a silently-unfiltered page), while absent / `epoch=all` proceed.
    #[tokio::test]
    #[serial_test::serial]
    async fn mint_runs_rejects_specific_epoch_on_epochless_suites() {
        clear_db_env();
        for suite in ["context", "agent"] {
            let (status, body) =
                get_json(test_router(), &format!("/api/terminus/mint/runs?suite={suite}&epoch=v2")).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "expected 400 for {suite}+epoch=v2");
            assert!(
                body["error"].as_str().unwrap_or_default().contains("not epoch-partitioned"),
                "error names the reason for {suite}"
            );
            let (status, _body) =
                get_json(test_router(), &format!("/api/terminus/mint/runs?suite={suite}&epoch=all")).await;
            assert_eq!(status, StatusCode::OK, "epoch=all proceeds for {suite}");
            // Case-insensitive per the EpochSelector contract (cycle-2 fix: a literal
            // string compare wrongly 400'd the uppercase form).
            let (status, _body) =
                get_json(test_router(), &format!("/api/terminus/mint/runs?suite={suite}&epoch=ALL")).await;
            assert_eq!(status, StatusCode::OK, "epoch=ALL proceeds for {suite}");
            let (status, _body) =
                get_json(test_router(), &format!("/api/terminus/mint/runs?suite={suite}")).await;
            assert_eq!(status, StatusCode::OK, "absent epoch proceeds for {suite}");
        }
        // The epoch-partitioned suite still accepts a specific epoch.
        let (status, _body) =
            get_json(test_router(), "/api/terminus/mint/runs?suite=code&epoch=v2").await;
        assert_eq!(status, StatusCode::OK);
    }

    /// Review-cycle-2 fix: the masking property is now tested for REAL — a
    /// planted secret-shaped value routed through this module's shared
    /// response helpers (`json_ok`/`json_status`, the single egress every
    /// handler returns through) must come out masked, mirroring the mask
    /// module's own negative-property test. A vacuous content-type check
    /// cannot regress silently anymore: if `json_ok` stops calling
    /// `mask_response`, this fails.
    #[tokio::test]
    async fn json_helpers_mask_planted_secrets() {
        let planted = "<REDACTED-SECRET>"; // pii-test-fixture
        for resp in [
            json_ok(json!({"models": [{"model_name": "m", "api_key": planted}]})),
            json_status(StatusCode::BAD_REQUEST, json!({"error": "x", "openrouter_api_key": planted})),
        ] {
            let (_parts, body) = resp.into_parts();
            let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
            let s = String::from_utf8_lossy(&bytes);
            assert!(!s.contains(planted), "planted secret must never survive egress");
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn mint_runs_degrades_to_empty_without_a_configured_db() {
        clear_db_env();
        let (status, body) = get_json(test_router(), "/api/terminus/mint/runs?suite=code").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["total"], 0);
        assert_eq!(body["runs"], json!([]));
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn mint_box_rejects_unknown_metric_with_400() {
        clear_db_env();
        let (status, _body) = get_json(test_router(), "/api/terminus/mint/box?metric=bogus").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn mint_box_degrades_to_empty_without_a_configured_db() {
        clear_db_env();
        let (status, body) = get_json(test_router(), "/api/terminus/mint/box").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["groups"], json!([]));
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn mint_language_stats_degrades_to_empty_without_a_configured_db() {
        clear_db_env();
        let (status, body) = get_json(test_router(), "/api/terminus/mint/language-stats").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["rows"], json!([]));
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn mint_failures_degrades_to_empty_without_a_configured_db() {
        clear_db_env();
        let (status, body) = get_json(test_router(), "/api/terminus/mint/failures").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["models"], json!([]));
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn mint_context_profiles_degrades_to_empty_without_a_configured_db() {
        clear_db_env();
        let (status, body) = get_json(test_router(), "/api/terminus/mint/context-profiles").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["models"], json!([]));
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn mint_activity_degrades_to_empty_without_a_configured_db() {
        clear_db_env();
        let (status, body) = get_json(test_router(), "/api/terminus/mint/activity").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["days"], json!([]));
    }

    // NOTE: the session-guard boundary test for these routes
    // (`GET /api/terminus/models` unauthenticated ⇒ 401) lives in
    // `crate::constellation::tests` alongside the guard's other routes —
    // that module already owns `test_state()`/`constellation_router()`
    // wiring (see `unauthenticated_request_to_protected_route_is_rejected_401`
    // and its `models_api`-specific sibling added there for CONST-21).

    #[test]
    fn quartiles_of_a_known_set_match_the_textbook_values() {
        // 1..=9: Q1=3, median=5, Q3=7 (linear-interpolation method, matches
        // the common "R-7"/numpy-default convention).
        let values: Vec<f64> = (1..=9).map(|n| n as f64).collect();
        let (min, q1, median, q3, max, outliers) = quartiles(&values);
        assert_eq!(min, 1.0);
        assert_eq!(q1, 3.0);
        assert_eq!(median, 5.0);
        assert_eq!(q3, 7.0);
        assert_eq!(max, 9.0);
        assert!(outliers.is_empty());
    }

    #[test]
    fn quartiles_flags_a_far_outlier() {
        let mut values: Vec<f64> = (1..=9).map(|n| n as f64).collect();
        values.push(1000.0);
        let (_, _, _, _, _, outliers) = quartiles(&values);
        assert_eq!(outliers.len(), 1);
        // The far outlier is the LAST element of this particular input, so a
        // regression to the old (broken) "index into the sorted copy" behavior
        // would happen to still pass this assertion by coincidence — see the
        // dedicated `_maps_back_to_original_order` test below for the case
        // that actually catches that class of bug.
        assert_eq!(outliers[0], values.len() - 1);
    }

    /// Regression test for the review-caught bug: `quartiles`'s returned
    /// outlier indices MUST index into the CALLER's original (unsorted)
    /// order, not into an internally-sorted copy. Uses an input that is
    /// deliberately NOT already sorted and NOT outlier-last, so a
    /// reintroduced "index into the sorted array" bug fails this test even
    /// though it might pass a sorted-input fixture by coincidence.
    #[test]
    fn quartiles_outlier_indices_map_back_to_original_order() {
        // Unsorted; the outlier (1000.0) sits at original index 2, nowhere
        // near where it would land after sorting (index 9, the max).
        let values = vec![5.0, 3.0, 1000.0, 7.0, 2.0, 4.0, 6.0, 1.0, 8.0, 9.0];
        let (_, _, _, _, _, outliers) = quartiles(&values);
        assert_eq!(outliers, vec![2], "outlier index must point at the original position of 1000.0");
        assert_eq!(values[outliers[0]], 1000.0);
    }

    /// End-to-end fixture proving `mint_box`'s outlier objects carry the
    /// RIGHT run's `run_id`/`case_id`/`failure_class` — the exact property
    /// the review finding was about (indices were previously computed
    /// against a sorted copy of `values` but used to index the still-
    /// unsorted `model_rows`, silently swapping which run's metadata each
    /// outlier reported).
    #[test]
    fn mint_box_outlier_metadata_matches_its_own_value_not_a_swapped_run() {
        use uuid::Uuid;

        let rows: Vec<storage::BoxMetricRow> = vec![
            (5.0, "case-a"),
            (3.0, "case-b"),
            (1000.0, "case-outlier"), // the far outlier, deliberately NOT last/first
            (7.0, "case-c"),
            (2.0, "case-d"),
            (4.0, "case-e"),
            (6.0, "case-f"),
            (1.0, "case-g"),
            (8.0, "case-h"),
            (9.0, "case-i"),
        ]
        .into_iter()
        .map(|(value, case_id)| storage::BoxMetricRow {
            model: "test-model".to_string(),
            value,
            run_id: Uuid::new_v4(),
            case_id: Some(case_id.to_string()),
            failure_class: None,
        })
        .collect();

        let model_rows: Vec<&storage::BoxMetricRow> = rows.iter().collect();
        let values: Vec<f64> = model_rows.iter().map(|r| r.value).collect();
        let (_, _, _, _, _, outlier_idx) = quartiles(&values);

        assert_eq!(outlier_idx.len(), 1);
        let outlier_row = model_rows[outlier_idx[0]];
        assert_eq!(outlier_row.value, 1000.0);
        assert_eq!(outlier_row.case_id.as_deref(), Some("case-outlier"));
    }

    #[test]
    fn paginate_clamps_limit_and_offset() {
        assert_eq!(paginate(None, None), (50, 0));
        assert_eq!(paginate(Some(10_000), Some(-5)), (500, 0));
        assert_eq!(paginate(Some(0), Some(3)), (1, 3));
    }

    #[test]
    fn epoch_selector_from_query_matches_the_documented_contract() {
        assert_eq!(epoch_selector_from_query(None), EpochSelector::Current);
        assert_eq!(epoch_selector_from_query(Some("all")), EpochSelector::All);
        assert_eq!(epoch_selector_from_query(Some("ALL")), EpochSelector::All);
        assert_eq!(epoch_selector_from_query(Some("v2")), EpochSelector::Only("v2".to_string()));
    }

    #[test]
    fn split_models_trims_and_drops_empties() {
        assert_eq!(split_models(Some(" a , b,,c ")), vec!["a", "b", "c"]);
        assert_eq!(split_models(None), Vec::<String>::new());
        assert_eq!(split_models(Some("")), Vec::<String>::new());
    }

    // ---- normalization (C1 capability radar) ----

    #[test]
    fn normalize_min_max_scales_into_zero_one() {
        assert_eq!(normalize_min_max(0.0, 10.0, 0.0), 0.0);
        assert_eq!(normalize_min_max(0.0, 10.0, 10.0), 1.0);
        assert_eq!(normalize_min_max(0.0, 10.0, 5.0), 0.5);
        assert_eq!(normalize_min_max(2.0, 8.0, 5.0), 0.5);
    }

    #[test]
    fn normalize_min_max_degenerate_range_is_the_midpoint() {
        // lo == hi (every fleet value identical, or a single sample) must not
        // divide by zero / produce NaN or infinity.
        assert_eq!(normalize_min_max(4.0, 4.0, 4.0), 0.5);
    }

    // ---- Pareto-input point-size encoding (C4 scatter) ----

    #[test]
    fn pareto_point_size_floors_and_ceils_at_the_fleet_extremes() {
        assert_eq!(pareto_point_size_px(Some(4.0), 4.0, 96.0), 8.0);
        assert_eq!(pareto_point_size_px(Some(96.0), 4.0, 96.0), 24.0);
    }

    #[test]
    fn pareto_point_size_is_monotonic_in_vram() {
        let small = pareto_point_size_px(Some(8.0), 4.0, 96.0);
        let mid = pareto_point_size_px(Some(24.0), 4.0, 96.0);
        let large = pareto_point_size_px(Some(64.0), 4.0, 96.0);
        assert!(small < mid, "{small} should be < {mid}");
        assert!(mid < large, "{mid} should be < {large}");
        assert!((8.0..=24.0).contains(&small));
        assert!((8.0..=24.0).contains(&large));
    }

    #[test]
    fn pareto_point_size_missing_or_non_positive_vram_floors_to_8px() {
        assert_eq!(pareto_point_size_px(None, 4.0, 96.0), 8.0);
        assert_eq!(pareto_point_size_px(Some(0.0), 4.0, 96.0), 8.0);
        assert_eq!(pareto_point_size_px(Some(-1.0), 4.0, 96.0), 8.0);
    }

    #[test]
    fn pareto_point_size_degenerate_fleet_range_is_the_midpoint() {
        assert_eq!(pareto_point_size_px(Some(32.0), 32.0, 32.0), 16.0);
    }

    // ---- top-5 + "other" folding (C6 failure-class bars) ----

    #[test]
    fn top_n_classes_keeps_the_highest_totals() {
        let mut totals: BTreeMap<&str, i64> = BTreeMap::new();
        totals.insert("timeout", 50);
        totals.insert("compilation_error", 40);
        totals.insert("test_failure", 30);
        totals.insert("truncation", 20);
        totals.insert("empty_diff", 10);
        totals.insert("provider_error", 5); // 6th — must be folded to "other"
        totals.insert("phase_stall", 1); // 7th — also folded

        let top = top_n_classes(&totals, 5);
        assert_eq!(top.len(), 5);
        assert!(top.contains("timeout"));
        assert!(top.contains("compilation_error"));
        assert!(top.contains("test_failure"));
        assert!(top.contains("truncation"));
        assert!(top.contains("empty_diff"));
        assert!(!top.contains("provider_error"), "6th-highest class must NOT survive the top-5 fold");
        assert!(!top.contains("phase_stall"));
    }

    #[test]
    fn top_n_classes_ties_break_by_name_for_determinism() {
        let mut totals: BTreeMap<&str, i64> = BTreeMap::new();
        totals.insert("b", 10);
        totals.insert("a", 10);
        totals.insert("c", 10);
        let top = top_n_classes(&totals, 2);
        // Same input must always fold the same way — alphabetically-first
        // wins a tie, deterministically (never "whatever HashMap order gave
        // us today").
        assert!(top.contains("a"));
        assert!(top.contains("b"));
        assert!(!top.contains("c"));
    }

    #[test]
    fn top_n_classes_fewer_than_n_keeps_everything() {
        let mut totals: BTreeMap<&str, i64> = BTreeMap::new();
        totals.insert("x", 3);
        totals.insert("y", 1);
        let top = top_n_classes(&totals, 5);
        assert_eq!(top.len(), 2);
    }

    // NOTE: the representative auth-401 / viewer-200 / masking spot-checks
    // across EVERY route this module registers live in
    // `crate::constellation::tests` (`every_models_api_route_rejects_
    // unauthenticated_requests` / `every_models_api_route_is_reachable_with_
    // a_valid_session`) — that module owns the real `protected_router`/
    // `auth::require_session` wiring this module's own local `test_router()`
    // (used only for the shape/degradation tests above) deliberately does
    // NOT include, so an auth-boundary assertion belongs there, not here.
}
