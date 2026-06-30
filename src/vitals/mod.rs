//! Vitals tools — health tracking via a REST API backend.
//!
//! All 6 tools use reqwest. Zero shell commands.
//!
//! Required env var:
//!   VITALS_API_URL  — base URL, e.g. http://192.168.0.x:8090

use async_trait::async_trait;
use serde_json::{json, Value};
use tracing::warn;

use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::tool::RustTool;

// ──────────────────────────────────────────────
// Shared client config
// ──────────────────────────────────────────────

#[derive(Clone)]
struct VitalsConfig {
    base_url: String,
}

impl VitalsConfig {
    fn from_env() -> Result<Self, ToolError> {
        let base_url = std::env::var("VITALS_API_URL").map_err(|_| {
            ToolError::NotConfigured("VITALS_API_URL not set".into())
        })?;
        Ok(Self { base_url })
    }

    fn client(&self) -> Result<reqwest::Client, ToolError> {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|e| ToolError::Http(e.to_string()))
    }
}

// ──────────────────────────────────────────────
// Input validation helpers
// ──────────────────────────────────────────────

fn validate_date(s: &str) -> Result<(), ToolError> {
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() != 3 {
        return Err(ToolError::InvalidArgument(
            "Please use YYYY-MM-DD format".into(),
        ));
    }
    let ok = parts[0].len() == 4
        && parts[1].len() == 2
        && parts[2].len() == 2
        && parts.iter().all(|p| p.chars().all(|c| c.is_ascii_digit()));
    if !ok {
        return Err(ToolError::InvalidArgument(
            "Please use YYYY-MM-DD format".into(),
        ));
    }
    Ok(())
}

fn sanitize_string(s: &str) -> Result<String, ToolError> {
    let trimmed = s.trim();
    if trimmed.len() > 500 {
        return Err(ToolError::InvalidArgument(
            "Field value exceeds 500 character limit".into(),
        ));
    }
    Ok(trimmed.to_string())
}

fn parse_positive_f64(v: &Value, field: &str) -> Result<f64, ToolError> {
    let n = v
        .as_f64()
        .ok_or_else(|| ToolError::InvalidArgument(format!("{field} must be a number")))?;
    if !n.is_finite() || n <= 0.0 {
        return Err(ToolError::InvalidArgument(format!(
            "{field} must be a positive finite number"
        )));
    }
    Ok(n)
}

fn parse_non_negative_f64(v: &Value, field: &str) -> Result<f64, ToolError> {
    let n = v
        .as_f64()
        .ok_or_else(|| ToolError::InvalidArgument(format!("{field} must be a number")))?;
    if !n.is_finite() || n < 0.0 {
        return Err(ToolError::InvalidArgument(format!(
            "{field} must be a non-negative finite number"
        )));
    }
    Ok(n)
}

// ──────────────────────────────────────────────
// Tool: vitals_log_weight
// ──────────────────────────────────────────────

pub struct VitalsLogWeight;

#[async_trait]
impl RustTool for VitalsLogWeight {
    fn name(&self) -> &str { "vitals_log_weight" }

    fn description(&self) -> &str {
        "Log a weight measurement in kilograms."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "date": { "type": "string", "description": "Date of measurement (YYYY-MM-DD)" },
                "kg":   { "type": "number", "description": "Weight in kilograms (must be positive)" }
            },
            "required": ["date", "kg"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let date = args["date"].as_str().ok_or_else(|| {
            ToolError::InvalidArgument("date is required".into())
        })?;
        validate_date(date)?;
        let kg = parse_positive_f64(&args["kg"], "kg")?;

        let cfg = VitalsConfig::from_env()?;
        let client = cfg.client()?;
        let url = format!("{}/api/weight", cfg.base_url);
        let payload = json!({ "date": date, "value": kg });
        let resp = client
            .post(&url)
            .json(&payload)
            .send()
            .await
            .map_err(|e| {
                warn!("vitals: request to {url} failed: {e}");
                ToolError::Http("The vitals service is unreachable.".into())
            })?;
        if !resp.status().is_success() {
            return Err(ToolError::Http(format!(
                "Vitals service returned {}", resp.status()
            )));
        }
        Ok(format!("Weight logged: {kg:.1} kg on {date}"))
    }
}

// ──────────────────────────────────────────────
// Tool: vitals_today
// ──────────────────────────────────────────────

pub struct VitalsToday;

#[async_trait]
impl RustTool for VitalsToday {
    fn name(&self) -> &str { "vitals_today" }

    fn description(&self) -> &str {
        "Get today's health metrics (weight, exercise, sleep if logged)."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }

    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        let cfg = VitalsConfig::from_env()?;
        let client = cfg.client()?;
        let url = format!("{}/api/today", cfg.base_url);
        let resp = client
            .get(&url)
            .send()
            .await
            .map_err(|e| {
                warn!("vitals: request to {url} failed: {e}");
                ToolError::Http("The vitals service is unreachable.".into())
            })?;
        if !resp.status().is_success() {
            return Err(ToolError::Http(format!(
                "Vitals service returned {}", resp.status()
            )));
        }
        let body: Value = resp.json().await.map_err(|e| ToolError::Http(e.to_string()))?;
        Ok(serde_json::to_string_pretty(&body)
            .unwrap_or_else(|_| "No data for today".to_string()))
    }
}

// ──────────────────────────────────────────────
// Tool: vitals_summary
// ──────────────────────────────────────────────

pub struct VitalsSummary;

#[async_trait]
impl RustTool for VitalsSummary {
    fn name(&self) -> &str { "vitals_summary" }

    fn description(&self) -> &str {
        "Get a health summary over the past N days."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "days": {
                    "type": "integer",
                    "description": "Number of days to summarise (default 7, max 365)",
                    "minimum": 1,
                    "maximum": 365
                }
            },
            "required": []
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let days = args["days"].as_u64().unwrap_or(7).min(365);

        let cfg = VitalsConfig::from_env()?;
        let client = cfg.client()?;
        let url = format!("{}/api/summary", cfg.base_url);
        let resp = client
            .get(&url)
            .query(&[("days", days.to_string())])
            .send()
            .await
            .map_err(|e| {
                warn!("vitals: request to {url} failed: {e}");
                ToolError::Http("The vitals service is unreachable.".into())
            })?;
        if !resp.status().is_success() {
            return Err(ToolError::Http(format!(
                "Vitals service returned {}", resp.status()
            )));
        }
        let body: Value = resp.json().await.map_err(|e| ToolError::Http(e.to_string()))?;
        Ok(serde_json::to_string_pretty(&body)
            .unwrap_or_else(|_| "No vitals data recorded yet".to_string()))
    }
}

// ──────────────────────────────────────────────
// Tool: vitals_log_exercise
// ──────────────────────────────────────────────

pub struct VitalsLogExercise;

#[async_trait]
impl RustTool for VitalsLogExercise {
    fn name(&self) -> &str { "vitals_log_exercise" }

    fn description(&self) -> &str {
        "Log an exercise session with type, duration, and optional calorie burn."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "date":         { "type": "string",  "description": "Date of exercise (YYYY-MM-DD)" },
                "type":         { "type": "string",  "description": "Exercise type, e.g. 'running', 'cycling' (max 500 chars)" },
                "duration_min": { "type": "number",  "description": "Duration in minutes (positive)" },
                "calories":     { "type": "number",  "description": "Estimated calories burned (optional, non-negative)" }
            },
            "required": ["date", "type", "duration_min"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let date = args["date"].as_str().ok_or_else(|| {
            ToolError::InvalidArgument("date is required".into())
        })?;
        validate_date(date)?;
        let exercise_type = sanitize_string(
            args["type"].as_str().ok_or_else(|| {
                ToolError::InvalidArgument("type is required".into())
            })?,
        )?;
        let duration_min = parse_positive_f64(&args["duration_min"], "duration_min")?;
        let calories = if args["calories"].is_number() {
            Some(parse_non_negative_f64(&args["calories"], "calories")?)
        } else {
            None
        };

        let cfg = VitalsConfig::from_env()?;
        let client = cfg.client()?;
        let url = format!("{}/api/exercise", cfg.base_url);
        let mut payload = json!({
            "date":         date,
            "type":         exercise_type,
            "duration_min": duration_min
        });
        if let Some(cal) = calories {
            payload["calories"] = json!(cal);
        }
        let resp = client
            .post(&url)
            .json(&payload)
            .send()
            .await
            .map_err(|e| {
                warn!("vitals: request to {url} failed: {e}");
                ToolError::Http("The vitals service is unreachable.".into())
            })?;
        if !resp.status().is_success() {
            return Err(ToolError::Http(format!(
                "Vitals service returned {}", resp.status()
            )));
        }
        let cal_str = calories
            .map(|c| format!(", {c:.0} cal"))
            .unwrap_or_default();
        Ok(format!(
            "Exercise logged: {exercise_type} for {duration_min:.0} min on {date}{cal_str}"
        ))
    }
}

// ──────────────────────────────────────────────
// Tool: vitals_log_sleep
// ──────────────────────────────────────────────

pub struct VitalsLogSleep;

#[async_trait]
impl RustTool for VitalsLogSleep {
    fn name(&self) -> &str { "vitals_log_sleep" }

    fn description(&self) -> &str {
        "Log a sleep session with duration and quality score."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "date":    { "type": "string",  "description": "Date of sleep (YYYY-MM-DD)" },
                "hours":   { "type": "number",  "description": "Hours slept (positive, max 24)" },
                "quality": { "type": "integer", "description": "Sleep quality 1–10 (optional)", "minimum": 1, "maximum": 10 }
            },
            "required": ["date", "hours"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let date = args["date"].as_str().ok_or_else(|| {
            ToolError::InvalidArgument("date is required".into())
        })?;
        validate_date(date)?;
        let hours = parse_positive_f64(&args["hours"], "hours")?;
        if hours > 24.0 {
            return Err(ToolError::InvalidArgument(
                "hours must not exceed 24".into(),
            ));
        }
        let quality = if args["quality"].is_number() {
            let q = args["quality"].as_u64().ok_or_else(|| {
                ToolError::InvalidArgument("quality must be an integer 1–10".into())
            })?;
            if !(1..=10).contains(&q) {
                return Err(ToolError::InvalidArgument(
                    "quality must be between 1 and 10".into(),
                ));
            }
            Some(q)
        } else {
            None
        };

        let cfg = VitalsConfig::from_env()?;
        let client = cfg.client()?;
        let url = format!("{}/api/sleep", cfg.base_url);
        let mut payload = json!({ "date": date, "hours": hours });
        if let Some(q) = quality {
            payload["quality"] = json!(q);
        }
        let resp = client
            .post(&url)
            .json(&payload)
            .send()
            .await
            .map_err(|e| {
                warn!("vitals: request to {url} failed: {e}");
                ToolError::Http("The vitals service is unreachable.".into())
            })?;
        if !resp.status().is_success() {
            return Err(ToolError::Http(format!(
                "Vitals service returned {}", resp.status()
            )));
        }
        let q_str = quality
            .map(|q| format!(", quality {q}/10"))
            .unwrap_or_default();
        Ok(format!("Sleep logged: {hours:.1} hours on {date}{q_str}"))
    }
}

// ──────────────────────────────────────────────
// Tool: vitals_trends
// ──────────────────────────────────────────────

pub struct VitalsTrends;

#[async_trait]
impl RustTool for VitalsTrends {
    fn name(&self) -> &str { "vitals_trends" }

    fn description(&self) -> &str {
        "Get trend data for a health metric over N days. Metric options: weight, exercise, sleep."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "metric": {
                    "type": "string",
                    "description": "Metric to trend: 'weight', 'exercise', or 'sleep'",
                    "enum": ["weight", "exercise", "sleep"]
                },
                "days": {
                    "type": "integer",
                    "description": "Number of days of history (default 30, max 365)",
                    "minimum": 1,
                    "maximum": 365
                }
            },
            "required": ["metric"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let metric = sanitize_string(
            args["metric"].as_str().ok_or_else(|| {
                ToolError::InvalidArgument("metric is required".into())
            })?,
        )?;
        // Validate allowlist
        if !matches!(metric.as_str(), "weight" | "exercise" | "sleep") {
            return Err(ToolError::InvalidArgument(
                "metric must be one of: weight, exercise, sleep".into(),
            ));
        }
        let days = args["days"].as_u64().unwrap_or(30).min(365);

        let cfg = VitalsConfig::from_env()?;
        let client = cfg.client()?;
        let url = format!("{}/api/trends", cfg.base_url);
        let resp = client
            .get(&url)
            .query(&[("metric", metric.as_str()), ("days", &days.to_string())])
            .send()
            .await
            .map_err(|e| {
                warn!("vitals: request to {url} failed: {e}");
                ToolError::Http("The vitals service is unreachable.".into())
            })?;
        if !resp.status().is_success() {
            return Err(ToolError::Http(format!(
                "Vitals service returned {}", resp.status()
            )));
        }
        let body: Value = resp.json().await.map_err(|e| ToolError::Http(e.to_string()))?;
        Ok(serde_json::to_string_pretty(&body)
            .unwrap_or_else(|_| format!("No {metric} trend data recorded yet")))
    }
}

// ──────────────────────────────────────────────
// Register all Vitals tools
// ──────────────────────────────────────────────

pub fn register(registry: &mut ToolRegistry) {
    registry.register_or_replace(Box::new(VitalsLogWeight));
    registry.register_or_replace(Box::new(VitalsToday));
    registry.register_or_replace(Box::new(VitalsSummary));
    registry.register_or_replace(Box::new(VitalsLogExercise));
    registry.register_or_replace(Box::new(VitalsLogSleep));
    registry.register_or_replace(Box::new(VitalsTrends));
}

// ──────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;
    use std::sync::Mutex;

    /// Serialise all tests that touch VITALS_API_URL env var.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn set_env(url: &str) {
        std::env::set_var("VITALS_API_URL", url);
    }

    fn clear_env() {
        std::env::remove_var("VITALS_API_URL");
    }

    // ── date validation ──

    #[test]
    fn test_valid_date_accepted() {
        assert!(validate_date("2026-06-07").is_ok());
    }

    #[test]
    fn test_invalid_date_rejected() {
        assert!(validate_date("2026/06/07").is_err());
        assert!(validate_date("not-a-date").is_err());
        assert!(validate_date("06-07-2026").is_err());
    }

    // ── numeric validation ──

    #[test]
    fn test_zero_rejected_for_positive() {
        let v = json!(0.0f64);
        assert!(parse_positive_f64(&v, "kg").is_err());
    }

    #[test]
    fn test_positive_accepted() {
        let v = json!(75.5f64);
        assert!(parse_positive_f64(&v, "kg").is_ok());
    }

    #[test]
    fn test_zero_accepted_for_non_negative() {
        let v = json!(0.0f64);
        assert!(parse_non_negative_f64(&v, "calories").is_ok());
    }

    #[test]
    fn test_negative_rejected_for_non_negative() {
        let v = json!(-1.0f64);
        assert!(parse_non_negative_f64(&v, "calories").is_err());
    }

    // ── string sanitization ──

    #[test]
    fn test_string_too_long_rejected() {
        let long = "x".repeat(501);
        assert!(sanitize_string(&long).is_err());
    }

    // ── NotConfigured when env not set ──

    #[tokio::test]
    async fn test_not_configured_when_url_missing() {
        let _lock = ENV_LOCK.lock().unwrap();
        clear_env();
        let tool = VitalsLogWeight;
        let err = tool.execute(json!({"date": "2026-06-07", "kg": 80.0})).await.unwrap_err();
        assert!(matches!(err, ToolError::NotConfigured(_)));
    }

    // ── vitals_log_weight ──

    #[tokio::test]
    async fn test_vitals_log_weight_sends_correct_request() {
        let _lock = ENV_LOCK.lock().unwrap();
        let server = MockServer::start();
        set_env(&server.base_url());

        let mock = server.mock(|when, then| {
            when.method(POST).path("/api/weight");
            then.status(201).json_body(json!({"id": "w1"}));
        });

        let tool = VitalsLogWeight;
        let result = tool.execute(json!({"date": "2026-06-07", "kg": 82.5})).await.unwrap();
        assert!(result.contains("82.5"));
        assert!(result.contains("2026-06-07"));
        mock.assert();
    }

    #[tokio::test]
    async fn test_vitals_log_weight_bad_date_rejected() {
        let _lock = ENV_LOCK.lock().unwrap();
        set_env("http://localhost:9");
        let tool = VitalsLogWeight;
        let err = tool.execute(json!({"date": "07/06/2026", "kg": 80.0})).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[tokio::test]
    async fn test_vitals_log_weight_zero_kg_rejected() {
        let _lock = ENV_LOCK.lock().unwrap();
        set_env("http://localhost:9");
        let tool = VitalsLogWeight;
        let err = tool.execute(json!({"date": "2026-06-07", "kg": 0.0})).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    // ── vitals_today ──

    #[tokio::test]
    async fn test_vitals_today_sends_correct_request() {
        let _lock = ENV_LOCK.lock().unwrap();
        let server = MockServer::start();
        set_env(&server.base_url());

        let mock = server.mock(|when, then| {
            when.method(GET).path("/api/today");
            then.status(200).json_body(json!({"weight": 82.5, "steps": 8000}));
        });

        let tool = VitalsToday;
        let result = tool.execute(json!({})).await.unwrap();
        assert!(result.contains("8000"));
        mock.assert();
    }

    // ── vitals_summary ──

    #[tokio::test]
    async fn test_vitals_summary_sends_correct_request() {
        let _lock = ENV_LOCK.lock().unwrap();
        let server = MockServer::start();
        set_env(&server.base_url());

        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/api/summary")
                .query_param("days", "14");
            then.status(200).json_body(json!({"avg_weight": 82.0, "days": 14}));
        });

        let tool = VitalsSummary;
        let result = tool.execute(json!({"days": 14})).await.unwrap();
        assert!(result.contains("82.0") || result.contains("82"));
        mock.assert();
    }

    #[tokio::test]
    async fn test_vitals_summary_caps_at_365() {
        let _lock = ENV_LOCK.lock().unwrap();
        let server = MockServer::start();
        set_env(&server.base_url());

        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/api/summary")
                .query_param("days", "365");
            then.status(200).json_body(json!({"days": 365}));
        });

        let tool = VitalsSummary;
        let _ = tool.execute(json!({"days": 9999})).await;
        mock.assert();
    }

    // ── vitals_log_exercise ──

    #[tokio::test]
    async fn test_vitals_log_exercise_sends_correct_request() {
        let _lock = ENV_LOCK.lock().unwrap();
        let server = MockServer::start();
        set_env(&server.base_url());

        let mock = server.mock(|when, then| {
            when.method(POST).path("/api/exercise");
            then.status(201).json_body(json!({"id": "e1"}));
        });

        let tool = VitalsLogExercise;
        let result = tool.execute(json!({
            "date":         "2026-06-07",
            "type":         "running",
            "duration_min": 30.0,
            "calories":     300.0
        })).await.unwrap();
        assert!(result.contains("running"));
        assert!(result.contains("30"));
        mock.assert();
    }

    #[tokio::test]
    async fn test_vitals_log_exercise_bad_date_rejected() {
        let _lock = ENV_LOCK.lock().unwrap();
        set_env("http://localhost:9");
        let tool = VitalsLogExercise;
        let err = tool.execute(json!({
            "date":         "7 June 2026",
            "type":         "walking",
            "duration_min": 20.0
        })).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[tokio::test]
    async fn test_vitals_log_exercise_negative_duration_rejected() {
        let _lock = ENV_LOCK.lock().unwrap();
        set_env("http://localhost:9");
        let tool = VitalsLogExercise;
        let err = tool.execute(json!({
            "date":         "2026-06-07",
            "type":         "cycling",
            "duration_min": -5.0
        })).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    // ── vitals_log_sleep ──

    #[tokio::test]
    async fn test_vitals_log_sleep_sends_correct_request() {
        let _lock = ENV_LOCK.lock().unwrap();
        let server = MockServer::start();
        set_env(&server.base_url());

        let mock = server.mock(|when, then| {
            when.method(POST).path("/api/sleep");
            then.status(201).json_body(json!({"id": "s1"}));
        });

        let tool = VitalsLogSleep;
        let result = tool.execute(json!({
            "date":    "2026-06-07",
            "hours":   7.5,
            "quality": 8
        })).await.unwrap();
        assert!(result.contains("7.5"));
        assert!(result.contains("8/10"));
        mock.assert();
    }

    #[tokio::test]
    async fn test_vitals_log_sleep_hours_over_24_rejected() {
        let _lock = ENV_LOCK.lock().unwrap();
        set_env("http://localhost:9");
        let tool = VitalsLogSleep;
        let err = tool.execute(json!({
            "date":  "2026-06-07",
            "hours": 25.0
        })).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[tokio::test]
    async fn test_vitals_log_sleep_quality_out_of_range_rejected() {
        let _lock = ENV_LOCK.lock().unwrap();
        set_env("http://localhost:9");
        let tool = VitalsLogSleep;
        let err = tool.execute(json!({
            "date":    "2026-06-07",
            "hours":   7.0,
            "quality": 11
        })).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    // ── vitals_trends ──

    #[tokio::test]
    async fn test_vitals_trends_sends_correct_request() {
        let _lock = ENV_LOCK.lock().unwrap();
        let server = MockServer::start();
        set_env(&server.base_url());

        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/api/trends")
                .query_param("metric", "weight")
                .query_param("days", "30");
            then.status(200).json_body(json!([{"date": "2026-06-01", "value": 82.0}]));
        });

        let tool = VitalsTrends;
        let result = tool.execute(json!({"metric": "weight", "days": 30})).await.unwrap();
        assert!(result.contains("82.0") || result.contains("82"));
        mock.assert();
    }

    #[tokio::test]
    async fn test_vitals_trends_invalid_metric_rejected() {
        let _lock = ENV_LOCK.lock().unwrap();
        set_env("http://localhost:9");
        let tool = VitalsTrends;
        let err = tool.execute(json!({"metric": "blood_pressure"})).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[tokio::test]
    async fn test_vitals_trends_days_capped_at_365() {
        let _lock = ENV_LOCK.lock().unwrap();
        let server = MockServer::start();
        set_env(&server.base_url());

        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/api/trends")
                .query_param("metric", "sleep")
                .query_param("days", "365");
            then.status(200).json_body(json!([]));
        });

        let tool = VitalsTrends;
        let _ = tool.execute(json!({"metric": "sleep", "days": 9999})).await;
        mock.assert();
    }

    // ── register ──

    #[test]
    fn test_register_adds_six_tools() {
        let mut reg = ToolRegistry::new();
        register(&mut reg);
        assert_eq!(reg.len(), 6);
        assert!(reg.contains("vitals_log_weight"));
        assert!(reg.contains("vitals_today"));
        assert!(reg.contains("vitals_summary"));
        assert!(reg.contains("vitals_log_exercise"));
        assert!(reg.contains("vitals_log_sleep"));
        assert!(reg.contains("vitals_trends"));
    }
}
