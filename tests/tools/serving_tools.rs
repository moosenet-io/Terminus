//! S85 SRV-07 integration tests — serving control + status tools.
//!
//! Three tools, exercised end-to-end against mocked/seeded backends:
//!   - `serving_residency_status` against a MOCK residency-state file (temp file +
//!     `CHORD_RESIDENCY_STATE_PATH`): SERVING shape and IDLE shape, sanitized.
//!   - `serving_profile_refresh` against a MOCK Chord control server (httpmock +
//!     `CHORD_CONTROL_URL`): reload signal fires; unreachable ⇒ genericized failure.
//!   - `serving_profile_get` against a SEEDED Postgres (gated on `DATABASE_URL`):
//!     expected shape for a profiled model; clean "no profile" for an unprofiled one.
//!
//! All env mutation is `#[serial]` (process-global env). Outputs are asserted to be
//! sanitized per S6 (no infra host/IP/path/secret leaks).

use httpmock::prelude::*;
use serde_json::json;
use serial_test::serial;

use terminus_rs::tools::serving_tools::{
    ServingProfileGet, ServingProfileRefresh, ServingResidencyStatus,
};
use terminus_rs::RustTool;

// ── residency status (mocked state file) ─────────────────────────────────────

#[tokio::test]
#[serial]
async fn residency_status_serving_shape_sanitized() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("residency.json");
    // The SRV-05 snapshot contract: ROLE-keyed residents, free/baseline VRAM, pin.
    let snapshot = json!({
        "residents": [
            { "role": "chat",      "model_id": "qwen3:8b",     "vram_gb": 7.5 },
            { "role": "keep-warm", "model_id": "gpt-oss:120b", "vram_gb": 64.0 },
            { "role": "transient", "model_id": "phi:3b",       "vram_gb": 3.0 }
        ],
        "free_vram_gb": 21.5,
        "baseline_vram_gb": 96.0,
        "pinned_chat_model": "qwen3:8b"
    });
    std::fs::write(&path, snapshot.to_string()).unwrap();
    std::env::set_var("CHORD_RESIDENCY_STATE_PATH", path.to_str().unwrap());

    let out = ServingResidencyStatus.execute(json!({})).await.unwrap();

    assert!(out.contains("resident=3"), "out: {out}");
    assert!(out.contains("chat_pinned=true"));
    assert!(out.contains("pinned_chat_model=qwen3:8b"));
    assert!(out.contains("role=chat"));
    assert!(out.contains("role=keep-warm"));
    assert!(out.contains("role=transient"));
    assert!(out.contains("free_vram_gb=21.5"));
    // S6: roles only — no endpoint/host/IP/path leaks.
    assert!(!out.contains("http"));
    assert!(!out.contains("192.168"));
    assert!(!out.to_lowercase().contains("/tmp"));

    std::env::remove_var("CHORD_RESIDENCY_STATE_PATH");
}

#[tokio::test]
#[serial]
async fn residency_status_idle_reports_resident_zero_at_baseline() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("residency.json");
    // IDLE: no residents, free VRAM == baseline, nothing pinned.
    let snapshot = json!({
        "residents": [],
        "free_vram_gb": 96.0,
        "baseline_vram_gb": 96.0,
        "pinned_chat_model": null
    });
    std::fs::write(&path, snapshot.to_string()).unwrap();
    std::env::set_var("CHORD_RESIDENCY_STATE_PATH", path.to_str().unwrap());

    let out = ServingResidencyStatus.execute(json!({})).await.unwrap();

    assert!(out.contains("resident=0"), "out: {out}");
    assert!(out.contains("state=IDLE"));
    assert!(out.contains("chat_pinned=false"));
    assert!(out.contains("free_vram_gb=96.0"));
    assert!(out.contains("baseline_vram_gb=96.0"));

    std::env::remove_var("CHORD_RESIDENCY_STATE_PATH");
}

#[tokio::test]
#[serial]
async fn residency_status_missing_snapshot_reads_as_idle() {
    let dir = tempfile::tempdir().unwrap();
    // Point at a path that does NOT exist → IDLE, not a crash.
    let path = dir.path().join("never-written.json");
    std::env::set_var("CHORD_RESIDENCY_STATE_PATH", path.to_str().unwrap());

    let out = ServingResidencyStatus.execute(json!({})).await.unwrap();
    assert!(out.contains("resident=0"));
    assert!(out.contains("state=IDLE"));

    std::env::remove_var("CHORD_RESIDENCY_STATE_PATH");
}

#[tokio::test]
#[serial]
async fn residency_status_unconfigured_is_clear_not_configured() {
    std::env::remove_var("CHORD_RESIDENCY_STATE_PATH");
    let err = ServingResidencyStatus.execute(json!({})).await.unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("CHORD_RESIDENCY_STATE_PATH"), "msg: {msg}");
}

// ── profile refresh (mocked Chord control) ───────────────────────────────────

#[tokio::test]
#[serial]
async fn refresh_signals_chord_reload() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(POST).path("/serving/reload");
        then.status(202);
    });
    std::env::set_var("CHORD_CONTROL_URL", server.base_url());

    let out = ServingProfileRefresh.execute(json!({})).await.unwrap();
    assert!(out.contains("reload signaled"), "out: {out}");
    mock.assert(); // the reload signal actually fired

    std::env::remove_var("CHORD_CONTROL_URL");
}

#[tokio::test]
#[serial]
async fn refresh_rejected_status_is_genericized() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(POST).path("/serving/reload");
        then.status(503);
    });
    std::env::set_var("CHORD_CONTROL_URL", server.base_url());

    let err = ServingProfileRefresh.execute(json!({})).await.unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("503"), "msg: {msg}");
    // S6: no host/IP echoed back to the operator.
    assert!(!msg.contains("127.0.0.1"));
    assert!(!msg.contains("192.168"));

    std::env::remove_var("CHORD_CONTROL_URL");
}

#[tokio::test]
#[serial]
async fn refresh_unreachable_chord_is_clear_failure() {
    // A port nothing listens on → connection refused → genericized failure.
    std::env::set_var("CHORD_CONTROL_URL", "http://localhost:1");
    let err = ServingProfileRefresh.execute(json!({})).await.unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("unreachable"), "msg: {msg}");
    // No host literal leaked.
    assert!(!msg.contains("localhost:1"));

    std::env::remove_var("CHORD_CONTROL_URL");
}

#[tokio::test]
#[serial]
async fn refresh_unconfigured_is_clear_not_configured() {
    std::env::remove_var("CHORD_CONTROL_URL");
    let err = ServingProfileRefresh.execute(json!({})).await.unwrap_err();
    assert!(err.to_string().contains("CHORD_CONTROL_URL"));
}

// ── profile get (seeded Postgres, gated on DATABASE_URL) ──────────────────────

/// Connect + seed a serving row, returning the pool. Skips (returns None) when no
/// DATABASE_URL is configured so the suite stays green on a DB-less box.
async fn seed_pool() -> Option<sqlx::PgPool> {
    let url = std::env::var("INTAKE_DATABASE_URL")
        .ok()
        .or_else(|| std::env::var("DATABASE_URL").ok())?;
    let pool = sqlx::PgPool::connect(&url).await.ok()?;
    // Ensure the SRV-01 schema is present (idempotent).
    terminus_rs::intake::serving::schema::migrate(&pool)
        .await
        .ok()?;
    Some(pool)
}

#[tokio::test]
#[serial]
async fn profile_get_seeded_model_returns_expected_shape() {
    let Some(pool) = seed_pool().await else {
        eprintln!("skipping: no DATABASE_URL");
        return;
    };
    let model = "srv07-test:qwen3-8b";
    sqlx::query("DELETE FROM serving_profile WHERE model_id = $1")
        .bind(model)
        .execute(&pool)
        .await
        .unwrap();

    let profile = terminus_rs::intake::serving::ServingProfile {
        model_id: terminus_rs::intake::serving::ModelId::from(model),
        backend_tag: terminus_rs::intake::serving::ServingBackend::LlamaGpu,
        best_runtime: terminus_rs::intake::serving::Runtime::LlamaCpp,
        env_json: r#"{"gfx_override":"11.0.0","mmap_flag":"0"}"#.into(),
        tok_s: Some(42.4),
        vram_or_ram_peak_gb: Some(7.5),
        cold_load_s: Some(12.0),
        keep_warm: false,
        fallback_runtime: Some(terminus_rs::intake::serving::Runtime::Ollama),
        exclusion_reason: terminus_rs::intake::serving::ExclusionReason::None,
        recheck_trigger: terminus_rs::intake::serving::RecheckTrigger::None,
        provenance: None,
    };
    terminus_rs::intake::serving::schema::upsert_serving_profile(
        &pool,
        uuid::Uuid::new_v4(),
        &profile,
    )
    .await
    .unwrap();

    let out = ServingProfileGet
        .execute(json!({ "model_id": model }))
        .await
        .unwrap();

    assert!(out.contains(model), "out: {out}");
    assert!(out.contains("backend=llama-gpu"));
    assert!(out.contains("runtime=llama-cpp"));
    assert!(out.contains("fallback=ollama"));
    assert!(out.contains("tok/s=42.4"));
    assert!(out.contains("launch_flags=[gfx_override, mmap_flag]"));
    assert!(out.contains("exclusion=none"));
    // S6: the env VALUE (gfx id) must never be surfaced — names only.
    assert!(!out.contains("11.0.0"), "leaked env value: {out}");

    sqlx::query("DELETE FROM serving_profile WHERE model_id = $1")
        .bind(model)
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
#[serial]
async fn profile_get_unprofiled_model_is_clean_no_profile() {
    let Some(pool) = seed_pool().await else {
        eprintln!("skipping: no DATABASE_URL");
        return;
    };
    let model = "srv07-test:never-profiled-zzz";
    sqlx::query("DELETE FROM serving_profile WHERE model_id = $1")
        .bind(model)
        .execute(&pool)
        .await
        .unwrap();

    // Unprofiled model → a clear Ok result, NOT an error crash.
    let out = ServingProfileGet
        .execute(json!({ "model_id": model }))
        .await
        .unwrap();
    assert!(out.contains("No serving profile"), "out: {out}");
    assert!(out.contains(model));
}

#[tokio::test]
#[serial]
async fn profile_get_rejects_empty_model_id() {
    let err = ServingProfileGet
        .execute(json!({ "model_id": "   " }))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("model_id"));
}
