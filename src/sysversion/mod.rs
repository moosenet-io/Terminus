//! `system_version` — a single never-fail tool that reports the version and
//! reachability of every component in the Lumina Constellation.
//!
//! ## Design contract
//! - **Never errors the whole tool** because one probe failed. Each service is
//!   probed independently with a hard 2-second timeout; an unreachable service
//!   is reported as `{"status": "unreachable"}`, not an error.
//! - **60-second process-wide cache** so repeated calls don't hammer endpoints.
//! - **No hardcoded hosts/tokens.** Service URLs come from the same env vars the
//!   other terminus modules already read. Unset URLs report cleanly as
//!   `"not_configured"`.
//! - **secrets_backend never exposes a version** — reachable/unreachable only.
//!
//! ## Configuration (env vars — reused from existing modules)
//! | Service          | Env var(s)                                   |
//! |------------------|----------------------------------------------|
//! | matrix_homeserver| `MATRIX_HOMESERVER`                          |
//! | model_server     | `OLLAMA_URL`→`OLLAMA_BASE_URL`→`OLLAMA_CPU_URL`|
//! | llm_proxy        | `LITELLM_URL`                               |
//! | secrets_backend  | `INFISICAL_URL`                             |
//! | work_queue       | `PLANE_API_URL`                             |
//! | git_server       | `GITEA_URL`                                 |
//! | metrics_collector| `PROMETHEUS_URL`                            |
//! | dgem_daemon      | `DGEM_BASE_URL` | `DGEM_BIND`+`DGEM_HTTP_PORT`|
//! | chord_proxy      | `CHORD_PROXY_URL` (default 127.0.0.1:8099)   |
//! | inference        | chord control API (`CHORD_CONTROL_URL` or    |
//! |                  | derived from `CHORD_PROXY_URL`+control port;  |
//! |                  | default 127.0.0.1:8090 — co-located)          |

use async_trait::async_trait;
use reqwest::Client;
use serde_json::{json, Value};
use std::env;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::tool::RustTool;

const PROBE_TIMEOUT_SECS: u64 = 2;
const CACHE_TTL_SECS: u64 = 60;

/// Process-wide cache: (collected_at, full_report_json).
static CACHE: OnceLock<Mutex<Option<(Instant, Value)>>> = OnceLock::new();

fn cache() -> &'static Mutex<Option<(Instant, Value)>> {
    CACHE.get_or_init(|| Mutex::new(None))
}

/// Return a cached report if one was collected within the TTL window.
fn cached_report() -> Option<Value> {
    let guard = cache().lock().ok()?;
    let (at, val) = guard.as_ref()?;
    if at.elapsed() < Duration::from_secs(CACHE_TTL_SECS) {
        Some(val.clone())
    } else {
        None
    }
}

/// Store a freshly collected report in the cache.
fn store_report(val: &Value) {
    if let Ok(mut guard) = cache().lock() {
        *guard = Some((Instant::now(), val.clone()));
    }
}

/// A reqwest client with a hard per-request 2s timeout baked in.
fn probe_client() -> Client {
    Client::builder()
        .timeout(Duration::from_secs(PROBE_TIMEOUT_SECS))
        .build()
        .unwrap_or_default()
}

/// Read an env var, returning None for missing or empty values.
fn env_url(key: &str) -> Option<String> {
    env::var(key)
        .ok()
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())
}

/// GET a URL and return the parsed JSON body if the request succeeds (2xx).
/// Any failure (DNS, connect, timeout, non-2xx, bad JSON) yields None.
async fn get_json(client: &Client, url: &str) -> Option<Value> {
    let resp = client.get(url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.json::<Value>().await.ok()
}

/// GET a URL and report only whether it was reachable (any HTTP response, even
/// an error status, counts as reachable — the host answered).
async fn reachable(client: &Client, url: &str) -> bool {
    client.get(url).send().await.is_ok()
}

/// Standard "not configured" service entry.
fn not_configured() -> Value {
    json!({"status": "not_configured"})
}

/// Standard "unreachable" service entry.
fn unreachable() -> Value {
    json!({"status": "unreachable"})
}

// ─── Per-service probes ──────────────────────────────────────────────────────

/// Matrix homeserver — try the federation version endpoint.
async fn probe_matrix(client: &Client) -> Value {
    let Some(base) = env_url("MATRIX_HOMESERVER") else {
        return not_configured();
    };
    match get_json(client, &format!("{base}/_matrix/federation/v1/version")).await {
        Some(v) => {
            let name = v.pointer("/server/name").and_then(|x| x.as_str());
            let ver = v.pointer("/server/version").and_then(|x| x.as_str());
            json!({"status": "reachable", "server": name, "version": ver})
        }
        None => {
            // Federation may be closed; fall back to a plain reachability ping.
            if reachable(client, &format!("{base}/_matrix/client/versions")).await {
                json!({"status": "reachable"})
            } else {
                unreachable()
            }
        }
    }
}

/// Resolve the Ollama base URL. The live chord-proxy runtime sets `OLLAMA_URL`
/// (not `OLLAMA_BASE_URL`); other modules use either name. Fallback order:
/// `OLLAMA_URL` → `OLLAMA_BASE_URL` → `OLLAMA_CPU_URL`. First set wins.
fn ollama_base_url() -> Option<String> {
    env_url("OLLAMA_URL")
        .or_else(|| env_url("OLLAMA_BASE_URL"))
        .or_else(|| env_url("OLLAMA_CPU_URL"))
}

/// Ollama model server — `/api/version` returns `{"version": "..."}`.
async fn probe_ollama(client: &Client) -> Value {
    let Some(base) = ollama_base_url() else {
        return not_configured();
    };
    match get_json(client, &format!("{base}/api/version")).await {
        Some(v) => json!({
            "status": "reachable",
            "version": v.get("version").and_then(|x| x.as_str()),
        }),
        None => unreachable(),
    }
}

/// LiteLLM proxy — `/health/liveliness` if reachable, else report configured.
async fn probe_litellm(client: &Client) -> Value {
    let Some(base) = env_url("LITELLM_URL") else {
        return not_configured();
    };
    if reachable(client, &format!("{base}/health/liveliness")).await
        || reachable(client, &format!("{base}/health")).await
    {
        json!({"status": "reachable"})
    } else {
        // Configured but its health endpoint is unauthenticated/unavailable.
        json!({"status": "configured"})
    }
}

/// <secret-manager> secrets backend — reachability ONLY. Never expose a version.
async fn probe_infisical(client: &Client) -> Value {
    let Some(base) = env_url("INFISICAL_URL") else {
        return not_configured();
    };
    if reachable(client, &format!("{base}/api/status")).await
        || reachable(client, base.as_str()).await
    {
        json!({"status": "reachable"})
    } else {
        unreachable()
    }
}

/// Plane work queue — ping the API root; reachability only.
async fn probe_plane(client: &Client) -> Value {
    let Some(base) = env_url("PLANE_API_URL") else {
        return not_configured();
    };
    if reachable(client, &base).await {
        json!({"status": "reachable"})
    } else {
        unreachable()
    }
}

/// Gitea git server — `/api/v1/version` returns `{"version": "..."}`.
async fn probe_gitea(client: &Client) -> Value {
    let Some(base) = env_url("GITEA_URL") else {
        return not_configured();
    };
    match get_json(client, &format!("{base}/api/v1/version")).await {
        Some(v) => json!({
            "status": "reachable",
            "version": v.get("version").and_then(|x| x.as_str()),
        }),
        None => unreachable(),
    }
}

/// Prometheus metrics collector — ping `/-/healthy`; reachability only.
async fn probe_prometheus(client: &Client) -> Value {
    let Some(base) = env_url("PROMETHEUS_URL") else {
        return not_configured();
    };
    if reachable(client, &format!("{base}/-/healthy")).await
        || reachable(client, &base).await
    {
        json!({"status": "reachable"})
    } else {
        unreachable()
    }
}

/// Resolve the DiffusionGemma daemon base URL, matching the dgem module's logic:
/// `DGEM_BASE_URL`, else `http://{DGEM_BIND|127.0.0.1}:{DGEM_HTTP_PORT|8877}`.
fn dgem_base_url() -> String {
    if let Some(url) = env_url("DGEM_BASE_URL") {
        return url;
    }
    let bind = env::var("DGEM_BIND")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "127.0.0.1".to_string());
    let port = env::var("DGEM_HTTP_PORT")
        .ok()
        .and_then(|s| s.trim().parse::<u16>().ok())
        .unwrap_or(8877);
    format!("http://{bind}:{port}")
}

/// DGEM daemon — `/status`.
async fn probe_dgem(client: &Client) -> Value {
    let base = dgem_base_url();
    match get_json(client, &format!("{base}/status")).await {
        Some(v) => {
            let mut out = json!({"status": "reachable"});
            if let Some(obj) = out.as_object_mut() {
                obj.insert("daemon".to_string(), v);
            }
            out
        }
        None => unreachable(),
    }
}

/// Resolve the chord proxy base URL. Co-located with this tool, so when
/// `CHORD_PROXY_URL` is unset we default to the local chord port (8099).
fn chord_proxy_base() -> String {
    env_url("CHORD_PROXY_URL").unwrap_or_else(|| "http://127.0.0.1:8099".to_string())
}

/// Chord proxy `/health` — includes version/commit/terminus_rs after step 5.
async fn probe_chord(client: &Client) -> Value {
    let base = chord_proxy_base();
    match get_json(client, &format!("{base}/health")).await {
        Some(v) => json!({
            "status": "reachable",
            "version": v.get("version").and_then(|x| x.as_str()),
            "commit": v.get("commit").and_then(|x| x.as_str()),
            "terminus_rs": v.get("terminus_rs").and_then(|x| x.as_str()),
        }),
        None => unreachable(),
    }
}

/// Resolve the chord control API base URL. Prefers `CHORD_CONTROL_URL`; else
/// derives from `CHORD_PROXY_URL` host with the control port
/// (`CHORD_CONTROL_PORT`, default 8090). Co-located with this tool, so when no
/// env is set / derivation fails we default to the local control port
/// (`http://127.0.0.1:8090`).
fn chord_control_base() -> String {
    if let Some(url) = env_url("CHORD_CONTROL_URL") {
        return url;
    }
    let port = env::var("CHORD_CONTROL_PORT")
        .ok()
        .and_then(|s| s.trim().parse::<u16>().ok())
        .unwrap_or(8090);
    let local = format!("http://127.0.0.1:{port}");
    let Some(proxy) = env_url("CHORD_PROXY_URL") else {
        return local;
    };
    // Swap the port on the proxy URL host. Best-effort string surgery.
    if let Ok(mut url) = reqwest::Url::parse(&proxy) {
        if url.set_port(Some(port)).is_ok() {
            return url.as_str().trim_end_matches('/').to_string();
        }
    }
    local
}

/// Inference state from the chord control model registry (`GET /api/models`).
/// Reports hot_model / warm_models / vram where derivable, never erroring.
async fn probe_inference(client: &Client) -> Value {
    let base = chord_control_base();
    let Some(body) = get_json(client, &format!("{base}/api/models")).await else {
        return json!({"status": "unreachable"});
    };

    // The registry returns `{"models": [...], "count": N}`. Model records carry
    // a state (hot/warm/cold) and may carry vram usage. Be defensive about shape.
    let models = body.get("models").and_then(|m| m.as_array());
    let mut hot_model: Option<String> = None;
    let mut warm_models: Vec<String> = Vec::new();
    let mut vram: Option<Value> = None;

    if let Some(arr) = models {
        for m in arr {
            let name = m
                .get("name")
                .or_else(|| m.get("model"))
                .and_then(|x| x.as_str())
                .map(|s| s.to_string());
            let state = m
                .get("state")
                .or_else(|| m.get("status"))
                .and_then(|x| x.as_str())
                .unwrap_or("");
            if let Some(n) = name {
                match state.to_ascii_lowercase().as_str() {
                    "hot" | "loaded" | "active" => hot_model = Some(n),
                    "warm" => warm_models.push(n),
                    _ => {}
                }
            }
            if vram.is_none() {
                if let Some(v) = m.get("vram").or_else(|| m.get("vram_mb")) {
                    vram = Some(v.clone());
                }
            }
        }
    }
    if let Some(top) = body.get("vram") {
        vram = Some(top.clone());
    }

    json!({
        "status": "reachable",
        "hot_model": hot_model,
        "warm_models": warm_models,
        "vram": vram,
        "model_count": body.get("count"),
    })
}

// ─── Constellation versions ──────────────────────────────────────────────────

/// Build the `constellation` block: lumina_core / chord_proxy / terminus_rs.
async fn collect_constellation(client: &Client, chord: &Value) -> Value {
    // terminus_rs version is always available — compiled in.
    let terminus = crate::VERSION;

    // chord_proxy version comes from its /health (probed above), if reachable.
    let chord_version = chord
        .get("version")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string());
    let chord_commit = chord.get("commit").and_then(|x| x.as_str());

    // lumina-core typically has no plain HTTP version endpoint. Try a best-effort
    // probe at LUMINA_HTTP_URL/health if configured; otherwise report "unknown".
    let lumina = if let Some(base) = env_url("LUMINA_HTTP_URL") {
        match get_json(client, &format!("{base}/health")).await {
            Some(v) => json!({
                "version": v.get("version").and_then(|x| x.as_str()),
                "status": "reachable",
            }),
            None => json!({"version": "unknown", "status": "unreachable"}),
        }
    } else {
        json!({"version": "unknown", "status": "no_http_endpoint"})
    };

    json!({
        "lumina_core": lumina,
        "chord_proxy": {
            "version": chord_version,
            "commit": chord_commit,
            "status": chord.get("status"),
        },
        "terminus_rs": {"version": terminus},
    })
}

// ─── Report assembly ─────────────────────────────────────────────────────────

/// Collect the full report by probing every component concurrently.
async fn collect_report() -> Value {
    let client = probe_client();

    let (
        matrix,
        ollama,
        litellm,
        <secret-manager>,
        plane,
        gitea,
        prometheus,
        dgem,
        chord,
        inference,
    ) = tokio::join!(
        probe_matrix(&client),
        probe_ollama(&client),
        probe_litellm(&client),
        probe_infisical(&client),
        probe_plane(&client),
        probe_gitea(&client),
        probe_prometheus(&client),
        probe_dgem(&client),
        probe_chord(&client),
        probe_inference(&client),
    );

    let constellation = collect_constellation(&client, &chord).await;

    json!({
        "constellation": constellation,
        "services": {
            "matrix_homeserver": matrix,
            "model_server": ollama,
            "llm_proxy": litellm,
            "secrets_backend": <secret-manager>,
            "work_queue": plane,
            "git_server": gitea,
            "metrics_collector": prometheus,
            "dgem_daemon": dgem,
            "chord_proxy": chord,
        },
        "inference": inference,
    })
}

// ─── Tool ────────────────────────────────────────────────────────────────────

/// The `system_version` tool. Reports versions + reachability for the whole
/// constellation and never fails.
pub struct SystemVersion;

#[async_trait]
impl RustTool for SystemVersion {
    fn name(&self) -> &str {
        "system_version"
    }

    fn description(&self) -> &str {
        "Report the version and reachability of every Lumina Constellation \
         component: the core binaries (lumina-core, chord-proxy, terminus-rs), \
         supporting services (Matrix, Ollama, LiteLLM, secrets, Plane, Gitea, \
         Prometheus, DGEM daemon, chord proxy), and current inference state \
         (hot/warm models, VRAM). Each service is probed with a 2s timeout; one \
         down service never fails the report. Results are cached for 60 seconds."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        })
    }

    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        // Serve from cache when fresh.
        if let Some(cached) = cached_report() {
            return Ok(pretty(&cached));
        }
        let report = collect_report().await;
        store_report(&report);
        Ok(pretty(&report))
    }
}

/// Pretty-print a JSON value, falling back to a compact representation.
fn pretty(v: &Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string())
}

/// Register the `system_version` tool.
pub fn register(registry: &mut ToolRegistry) {
    let _ = registry.register(Box::new(SystemVersion));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_registers() {
        let mut reg = ToolRegistry::new();
        register(&mut reg);
        assert!(reg.contains("system_version"));
    }

    #[test]
    fn tool_name_and_schema() {
        let tool = SystemVersion;
        assert_eq!(tool.name(), "system_version");
        let params = tool.parameters();
        assert_eq!(params["type"], "object");
        assert!(params.get("properties").is_some());
    }

    #[test]
    fn env_url_trims_and_filters_empty() {
        std::env::set_var("SV_TEST_URL", "  http://host:1/ ");
        assert_eq!(env_url("SV_TEST_URL").as_deref(), Some("http://host:1"));
        std::env::set_var("SV_TEST_URL", "   ");
        assert_eq!(env_url("SV_TEST_URL"), None);
        std::env::remove_var("SV_TEST_URL");
        assert_eq!(env_url("SV_TEST_URL"), None);
    }

    #[tokio::test]
    async fn unreachable_service_reports_status_not_error() {
        // Point at an unused localhost port — the probe must resolve to
        // "unreachable" and must NOT propagate an error.
        let client = probe_client();
        // A reserved/unused high port; connection refused, not a panic.
        let res = get_json(&client, "http://127.0.0.1:1/api/version").await;
        assert!(res.is_none());

        // The Ollama probe against a bad URL yields "unreachable", never panics.
        // The live runtime sets OLLAMA_URL (not OLLAMA_BASE_URL).
        std::env::remove_var("OLLAMA_BASE_URL");
        std::env::remove_var("OLLAMA_CPU_URL");
        std::env::set_var("OLLAMA_URL", "http://127.0.0.1:1");
        let v = probe_ollama(&client).await;
        assert_eq!(v["status"], "unreachable");
        std::env::remove_var("OLLAMA_URL");
    }

    #[test]
    fn ollama_url_fallback_order() {
        for k in ["OLLAMA_URL", "OLLAMA_BASE_URL", "OLLAMA_CPU_URL"] {
            std::env::remove_var(k);
        }
        // Unset → not configured.
        assert_eq!(ollama_base_url(), None);

        // OLLAMA_CPU_URL alone is used as last resort.
        std::env::set_var("OLLAMA_CPU_URL", "http://cpu:11434");
        assert_eq!(ollama_base_url().as_deref(), Some("http://cpu:11434"));

        // OLLAMA_BASE_URL takes precedence over OLLAMA_CPU_URL.
        std::env::set_var("OLLAMA_BASE_URL", "http://base:11434");
        assert_eq!(ollama_base_url().as_deref(), Some("http://base:11434"));

        // OLLAMA_URL (the live runtime var) wins over everything.
        std::env::set_var("OLLAMA_URL", "http://live:11434");
        assert_eq!(ollama_base_url().as_deref(), Some("http://live:11434"));

        for k in ["OLLAMA_URL", "OLLAMA_BASE_URL", "OLLAMA_CPU_URL"] {
            std::env::remove_var(k);
        }
    }

    #[test]
    fn chord_defaults_to_localhost_when_unset() {
        std::env::remove_var("CHORD_PROXY_URL");
        std::env::remove_var("CHORD_CONTROL_URL");
        std::env::remove_var("CHORD_CONTROL_PORT");
        // Co-located defaults.
        assert_eq!(chord_proxy_base(), "http://127.0.0.1:8099");
        assert_eq!(chord_control_base(), "http://127.0.0.1:8090");

        // Explicit env still honoured.
        std::env::set_var("CHORD_PROXY_URL", "http://chord:9000");
        assert_eq!(chord_proxy_base(), "http://chord:9000");
        // Control derived from proxy host + control port.
        assert_eq!(chord_control_base(), "http://chord:8090");
        std::env::remove_var("CHORD_PROXY_URL");
    }

    #[tokio::test]
    async fn missing_env_reports_not_configured() {
        std::env::remove_var("GITEA_URL");
        let client = probe_client();
        let v = probe_gitea(&client).await;
        assert_eq!(v["status"], "not_configured");
    }

    #[test]
    fn cache_returns_within_ttl_window() {
        // Directly exercise the cache helpers (deterministic, no network).
        let sample = json!({"hello": "world"});
        store_report(&sample);
        let got = cached_report().expect("fresh entry should be cached");
        assert_eq!(got, sample);
    }

    #[tokio::test]
    async fn execute_never_errors_even_with_no_services() {
        // Clear every service URL so all probes go to "not_configured" /
        // "unreachable". execute() must still return Ok with valid JSON.
        for k in [
            "MATRIX_HOMESERVER", "OLLAMA_URL", "OLLAMA_BASE_URL", "OLLAMA_CPU_URL",
            "LITELLM_URL", "INFISICAL_URL",
            "PLANE_API_URL", "GITEA_URL", "PROMETHEUS_URL", "DGEM_BASE_URL",
            "CHORD_PROXY_URL", "CHORD_CONTROL_URL", "LUMINA_HTTP_URL",
        ] {
            std::env::remove_var(k);
        }
        // Bust any cache a prior test may have populated.
        if let Ok(mut g) = cache().lock() {
            *g = None;
        }
        let tool = SystemVersion;
        let out = tool.execute(json!({})).await.expect("must not error");
        let parsed: Value = serde_json::from_str(&out).expect("valid JSON");
        assert!(parsed.get("constellation").is_some());
        assert!(parsed.get("services").is_some());
        assert!(parsed.get("inference").is_some());
        // terminus_rs version is always present (compiled in).
        assert_eq!(
            parsed["constellation"]["terminus_rs"]["version"],
            crate::VERSION
        );
    }
}
