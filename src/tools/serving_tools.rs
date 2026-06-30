//! S85 SRV-07 — Serving-profile control + status tools (Terminus MCP).
//!
//! Operator-facing tools to inspect and operate the serving profile produced by
//! the harness (SRV-01..03) and consumed by Chord (SRV-04..06). Read-mostly; the
//! refresh is the one mutating action.
//!
//! Three [`RustTool`]s:
//!   - `serving_profile_get(model_id)`  — a model's serving row(s): runtime, env,
//!     tok/s, keep_warm, exclusion reason. Builds on SRV-01 (`serving_profile`
//!     table via [`crate::intake::serving::schema`]). An UNPROFILED model returns a
//!     clear "no profile" result, NOT an error crash.
//!   - `serving_residency_status()`     — current residents, free VRAM, the pinned
//!     chat role. Reads the residency snapshot SRV-05 writes (path from a config
//!     helper). IDLE ⇒ resident=0, free VRAM at baseline.
//!   - `serving_profile_refresh()`      — signal Chord to reload its routing map
//!     from the DB (POST to the Chord control endpoint from a config helper). Chord
//!     unreachable ⇒ a clear, genericized failure.
//!
//! ## Sanitization (S6 — outputs are operator-facing and genericized)
//! Every output is built from *typed, allow-listed* fields only: runtime / backend
//! /exclusion enums (fixed wire strings), the model id (a registry key, not an
//! infra string), numeric measurements, and the `env_json` reduced to its KEY set
//! (the launch-flag NAMES, never their values — values can hold gfx ids / lib
//! paths). No URL, host, IP, file path, or secret from config/vault is ever placed
//! in a tool output:
//!   - residency residents are reported by ROLE (`chat` / `keep-warm` / `transient`)
//!     and model id, never by endpoint;
//!   - the Chord control URL / residency state PATH are read from config but NEVER
//!     echoed — error text is generic ("Chord control endpoint unreachable"), not
//!     the host;
//!   - DB connection errors are surfaced as a generic "serving profile store
//!     unavailable", never the connection string.
//! This keeps the `pii_gate` hook clean (no infra literal in source) AND keeps the
//! runtime output free of infra/secret leakage.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::config;
use crate::error::ToolError;
use crate::intake::serving::schema;
use crate::registry::ToolRegistry;
use crate::tool::RustTool;

// ---------------------------------------------------------------------------
// Sanitization helpers (S6)
// ---------------------------------------------------------------------------

/// Reduce an `env_json` object to its sorted KEY set — the launch-flag *names*
/// (e.g. `gfx_override`, `mmap_flag`, `flash_attn`, `cpu_lib`) without their
/// values, which can carry gfx ids / library paths we must not surface.
fn env_flag_names(env_json: &str) -> Vec<String> {
    let mut names: Vec<String> = serde_json::from_str::<Value>(env_json)
        .ok()
        .and_then(|v| v.as_object().map(|m| m.keys().cloned().collect()))
        .unwrap_or_default();
    names.sort();
    names
}

/// Generic, infra-free message for a serving-store connection failure. The raw
/// error can echo the connection string, so we never forward it.
fn store_unavailable() -> ToolError {
    ToolError::Database("serving profile store unavailable".into())
}

// ---------------------------------------------------------------------------
// serving_profile_get
// ---------------------------------------------------------------------------

/// One serving row, read back from `serving_profile` (sanitized columns only — no
/// `run_id`, no raw `env_json` values). Built from a positional tuple row (the
/// crate's sqlx build has the `macros`/`FromRow` derive disabled, matching
/// `intake::serving::schema`'s tuple-based `query_as` style).
#[derive(Debug)]
struct ServingRow {
    backend_tag: String,
    best_runtime: String,
    env_json: String,
    tok_s: Option<f64>,
    vram_or_ram_peak_gb: Option<f64>,
    cold_load_s: Option<f64>,
    keep_warm: bool,
    fallback_runtime: Option<String>,
    exclusion_reason: String,
    recheck_trigger: String,
    provenance: Option<String>,
}

/// The positional column tuple `serving_profile_get` selects, in SELECT order.
type ServingRowTuple = (
    String,         // backend_tag
    String,         // best_runtime
    String,         // env_json (::text)
    Option<f64>,    // tok_s
    Option<f64>,    // vram_or_ram_peak_gb
    Option<f64>,    // cold_load_s
    bool,           // keep_warm
    Option<String>, // fallback_runtime
    String,         // exclusion_reason
    String,         // recheck_trigger
    Option<String>, // provenance
);

impl From<ServingRowTuple> for ServingRow {
    fn from(t: ServingRowTuple) -> Self {
        ServingRow {
            backend_tag: t.0,
            best_runtime: t.1,
            env_json: t.2,
            tok_s: t.3,
            vram_or_ram_peak_gb: t.4,
            cold_load_s: t.5,
            keep_warm: t.6,
            fallback_runtime: t.7,
            exclusion_reason: t.8,
            recheck_trigger: t.9,
            provenance: t.10,
        }
    }
}

pub struct ServingProfileGet;

#[async_trait]
impl RustTool for ServingProfileGet {
    fn name(&self) -> &str {
        "serving_profile_get"
    }

    fn description(&self) -> &str {
        "Show a model's serving profile (chosen runtime, launch-flag names, tok/s, \
         keep_warm, exclusion reason) across its serving backends. Returns a clear \
         'no profile' result for an unprofiled model."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "model_id": {
                    "type": "string",
                    "description": "Model registry id (S83-consistent, e.g. 'qwen3:8b')"
                }
            },
            "required": ["model_id"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let model_id = args["model_id"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::InvalidArgument("'model_id' must be a non-empty string".into()))?;

        let pool = schema::get_pool().await.map_err(|_| store_unavailable())?;

        // env_json is reduced to its key set in formatting; never bound into output raw.
        let tuples: Vec<ServingRowTuple> = sqlx::query_as::<_, ServingRowTuple>(
            "SELECT backend_tag, best_runtime, env_json::text, tok_s, \
                    vram_or_ram_peak_gb, cold_load_s, keep_warm, fallback_runtime, \
                    exclusion_reason, recheck_trigger, provenance \
             FROM serving_profile \
             WHERE model_id = $1 \
             ORDER BY backend_tag",
        )
        .bind(model_id)
        .fetch_all(&pool)
        .await
        .map_err(|_| store_unavailable())?;
        let rows: Vec<ServingRow> = tuples.into_iter().map(ServingRow::from).collect();

        // Unprofiled model → a CLEAR result, not a crash (Ok, not Err).
        if rows.is_empty() {
            return Ok(format!(
                "No serving profile for model '{model_id}'. The serving harness has \
                 not recorded a row for this model yet."
            ));
        }

        Ok(format_profile(model_id, &rows))
    }
}

/// Render the serving rows for a model into a sanitized, operator-facing block.
fn format_profile(model_id: &str, rows: &[ServingRow]) -> String {
    let mut out = format!(
        "Serving profile for '{model_id}' ({} backend(s)):\n\n",
        rows.len()
    );
    for r in rows {
        out.push_str(&format!("• backend={} runtime={}", r.backend_tag, r.best_runtime));
        if let Some(fb) = &r.fallback_runtime {
            out.push_str(&format!(" fallback={fb}"));
        }
        out.push('\n');
        out.push_str(&format!("  keep_warm={}", r.keep_warm));
        if let Some(t) = r.tok_s {
            out.push_str(&format!(" tok/s={t:.1}"));
        }
        if let Some(p) = r.vram_or_ram_peak_gb {
            out.push_str(&format!(" peak_gb={p:.1}"));
        }
        if let Some(c) = r.cold_load_s {
            out.push_str(&format!(" cold_load_s={c:.0}"));
        }
        out.push('\n');
        // Launch-flag NAMES only — never their (possibly infra-bearing) values.
        let flags = env_flag_names(&r.env_json);
        if !flags.is_empty() {
            out.push_str(&format!("  launch_flags=[{}]\n", flags.join(", ")));
        }
        out.push_str(&format!(
            "  exclusion={} recheck={}\n",
            r.exclusion_reason, r.recheck_trigger
        ));
        if let Some(prov) = &r.provenance {
            out.push_str(&format!("  provenance: {prov}\n"));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// serving_residency_status
// ---------------------------------------------------------------------------

/// The residency snapshot SRV-05's residency manager writes. This is the SRV-07
/// reader's view of that contract: ROLE-keyed residents (never an endpoint), a
/// free-VRAM number, the baseline VRAM, and which model is the pinned chat role.
///
/// The producer may carry more fields; we deserialize only the sanitized subset
/// (`#[serde(default)]` everywhere) so an IDLE/partial snapshot reads cleanly.
#[derive(Debug, Default, Deserialize)]
struct ResidencySnapshot {
    /// Resident models, by role. Empty ⇒ IDLE.
    #[serde(default)]
    residents: Vec<Resident>,
    /// Free VRAM (GB) right now.
    #[serde(default)]
    free_vram_gb: f64,
    /// Baseline (total available) VRAM (GB) — what "free" returns to when IDLE.
    #[serde(default)]
    baseline_vram_gb: f64,
    /// The model id pinned as the live chat role (never evicted). `None` when no
    /// chat model is currently pinned.
    #[serde(default)]
    pinned_chat_model: Option<String>,
}

/// One resident model. ROLE + model id + footprint — no endpoint, no host.
#[derive(Debug, Default, Deserialize)]
struct Resident {
    /// `chat` | `keep-warm` | `transient` (the tier role from SRV-05's eviction policy).
    #[serde(default)]
    role: String,
    /// Model id (S83-consistent registry key).
    #[serde(default)]
    model_id: String,
    /// VRAM footprint (GB) of this resident.
    #[serde(default)]
    vram_gb: f64,
}

pub struct ServingResidencyStatus;

#[async_trait]
impl RustTool for ServingResidencyStatus {
    fn name(&self) -> &str {
        "serving_residency_status"
    }

    fn description(&self) -> &str {
        "Show Chord's current serving residency: resident models (by role), free \
         VRAM, and the pinned chat model. IDLE reports resident=0 with free VRAM at \
         baseline."
    }

    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        let path = config::chord_residency_state_path().ok_or_else(|| {
            ToolError::NotConfigured(
                "CHORD_RESIDENCY_STATE_PATH not set — residency status requires the \
                 Chord residency-state snapshot path"
                    .into(),
            )
        })?;

        // Missing snapshot ⇒ treat as IDLE (Chord has not written one yet), not a
        // crash. A present-but-unreadable file is a genuine, genericized error.
        let snapshot = match std::fs::read_to_string(&path) {
            Ok(contents) => serde_json::from_str::<ResidencySnapshot>(&contents).map_err(|_| {
                ToolError::Execution("residency state snapshot is unreadable".into())
            })?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => ResidencySnapshot::default(),
            Err(_) => {
                // Do NOT echo the path (it is an infra mount) — generic message only.
                return Err(ToolError::Execution(
                    "residency state snapshot could not be read".into(),
                ));
            }
        };

        Ok(format_residency(&snapshot))
    }
}

/// Render the residency snapshot into a sanitized, operator-facing block.
fn format_residency(s: &ResidencySnapshot) -> String {
    let mut out = String::new();
    out.push_str(&format!("resident={}\n", s.residents.len()));
    out.push_str(&format!(
        "free_vram_gb={:.1} baseline_vram_gb={:.1}\n",
        s.free_vram_gb, s.baseline_vram_gb
    ));

    let chat_pinned = s.pinned_chat_model.is_some();
    out.push_str(&format!("chat_pinned={chat_pinned}"));
    if let Some(m) = &s.pinned_chat_model {
        out.push_str(&format!(" pinned_chat_model={m}"));
    }
    out.push('\n');

    if s.residents.is_empty() {
        out.push_str("state=IDLE (no resident models; free VRAM at baseline)\n");
    } else {
        out.push_str("residents:\n");
        for r in &s.residents {
            // Role + model id + footprint only — never an endpoint/host.
            out.push_str(&format!(
                "  • role={} model={} vram_gb={:.1}\n",
                r.role, r.model_id, r.vram_gb
            ));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// serving_profile_refresh
// ---------------------------------------------------------------------------

pub struct ServingProfileRefresh;

#[async_trait]
impl RustTool for ServingProfileRefresh {
    fn name(&self) -> &str {
        "serving_profile_refresh"
    }

    fn description(&self) -> &str {
        "Signal Chord to reload its serving routing map from the database (after the \
         serving harness has written new/updated rows). The one mutating serving tool."
    }

    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        let base = config::chord_control_url().ok_or_else(|| {
            ToolError::NotConfigured(
                "CHORD_CONTROL_URL not set — profile refresh requires the Chord \
                 control endpoint"
                    .into(),
            )
        })?;
        let url = format!("{}/serving/reload", base.trim_end_matches('/'));

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|_| ToolError::Execution("could not build control client".into()))?;

        // Chord unreachable / non-2xx ⇒ a CLEAR, genericized failure (no host echoed).
        let resp = client
            .post(&url)
            .send()
            .await
            .map_err(|_| ToolError::Execution("Chord control endpoint unreachable".into()))?;

        if resp.status().is_success() {
            Ok("Chord routing map reload signaled".into())
        } else {
            Err(ToolError::Execution(format!(
                "Chord rejected the routing-map reload (status {})",
                resp.status().as_u16()
            )))
        }
    }
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

pub fn register(registry: &mut ToolRegistry) {
    registry.register_or_replace(Box::new(ServingProfileGet));
    registry.register_or_replace(Box::new(ServingResidencyStatus));
    registry.register_or_replace(Box::new(ServingProfileRefresh));
}

// ---------------------------------------------------------------------------
// Unit tests (pure-function + metadata; DB/HTTP behavior in tests/tools/)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn meta_ok(tool: &dyn RustTool) {
        assert!(!tool.name().is_empty());
        assert!(!tool.description().is_empty());
        assert_eq!(tool.parameters()["type"], "object");
    }

    #[test]
    fn all_three_have_metadata() {
        meta_ok(&ServingProfileGet);
        meta_ok(&ServingResidencyStatus);
        meta_ok(&ServingProfileRefresh);
        assert_eq!(ServingProfileGet.name(), "serving_profile_get");
        assert_eq!(ServingResidencyStatus.name(), "serving_residency_status");
        assert_eq!(ServingProfileRefresh.name(), "serving_profile_refresh");
    }

    #[test]
    fn get_requires_model_id() {
        let p = ServingProfileGet.parameters();
        assert!(p["required"].as_array().unwrap().iter().any(|v| v == "model_id"));
    }

    #[test]
    fn status_and_refresh_take_no_args() {
        assert!(ServingResidencyStatus.parameters()["properties"]
            .as_object()
            .unwrap()
            .is_empty());
        assert!(ServingProfileRefresh.parameters()["properties"]
            .as_object()
            .unwrap()
            .is_empty());
    }

    #[test]
    fn env_flag_names_returns_sorted_keys_only_no_values() {
        // Values can carry a gfx id / lib path; only NAMES come out, sorted.
        let env = r#"{"mmap_flag":"0","gfx_override":"11.0.0","cpu_lib":"/x/y"}"#;
        let names = env_flag_names(env);
        assert_eq!(names, vec!["cpu_lib", "gfx_override", "mmap_flag"]);
        // None of the (infra-bearing) values leak.
        assert!(!names.iter().any(|n| n.contains("11.0.0") || n.contains("/x/y")));
        // Malformed / empty → empty set, no panic.
        assert!(env_flag_names("not json").is_empty());
        assert!(env_flag_names("{}").is_empty());
    }

    #[test]
    fn format_profile_is_sanitized_and_shows_fields() {
        let rows = vec![ServingRow {
            backend_tag: "llama-gpu".into(),
            best_runtime: "llama-cpp".into(),
            env_json: r#"{"gfx_override":"11.0.0","mmap_flag":"0"}"#.into(),
            tok_s: Some(42.4),
            vram_or_ram_peak_gb: Some(7.5),
            cold_load_s: Some(12.0),
            keep_warm: false,
            fallback_runtime: Some("ollama".into()),
            exclusion_reason: "none".into(),
            recheck_trigger: "none".into(),
            provenance: None,
        }];
        let out = format_profile("qwen3:8b", &rows);
        assert!(out.contains("qwen3:8b"));
        assert!(out.contains("runtime=llama-cpp"));
        assert!(out.contains("fallback=ollama"));
        assert!(out.contains("keep_warm=false"));
        assert!(out.contains("tok/s=42.4"));
        assert!(out.contains("launch_flags=[gfx_override, mmap_flag]"));
        assert!(out.contains("exclusion=none"));
        // Sanitization: the env VALUE (gfx id) never appears.
        assert!(!out.contains("11.0.0"));
    }

    #[test]
    fn format_profile_carries_provenance_note() {
        let rows = vec![ServingRow {
            backend_tag: "llama-gpu".into(),
            best_runtime: "llama-cpp".into(),
            env_json: "{}".into(),
            tok_s: None,
            vram_or_ram_peak_gb: None,
            cold_load_s: None,
            keep_warm: false,
            fallback_runtime: None,
            exclusion_reason: "build-conditional".into(),
            recheck_trigger: "llama-cpp-version-bump".into(),
            provenance: Some("verdict by inference; weights absent at confirmation".into()),
        }];
        let out = format_profile("glm-4.7-flash", &rows);
        assert!(out.contains("provenance: verdict by inference"));
        assert!(out.contains("exclusion=build-conditional"));
        assert!(out.contains("recheck=llama-cpp-version-bump"));
    }

    #[test]
    fn format_residency_idle_reports_resident_zero_at_baseline() {
        let snap = ResidencySnapshot {
            residents: vec![],
            free_vram_gb: 96.0,
            baseline_vram_gb: 96.0,
            pinned_chat_model: None,
        };
        let out = format_residency(&snap);
        assert!(out.contains("resident=0"));
        assert!(out.contains("state=IDLE"));
        assert!(out.contains("chat_pinned=false"));
        // free VRAM at baseline.
        assert!(out.contains("free_vram_gb=96.0"));
        assert!(out.contains("baseline_vram_gb=96.0"));
    }

    #[test]
    fn format_residency_serving_reports_roles_and_pin_no_endpoints() {
        let snap = ResidencySnapshot {
            residents: vec![
                Resident { role: "chat".into(), model_id: "qwen3:8b".into(), vram_gb: 7.5 },
                Resident { role: "keep-warm".into(), model_id: "gpt-oss:120b".into(), vram_gb: 64.0 },
            ],
            free_vram_gb: 24.5,
            baseline_vram_gb: 96.0,
            pinned_chat_model: Some("qwen3:8b".into()),
        };
        let out = format_residency(&snap);
        assert!(out.contains("resident=2"));
        assert!(out.contains("chat_pinned=true"));
        assert!(out.contains("pinned_chat_model=qwen3:8b"));
        assert!(out.contains("role=chat"));
        assert!(out.contains("role=keep-warm"));
        // Roles only — no infra endpoint/host/IP shape anywhere.
        assert!(!out.contains("http"));
        assert!(!out.contains("192.168"));
    }

    #[test]
    fn residency_snapshot_tolerates_partial_json() {
        // A producer writing only a subset still reads cleanly (serde defaults).
        let snap: ResidencySnapshot = serde_json::from_str(r#"{"free_vram_gb": 80.0}"#).unwrap();
        assert!(snap.residents.is_empty());
        assert_eq!(snap.free_vram_gb, 80.0);
        assert!(snap.pinned_chat_model.is_none());
    }

    #[test]
    fn registration_adds_three() {
        let mut reg = ToolRegistry::new();
        register(&mut reg);
        assert!(reg.contains("serving_profile_get"));
        assert!(reg.contains("serving_residency_status"));
        assert!(reg.contains("serving_profile_refresh"));
        assert_eq!(reg.len(), 3);
    }
}
