//! Relay tools — vehicle/maintenance tracking via LubeLogger.
//!
//! All 7 tools use reqwest to call the LubeLogger REST API directly.
//! Zero shell commands. Typed request/response throughout.
//!
//! Required env vars:
//!   LUBELOGGER_URL      — base URL, e.g. http://192.168.0.x:8080
//!   LUBELOGGER_API_KEY  — Bearer token for auth

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::tool::RustTool;

// ──────────────────────────────────────────────
// Shared client config
// ──────────────────────────────────────────────

#[derive(Clone)]
struct RelayConfig {
    base_url: String,
    api_key: String,
}

impl RelayConfig {
    fn from_env() -> Result<Self, ToolError> {
        let base_url = std::env::var("LUBELOGGER_URL").map_err(|_| {
            ToolError::NotConfigured("LUBELOGGER_URL not set".into())
        })?;
        let api_key = std::env::var("LUBELOGGER_API_KEY").map_err(|_| {
            ToolError::NotConfigured("LUBELOGGER_API_KEY not set".into())
        })?;
        Ok(Self { base_url, api_key })
    }

    fn client(&self) -> Result<reqwest::Client, ToolError> {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|e| ToolError::Http(e.to_string()))
    }

    fn auth_header(&self) -> String {
        format!("Bearer {}", self.api_key)
    }
}

// ──────────────────────────────────────────────
// Input validation helpers
// ──────────────────────────────────────────────

/// Validate that a string is a YYYY-MM-DD date.
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

/// Trim + enforce max 500-char limit on string inputs.
fn sanitize_string(s: &str) -> Result<String, ToolError> {
    let trimmed = s.trim();
    if trimmed.len() > 500 {
        return Err(ToolError::InvalidArgument(
            "Field value exceeds 500 character limit".into(),
        ));
    }
    Ok(trimmed.to_string())
}

/// Parse f64 and reject NaN/infinite.
fn parse_positive_f64(v: &Value, field: &str) -> Result<f64, ToolError> {
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
// Tool: relay_vehicles
// ──────────────────────────────────────────────

pub struct RelayVehicles;

#[async_trait]
impl RustTool for RelayVehicles {
    fn name(&self) -> &str { "relay_vehicles" }

    fn description(&self) -> &str {
        "List all vehicles tracked in LubeLogger."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }

    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        let cfg = RelayConfig::from_env()?;
        let client = cfg.client()?;
        let url = format!("{}/api/vehicles", cfg.base_url);
        let resp = client
            .get(&url)
            .header("Authorization", cfg.auth_header())
            .send()
            .await
            .map_err(|e| ToolError::Http(format!("LubeLogger unreachable: {e}")))?;
        if !resp.status().is_success() {
            return Err(ToolError::Http(format!(
                "LubeLogger returned {}", resp.status()
            )));
        }
        let body: Value = resp.json().await.map_err(|e| ToolError::Http(e.to_string()))?;
        Ok(serde_json::to_string_pretty(&body)
            .unwrap_or_else(|_| "[]".to_string()))
    }
}

// ──────────────────────────────────────────────
// Tool: relay_fuel_log
// ──────────────────────────────────────────────

pub struct RelayFuelLog;

#[async_trait]
impl RustTool for RelayFuelLog {
    fn name(&self) -> &str { "relay_fuel_log" }

    fn description(&self) -> &str {
        "Add a fuel record for a vehicle. Requires vehicle_id, date (YYYY-MM-DD), gallons, miles, and price."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "vehicle_id": { "type": "string", "description": "Vehicle identifier" },
                "date":       { "type": "string", "description": "Date of fill-up (YYYY-MM-DD)" },
                "gallons":    { "type": "number", "description": "Gallons of fuel added" },
                "miles":      { "type": "number", "description": "Odometer reading at fill-up" },
                "price":      { "type": "number", "description": "Price per gallon" }
            },
            "required": ["vehicle_id", "date", "gallons", "miles", "price"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let vehicle_id = sanitize_string(
            args["vehicle_id"].as_str().ok_or_else(|| {
                ToolError::InvalidArgument("vehicle_id is required".into())
            })?,
        )?;
        let date = args["date"].as_str().ok_or_else(|| {
            ToolError::InvalidArgument("date is required".into())
        })?;
        validate_date(date)?;
        let gallons = parse_positive_f64(&args["gallons"], "gallons")?;
        let miles   = parse_positive_f64(&args["miles"],   "miles")?;
        let price   = parse_positive_f64(&args["price"],   "price")?;

        let cfg = RelayConfig::from_env()?;
        let client = cfg.client()?;
        let url = format!("{}/api/vehicles/{}/fuelrecords", cfg.base_url, vehicle_id);
        let payload = json!({
            "date":    date,
            "gallons": gallons,
            "odometer": miles,
            "cost":    price
        });
        let resp = client
            .post(&url)
            .header("Authorization", cfg.auth_header())
            .json(&payload)
            .send()
            .await
            .map_err(|e| ToolError::Http(format!("LubeLogger unreachable: {e}")))?;
        if !resp.status().is_success() {
            return Err(ToolError::Http(format!(
                "LubeLogger returned {}", resp.status()
            )));
        }
        Ok(format!("Fuel record added for vehicle {vehicle_id} on {date}: {gallons:.2} gal at ${price:.3}/gal, odometer {miles:.0}"))
    }
}

// ──────────────────────────────────────────────
// Tool: relay_service_log
// ──────────────────────────────────────────────

pub struct RelayServiceLog;

#[async_trait]
impl RustTool for RelayServiceLog {
    fn name(&self) -> &str { "relay_service_log" }

    fn description(&self) -> &str {
        "Add a service record for a vehicle. Requires vehicle_id, date, description, and mileage."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "vehicle_id":   { "type": "string", "description": "Vehicle identifier" },
                "date":         { "type": "string", "description": "Service date (YYYY-MM-DD)" },
                "description":  { "type": "string", "description": "Service description (max 500 chars)" },
                "mileage":      { "type": "number", "description": "Odometer reading at service" }
            },
            "required": ["vehicle_id", "date", "description", "mileage"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let vehicle_id = sanitize_string(
            args["vehicle_id"].as_str().ok_or_else(|| {
                ToolError::InvalidArgument("vehicle_id is required".into())
            })?,
        )?;
        let date = args["date"].as_str().ok_or_else(|| {
            ToolError::InvalidArgument("date is required".into())
        })?;
        validate_date(date)?;
        let description = sanitize_string(
            args["description"].as_str().ok_or_else(|| {
                ToolError::InvalidArgument("description is required".into())
            })?,
        )?;
        let mileage = parse_positive_f64(&args["mileage"], "mileage")?;

        let cfg = RelayConfig::from_env()?;
        let client = cfg.client()?;
        let url = format!("{}/api/vehicles/{}/servicerecords", cfg.base_url, vehicle_id);
        let payload = json!({
            "date":        date,
            "description": description,
            "odometer":    mileage
        });
        let resp = client
            .post(&url)
            .header("Authorization", cfg.auth_header())
            .json(&payload)
            .send()
            .await
            .map_err(|e| ToolError::Http(format!("LubeLogger unreachable: {e}")))?;
        if !resp.status().is_success() {
            return Err(ToolError::Http(format!(
                "LubeLogger returned {}", resp.status()
            )));
        }
        Ok(format!("Service record added for vehicle {vehicle_id} on {date}: {description}"))
    }
}

// ──────────────────────────────────────────────
// Tool: relay_next_due
// ──────────────────────────────────────────────

pub struct RelayNextDue;

#[async_trait]
impl RustTool for RelayNextDue {
    fn name(&self) -> &str { "relay_next_due" }

    fn description(&self) -> &str {
        "Get upcoming maintenance items for a vehicle."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "vehicle_id": { "type": "string", "description": "Vehicle identifier" }
            },
            "required": ["vehicle_id"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let vehicle_id = sanitize_string(
            args["vehicle_id"].as_str().ok_or_else(|| {
                ToolError::InvalidArgument("vehicle_id is required".into())
            })?,
        )?;
        let cfg = RelayConfig::from_env()?;
        let client = cfg.client()?;
        let url = format!("{}/api/vehicles/{}/upcoming", cfg.base_url, vehicle_id);
        let resp = client
            .get(&url)
            .header("Authorization", cfg.auth_header())
            .send()
            .await
            .map_err(|e| ToolError::Http(format!("LubeLogger unreachable: {e}")))?;
        if !resp.status().is_success() {
            return Err(ToolError::Http(format!(
                "LubeLogger returned {}", resp.status()
            )));
        }
        let body: Value = resp.json().await.map_err(|e| ToolError::Http(e.to_string()))?;
        Ok(serde_json::to_string_pretty(&body)
            .unwrap_or_else(|_| "No upcoming maintenance found".to_string()))
    }
}

// ──────────────────────────────────────────────
// Tool: relay_odometer
// ──────────────────────────────────────────────

pub struct RelayOdometer;

#[async_trait]
impl RustTool for RelayOdometer {
    fn name(&self) -> &str { "relay_odometer" }

    fn description(&self) -> &str {
        "Get current odometer reading for a vehicle."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "vehicle_id": { "type": "string", "description": "Vehicle identifier" }
            },
            "required": ["vehicle_id"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let vehicle_id = sanitize_string(
            args["vehicle_id"].as_str().ok_or_else(|| {
                ToolError::InvalidArgument("vehicle_id is required".into())
            })?,
        )?;
        let cfg = RelayConfig::from_env()?;
        let client = cfg.client()?;
        let url = format!("{}/api/vehicles/{}/odometer", cfg.base_url, vehicle_id);
        let resp = client
            .get(&url)
            .header("Authorization", cfg.auth_header())
            .send()
            .await
            .map_err(|e| ToolError::Http(format!("LubeLogger unreachable: {e}")))?;
        if !resp.status().is_success() {
            return Err(ToolError::Http(format!(
                "LubeLogger returned {}", resp.status()
            )));
        }
        let body: Value = resp.json().await.map_err(|e| ToolError::Http(e.to_string()))?;
        Ok(serde_json::to_string_pretty(&body)
            .unwrap_or_else(|_| "No odometer data found".to_string()))
    }
}

// ──────────────────────────────────────────────
// Tool: relay_cost_summary
// ──────────────────────────────────────────────

pub struct RelayCostSummary;

#[async_trait]
impl RustTool for RelayCostSummary {
    fn name(&self) -> &str { "relay_cost_summary" }

    fn description(&self) -> &str {
        "Get a cost summary (fuel, service, parts) for a vehicle."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "vehicle_id": { "type": "string", "description": "Vehicle identifier" }
            },
            "required": ["vehicle_id"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let vehicle_id = sanitize_string(
            args["vehicle_id"].as_str().ok_or_else(|| {
                ToolError::InvalidArgument("vehicle_id is required".into())
            })?,
        )?;
        let cfg = RelayConfig::from_env()?;
        let client = cfg.client()?;
        let url = format!("{}/api/vehicles/{}/costsummary", cfg.base_url, vehicle_id);
        let resp = client
            .get(&url)
            .header("Authorization", cfg.auth_header())
            .send()
            .await
            .map_err(|e| ToolError::Http(format!("LubeLogger unreachable: {e}")))?;
        if !resp.status().is_success() {
            return Err(ToolError::Http(format!(
                "LubeLogger returned {}", resp.status()
            )));
        }
        let body: Value = resp.json().await.map_err(|e| ToolError::Http(e.to_string()))?;
        Ok(serde_json::to_string_pretty(&body)
            .unwrap_or_else(|_| "No cost data found".to_string()))
    }
}

// ──────────────────────────────────────────────
// Tool: relay_maintenance_history
// ──────────────────────────────────────────────

pub struct RelayMaintenanceHistory;

#[async_trait]
impl RustTool for RelayMaintenanceHistory {
    fn name(&self) -> &str { "relay_maintenance_history" }

    fn description(&self) -> &str {
        "Get maintenance history for a vehicle, newest first. Optionally limit the number of records."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "vehicle_id": { "type": "string", "description": "Vehicle identifier" },
                "limit":      { "type": "integer", "description": "Max records to return (default 20)", "minimum": 1, "maximum": 200 }
            },
            "required": ["vehicle_id"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let vehicle_id = sanitize_string(
            args["vehicle_id"].as_str().ok_or_else(|| {
                ToolError::InvalidArgument("vehicle_id is required".into())
            })?,
        )?;
        let limit = args["limit"].as_u64().unwrap_or(20).min(200);

        let cfg = RelayConfig::from_env()?;
        let client = cfg.client()?;
        let url = format!("{}/api/vehicles/{}/servicerecords", cfg.base_url, vehicle_id);
        let resp = client
            .get(&url)
            .header("Authorization", cfg.auth_header())
            .send()
            .await
            .map_err(|e| ToolError::Http(format!("LubeLogger unreachable: {e}")))?;
        if !resp.status().is_success() {
            return Err(ToolError::Http(format!(
                "LubeLogger returned {}", resp.status()
            )));
        }
        let mut body: Value = resp.json().await.map_err(|e| ToolError::Http(e.to_string()))?;
        // Truncate to limit if it's an array
        if let Some(arr) = body.as_array_mut() {
            arr.truncate(limit as usize);
        }
        Ok(serde_json::to_string_pretty(&body)
            .unwrap_or_else(|_| "No service records found".to_string()))
    }
}

// ──────────────────────────────────────────────
// Register all Relay tools
// ──────────────────────────────────────────────

pub fn register(registry: &mut ToolRegistry) {
    registry.register_or_replace(Box::new(RelayVehicles));
    registry.register_or_replace(Box::new(RelayFuelLog));
    registry.register_or_replace(Box::new(RelayServiceLog));
    registry.register_or_replace(Box::new(RelayNextDue));
    registry.register_or_replace(Box::new(RelayOdometer));
    registry.register_or_replace(Box::new(RelayCostSummary));
    registry.register_or_replace(Box::new(RelayMaintenanceHistory));
}

// ──────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;
    use std::sync::Mutex;

    /// Serialise all tests that touch LUBELOGGER_* env vars.
    /// std::env is process-global; concurrent tokio tests would race otherwise.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn mock_cfg(server: &MockServer) -> (String, String) {
        (server.base_url(), "test-key".to_string())
    }

    fn set_env(url: &str, key: &str) {
        std::env::set_var("LUBELOGGER_URL", url);
        std::env::set_var("LUBELOGGER_API_KEY", key);
    }

    fn clear_env() {
        std::env::remove_var("LUBELOGGER_URL");
        std::env::remove_var("LUBELOGGER_API_KEY");
    }

    // ── date validation ──

    #[test]
    fn test_valid_date_accepted() {
        assert!(validate_date("2026-06-07").is_ok());
        assert!(validate_date("2000-01-01").is_ok());
    }

    #[test]
    fn test_invalid_date_rejected() {
        assert!(validate_date("2026/06/07").is_err());
        assert!(validate_date("26-6-7").is_err());
        assert!(validate_date("not-a-date").is_err());
        // Note: validate_date only checks YYYY-MM-DD structural format, not calendar validity.
        // Semantically invalid months/days (e.g. month 13) are accepted by the formatter
        // since calendar validation is left to the upstream API.
    }

    #[test]
    fn test_date_wrong_separator_rejected() {
        assert!(validate_date("20260607").is_err());
        assert!(validate_date("2026.06.07").is_err());
    }

    // ── string sanitization ──

    #[test]
    fn test_string_trim() {
        assert_eq!(sanitize_string("  hello  ").unwrap(), "hello");
    }

    #[test]
    fn test_string_too_long_rejected() {
        let long = "x".repeat(501);
        assert!(sanitize_string(&long).is_err());
    }

    #[test]
    fn test_string_exactly_500_ok() {
        let at_limit = "x".repeat(500);
        assert!(sanitize_string(&at_limit).is_ok());
    }

    // ── numeric validation ──

    #[test]
    fn test_negative_number_rejected() {
        let v = json!(-1.0f64);
        assert!(parse_positive_f64(&v, "gallons").is_err());
    }

    #[test]
    fn test_nan_rejected() {
        let v = json!(f64::NAN);
        // serde_json serializes NaN as null when using json! macro
        // so as_f64() returns None → InvalidArgument
        assert!(parse_positive_f64(&v, "gallons").is_err());
    }

    #[test]
    fn test_zero_accepted() {
        let v = json!(0.0f64);
        assert!(parse_positive_f64(&v, "gallons").is_ok());
    }

    // ── NotConfigured when env not set ──

    #[tokio::test]
    async fn test_not_configured_when_url_missing() {
        let _lock = ENV_LOCK.lock().unwrap();
        clear_env();
        let tool = RelayVehicles;
        let err = tool.execute(json!({})).await.unwrap_err();
        assert!(matches!(err, ToolError::NotConfigured(_)));
    }

    // ── relay_vehicles HTTP ──

    #[tokio::test]
    async fn test_relay_vehicles_sends_correct_request() {
        let _lock = ENV_LOCK.lock().unwrap();
        let server = MockServer::start();
        let (url, key) = mock_cfg(&server);
        set_env(&url, &key);

        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/api/vehicles")
                .header("authorization", format!("Bearer {key}"));
            then.status(200).json_body(json!([{"id": "1", "name": "Truck"}]));
        });

        let tool = RelayVehicles;
        let result = tool.execute(json!({})).await.unwrap();
        assert!(result.contains("Truck"));
        mock.assert();
    }

    // ── relay_fuel_log HTTP ──

    #[tokio::test]
    async fn test_relay_fuel_log_sends_correct_request() {
        let _lock = ENV_LOCK.lock().unwrap();
        let server = MockServer::start();
        let (url, key) = mock_cfg(&server);
        set_env(&url, &key);

        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/api/vehicles/42/fuelrecords");
            then.status(200).json_body(json!({"id": "r1"}));
        });

        let tool = RelayFuelLog;
        let result = tool.execute(json!({
            "vehicle_id": "42",
            "date":       "2026-06-07",
            "gallons":    12.5,
            "miles":      45000.0,
            "price":      3.499
        })).await.unwrap();
        assert!(result.contains("42"));
        mock.assert();
    }

    #[tokio::test]
    async fn test_relay_fuel_log_bad_date_rejected() {
        let _lock = ENV_LOCK.lock().unwrap();
        // Validation happens before any HTTP call; set a dummy URL to get past NotConfigured.
        set_env("http://localhost:9", "x");
        let tool = RelayFuelLog;
        let err = tool.execute(json!({
            "vehicle_id": "1",
            "date":       "06/07/2026",
            "gallons":    10.0,
            "miles":      1000.0,
            "price":      3.5
        })).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    // ── relay_service_log HTTP ──

    #[tokio::test]
    async fn test_relay_service_log_sends_correct_request() {
        let _lock = ENV_LOCK.lock().unwrap();
        let server = MockServer::start();
        let (url, key) = mock_cfg(&server);
        set_env(&url, &key);

        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/api/vehicles/7/servicerecords");
            then.status(200).json_body(json!({"id": "s1"}));
        });

        let tool = RelayServiceLog;
        let result = tool.execute(json!({
            "vehicle_id":  "7",
            "date":        "2026-05-01",
            "description": "Oil change",
            "mileage":     50000.0
        })).await.unwrap();
        assert!(result.contains("Oil change"));
        mock.assert();
    }

    // ── relay_next_due HTTP ──

    #[tokio::test]
    async fn test_relay_next_due_sends_correct_request() {
        let _lock = ENV_LOCK.lock().unwrap();
        let server = MockServer::start();
        let (url, key) = mock_cfg(&server);
        set_env(&url, &key);

        let mock = server.mock(|when, then| {
            when.method(GET).path("/api/vehicles/3/upcoming");
            then.status(200).json_body(json!([{"task": "Tire rotation"}]));
        });

        let tool = RelayNextDue;
        let result = tool.execute(json!({"vehicle_id": "3"})).await.unwrap();
        assert!(result.contains("Tire rotation"));
        mock.assert();
    }

    // ── relay_odometer HTTP ──

    #[tokio::test]
    async fn test_relay_odometer_sends_correct_request() {
        let _lock = ENV_LOCK.lock().unwrap();
        let server = MockServer::start();
        let (url, key) = mock_cfg(&server);
        set_env(&url, &key);

        let mock = server.mock(|when, then| {
            when.method(GET).path("/api/vehicles/5/odometer");
            then.status(200).json_body(json!({"mileage": 62000}));
        });

        let tool = RelayOdometer;
        let result = tool.execute(json!({"vehicle_id": "5"})).await.unwrap();
        assert!(result.contains("62000"));
        mock.assert();
    }

    // ── relay_cost_summary HTTP ──

    #[tokio::test]
    async fn test_relay_cost_summary_sends_correct_request() {
        let _lock = ENV_LOCK.lock().unwrap();
        let server = MockServer::start();
        let (url, key) = mock_cfg(&server);
        set_env(&url, &key);

        let mock = server.mock(|when, then| {
            when.method(GET).path("/api/vehicles/2/costsummary");
            then.status(200).json_body(json!({"total": 4200.0}));
        });

        let tool = RelayCostSummary;
        let result = tool.execute(json!({"vehicle_id": "2"})).await.unwrap();
        assert!(result.contains("4200"));
        mock.assert();
    }

    // ── relay_maintenance_history HTTP ──

    #[tokio::test]
    async fn test_relay_maintenance_history_sends_correct_request() {
        let _lock = ENV_LOCK.lock().unwrap();
        let server = MockServer::start();
        let (url, key) = mock_cfg(&server);
        set_env(&url, &key);

        let mock = server.mock(|when, then| {
            when.method(GET).path("/api/vehicles/9/servicerecords");
            then.status(200).json_body(json!([
                {"id": "1", "description": "Brake pads"},
                {"id": "2", "description": "Air filter"}
            ]));
        });

        let tool = RelayMaintenanceHistory;
        let result = tool.execute(json!({
            "vehicle_id": "9",
            "limit": 5
        })).await.unwrap();
        assert!(result.contains("Brake pads"));
        mock.assert();
    }

    #[tokio::test]
    async fn test_relay_maintenance_history_limit_caps_at_200() {
        let _lock = ENV_LOCK.lock().unwrap();
        let server = MockServer::start();
        let (url, key) = mock_cfg(&server);
        set_env(&url, &key);

        // Build a 250-item array
        let records: Vec<Value> = (0..250u32)
            .map(|i| json!({"id": i.to_string()}))
            .collect();

        server.mock(|when, then| {
            when.method(GET).path("/api/vehicles/10/servicerecords");
            then.status(200).json_body(json!(records));
        });

        let tool = RelayMaintenanceHistory;
        // Request more than cap
        let result = tool.execute(json!({
            "vehicle_id": "10",
            "limit": 9999
        })).await.unwrap();
        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed.as_array().unwrap().len(), 200);
    }

    // ── register ──

    #[test]
    fn test_register_adds_seven_tools() {
        let mut reg = ToolRegistry::new();
        register(&mut reg);
        assert_eq!(reg.len(), 7);
        assert!(reg.contains("relay_vehicles"));
        assert!(reg.contains("relay_fuel_log"));
        assert!(reg.contains("relay_service_log"));
        assert!(reg.contains("relay_next_due"));
        assert!(reg.contains("relay_odometer"));
        assert!(reg.contains("relay_cost_summary"));
        assert!(reg.contains("relay_maintenance_history"));
    }
}
