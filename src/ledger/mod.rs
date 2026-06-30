//! Ledger tools — finance tracking via Actual Budget HTTP API.
//!
//! All 7 tools use reqwest. Zero shell commands.
//!
//! Required env vars:
//!   ACTUAL_SERVER_URL   — base URL, e.g. http://192.168.0.x:5006
//!   ACTUAL_HTTP_API_KEY — API key for Actual Budget
//!   ACTUAL_BUDGET_ID    — budget sync ID (UUID)

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::tool::RustTool;

// ──────────────────────────────────────────────
// Shared client config
// ──────────────────────────────────────────────

#[derive(Clone)]
struct LedgerConfig {
    base_url: String,
    api_key: String,
    budget_id: String,
}

impl LedgerConfig {
    fn from_env() -> Result<Self, ToolError> {
        let base_url = std::env::var("ACTUAL_SERVER_URL").map_err(|_| {
            ToolError::NotConfigured("ACTUAL_SERVER_URL not set".into())
        })?;
        let api_key = std::env::var("ACTUAL_HTTP_API_KEY").map_err(|_| {
            ToolError::NotConfigured("ACTUAL_HTTP_API_KEY not set".into())
        })?;
        let budget_id = std::env::var("ACTUAL_BUDGET_ID").map_err(|_| {
            ToolError::NotConfigured("ACTUAL_BUDGET_ID not set".into())
        })?;
        Ok(Self { base_url, api_key, budget_id })
    }

    fn client(&self) -> Result<reqwest::Client, ToolError> {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|e| ToolError::Http(e.to_string()))
    }

    fn auth_header(&self) -> (&'static str, String) {
        ("x-api-key", self.api_key.clone())
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

/// Validate a YYYY-MM month string.
fn validate_month(s: &str) -> Result<(), ToolError> {
    let parts: Vec<&str> = s.split('-').collect();
    let ok = parts.len() == 2
        && parts[0].len() == 4
        && parts[1].len() == 2
        && parts.iter().all(|p| p.chars().all(|c| c.is_ascii_digit()));
    if !ok {
        return Err(ToolError::InvalidArgument(
            "Please use YYYY-MM format for month".into(),
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

fn parse_amount(v: &Value, field: &str) -> Result<f64, ToolError> {
    let n = v
        .as_f64()
        .ok_or_else(|| ToolError::InvalidArgument(format!("{field} must be a number")))?;
    if !n.is_finite() {
        return Err(ToolError::InvalidArgument(format!(
            "{field} must be a finite number"
        )));
    }
    Ok(n)
}

// ──────────────────────────────────────────────
// Tool: ledger_accounts
// ──────────────────────────────────────────────

pub struct LedgerAccounts;

#[async_trait]
impl RustTool for LedgerAccounts {
    fn name(&self) -> &str { "ledger_accounts" }

    fn description(&self) -> &str {
        "List all accounts in the Actual Budget."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }

    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        let cfg = LedgerConfig::from_env()?;
        let client = cfg.client()?;
        let (key, val) = cfg.auth_header();
        let url = format!("{}/v1/budgets/{}/accounts", cfg.base_url, cfg.budget_id);
        let resp = client
            .get(&url)
            .header(key, val)
            .send()
            .await
            .map_err(|e| ToolError::Http(format!("Actual Budget unreachable: {e}")))?;
        if !resp.status().is_success() {
            return Err(ToolError::Http(format!(
                "Actual Budget returned {}", resp.status()
            )));
        }
        let body: Value = resp.json().await.map_err(|e| ToolError::Http(e.to_string()))?;
        Ok(serde_json::to_string_pretty(&body)
            .unwrap_or_else(|_| "[]".to_string()))
    }
}

// ──────────────────────────────────────────────
// Tool: ledger_transactions
// ──────────────────────────────────────────────

pub struct LedgerTransactions;

#[async_trait]
impl RustTool for LedgerTransactions {
    fn name(&self) -> &str { "ledger_transactions" }

    fn description(&self) -> &str {
        "Get transactions for an account within a date range."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "account_id": { "type": "string", "description": "Account ID" },
                "start_date": { "type": "string", "description": "Start date (YYYY-MM-DD)" },
                "end_date":   { "type": "string", "description": "End date (YYYY-MM-DD)" }
            },
            "required": ["account_id", "start_date", "end_date"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let account_id = sanitize_string(
            args["account_id"].as_str().ok_or_else(|| {
                ToolError::InvalidArgument("account_id is required".into())
            })?,
        )?;
        let start_date = args["start_date"].as_str().ok_or_else(|| {
            ToolError::InvalidArgument("start_date is required".into())
        })?;
        let end_date = args["end_date"].as_str().ok_or_else(|| {
            ToolError::InvalidArgument("end_date is required".into())
        })?;
        validate_date(start_date)?;
        validate_date(end_date)?;

        let cfg = LedgerConfig::from_env()?;
        let client = cfg.client()?;
        let (key, val) = cfg.auth_header();
        let url = format!(
            "{}/v1/budgets/{}/accounts/{}/transactions",
            cfg.base_url, cfg.budget_id, account_id
        );
        let resp = client
            .get(&url)
            .header(key, val)
            .query(&[("since_date", start_date), ("before_date", end_date)])
            .send()
            .await
            .map_err(|e| ToolError::Http(format!("Actual Budget unreachable: {e}")))?;
        if !resp.status().is_success() {
            return Err(ToolError::Http(format!(
                "Actual Budget returned {}", resp.status()
            )));
        }
        let body: Value = resp.json().await.map_err(|e| ToolError::Http(e.to_string()))?;
        Ok(serde_json::to_string_pretty(&body)
            .unwrap_or_else(|_| "[]".to_string()))
    }
}

// ──────────────────────────────────────────────
// Tool: ledger_add_transaction
// ──────────────────────────────────────────────

pub struct LedgerAddTransaction;

#[async_trait]
impl RustTool for LedgerAddTransaction {
    fn name(&self) -> &str { "ledger_add_transaction" }

    fn description(&self) -> &str {
        "Add a transaction to an account. Amount is in dollars (negative = expense)."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "account_id": { "type": "string", "description": "Account ID" },
                "amount":     { "type": "number", "description": "Amount in dollars (negative for expense)" },
                "payee":      { "type": "string", "description": "Payee name (max 500 chars)" },
                "notes":      { "type": "string", "description": "Optional notes (max 500 chars)" },
                "date":       { "type": "string", "description": "Transaction date (YYYY-MM-DD)" }
            },
            "required": ["account_id", "amount", "payee", "date"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let account_id = sanitize_string(
            args["account_id"].as_str().ok_or_else(|| {
                ToolError::InvalidArgument("account_id is required".into())
            })?,
        )?;
        let amount = parse_amount(&args["amount"], "amount")?;
        let payee = sanitize_string(
            args["payee"].as_str().ok_or_else(|| {
                ToolError::InvalidArgument("payee is required".into())
            })?,
        )?;
        let notes = if args["notes"].is_string() {
            sanitize_string(args["notes"].as_str().unwrap())?
        } else {
            String::new()
        };
        let date = args["date"].as_str().ok_or_else(|| {
            ToolError::InvalidArgument("date is required".into())
        })?;
        validate_date(date)?;

        // Actual Budget stores amounts in milliunits (cents * 10)
        let milliunits = (amount * 100.0).round() as i64 * 10;

        let cfg = LedgerConfig::from_env()?;
        let client = cfg.client()?;
        let (key, val) = cfg.auth_header();
        let url = format!("{}/v1/budgets/{}/transactions", cfg.base_url, cfg.budget_id);
        let payload = json!({
            "transactions": [{
                "account_id":   account_id,
                "date":         date,
                "amount":       milliunits,
                "payee_name":   payee,
                "notes":        notes,
                "cleared":      true
            }]
        });
        let resp = client
            .post(&url)
            .header(key, val)
            .json(&payload)
            .send()
            .await
            .map_err(|e| ToolError::Http(format!("Actual Budget unreachable: {e}")))?;
        if !resp.status().is_success() {
            return Err(ToolError::Http(format!(
                "Actual Budget returned {}", resp.status()
            )));
        }
        Ok(format!(
            "Transaction added: {payee} ${amount:.2} on {date}"
        ))
    }
}

// ──────────────────────────────────────────────
// Tool: ledger_budget_summary
// ──────────────────────────────────────────────

pub struct LedgerBudgetSummary;

#[async_trait]
impl RustTool for LedgerBudgetSummary {
    fn name(&self) -> &str { "ledger_budget_summary" }

    fn description(&self) -> &str {
        "Get budget summary for a given month (YYYY-MM)."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "month": { "type": "string", "description": "Month in YYYY-MM format" }
            },
            "required": ["month"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let month = args["month"].as_str().ok_or_else(|| {
            ToolError::InvalidArgument("month is required".into())
        })?;
        validate_month(month)?;

        let cfg = LedgerConfig::from_env()?;
        let client = cfg.client()?;
        let (key, val) = cfg.auth_header();
        let url = format!(
            "{}/v1/budgets/{}/months/{}-01",
            cfg.base_url, cfg.budget_id, month
        );
        let resp = client
            .get(&url)
            .header(key, val)
            .send()
            .await
            .map_err(|e| ToolError::Http(format!("Actual Budget unreachable: {e}")))?;
        if !resp.status().is_success() {
            return Err(ToolError::Http(format!(
                "Actual Budget returned {}", resp.status()
            )));
        }
        let body: Value = resp.json().await.map_err(|e| ToolError::Http(e.to_string()))?;
        Ok(serde_json::to_string_pretty(&body)
            .unwrap_or_else(|_| "No budget data found".to_string()))
    }
}

// ──────────────────────────────────────────────
// Tool: ledger_category_spend
// ──────────────────────────────────────────────

pub struct LedgerCategorySpend;

#[async_trait]
impl RustTool for LedgerCategorySpend {
    fn name(&self) -> &str { "ledger_category_spend" }

    fn description(&self) -> &str {
        "Get spending for a specific category in a given month. Aggregates from budget month data."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "category": { "type": "string", "description": "Category name to look up (max 500 chars)" },
                "month":    { "type": "string", "description": "Month in YYYY-MM format" }
            },
            "required": ["category", "month"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let category = sanitize_string(
            args["category"].as_str().ok_or_else(|| {
                ToolError::InvalidArgument("category is required".into())
            })?,
        )?;
        let month = args["month"].as_str().ok_or_else(|| {
            ToolError::InvalidArgument("month is required".into())
        })?;
        validate_month(month)?;

        let cfg = LedgerConfig::from_env()?;
        let client = cfg.client()?;
        let (key, val) = cfg.auth_header();
        // Fetch the full month budget, then filter client-side
        let url = format!(
            "{}/v1/budgets/{}/months/{}-01",
            cfg.base_url, cfg.budget_id, month
        );
        let resp = client
            .get(&url)
            .header(key, val)
            .send()
            .await
            .map_err(|e| ToolError::Http(format!("Actual Budget unreachable: {e}")))?;
        if !resp.status().is_success() {
            return Err(ToolError::Http(format!(
                "Actual Budget returned {}", resp.status()
            )));
        }
        let body: Value = resp.json().await.map_err(|e| ToolError::Http(e.to_string()))?;

        // Aggregate across category_groups → categories
        let category_lower = category.to_lowercase();
        let groups = body["data"]["category_groups"]
            .as_array()
            .cloned()
            .unwrap_or_default();

        let mut found = false;
        let mut total_spent: f64 = 0.0;
        let mut budgeted: f64 = 0.0;

        for group in &groups {
            let cats = group["categories"].as_array().cloned().unwrap_or_default();
            for cat in &cats {
                let name = cat["name"].as_str().unwrap_or("").to_lowercase();
                if name.contains(&category_lower) {
                    found = true;
                    // Actual stores in milliunits
                    let spent = cat["spent"].as_f64().unwrap_or(0.0) / 1000.0;
                    let bud   = cat["budgeted"].as_f64().unwrap_or(0.0) / 1000.0;
                    total_spent += spent;
                    budgeted    += bud;
                }
            }
        }

        if !found {
            return Err(ToolError::NotFound(format!(
                "Category '{category}' not found in {month}"
            )));
        }

        Ok(format!(
            "Category '{}' in {}: spent ${:.2} of ${:.2} budgeted",
            category, month, total_spent.abs(), budgeted
        ))
    }
}

// ──────────────────────────────────────────────
// Tool: ledger_balance
// ──────────────────────────────────────────────

pub struct LedgerBalance;

#[async_trait]
impl RustTool for LedgerBalance {
    fn name(&self) -> &str { "ledger_balance" }

    fn description(&self) -> &str {
        "Get the current balance of an account."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "account_id": { "type": "string", "description": "Account ID" }
            },
            "required": ["account_id"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let account_id = sanitize_string(
            args["account_id"].as_str().ok_or_else(|| {
                ToolError::InvalidArgument("account_id is required".into())
            })?,
        )?;
        let cfg = LedgerConfig::from_env()?;
        let client = cfg.client()?;
        let (key, val) = cfg.auth_header();
        let url = format!(
            "{}/v1/budgets/{}/accounts/{}",
            cfg.base_url, cfg.budget_id, account_id
        );
        let resp = client
            .get(&url)
            .header(key, val)
            .send()
            .await
            .map_err(|e| ToolError::Http(format!("Actual Budget unreachable: {e}")))?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(ToolError::NotFound(format!(
                "Account '{account_id}' not found"
            )));
        }
        if !resp.status().is_success() {
            return Err(ToolError::Http(format!(
                "Actual Budget returned {}", resp.status()
            )));
        }
        let body: Value = resp.json().await.map_err(|e| ToolError::Http(e.to_string()))?;
        let balance_milliunits = body["data"]["balance"]
            .as_f64()
            .unwrap_or(0.0);
        let balance = balance_milliunits / 1000.0;
        let name = body["data"]["name"].as_str().unwrap_or("Account");
        Ok(format!("{name}: ${balance:.2}"))
    }
}

// ──────────────────────────────────────────────
// Tool: ledger_recent
// ──────────────────────────────────────────────

pub struct LedgerRecent;

#[async_trait]
impl RustTool for LedgerRecent {
    fn name(&self) -> &str { "ledger_recent" }

    fn description(&self) -> &str {
        "Get recent transactions across all accounts, newest first."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "limit": {
                    "type": "integer",
                    "description": "Max transactions to return (default 20, max 100)",
                    "minimum": 1,
                    "maximum": 100
                }
            },
            "required": []
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let limit = args["limit"].as_u64().unwrap_or(20).min(100) as usize;

        let cfg = LedgerConfig::from_env()?;
        let client = cfg.client()?;
        let (key, val) = cfg.auth_header();

        // Fetch all accounts first
        let accounts_url = format!("{}/v1/budgets/{}/accounts", cfg.base_url, cfg.budget_id);
        let accounts_resp = client
            .get(&accounts_url)
            .header(key.clone(), val.clone())
            .send()
            .await
            .map_err(|e| ToolError::Http(format!("Actual Budget unreachable: {e}")))?;
        if !accounts_resp.status().is_success() {
            return Err(ToolError::Http(format!(
                "Actual Budget returned {} fetching accounts", accounts_resp.status()
            )));
        }
        let accounts_body: Value = accounts_resp
            .json()
            .await
            .map_err(|e| ToolError::Http(e.to_string()))?;
        let accounts = accounts_body["data"]
            .as_array()
            .cloned()
            .unwrap_or_default();

        // Collect transactions across all accounts (fetch first account's recent, limited)
        // Use the budget-level transactions endpoint to get all
        let txn_url = format!("{}/v1/budgets/{}/transactions", cfg.base_url, cfg.budget_id);
        let txn_resp = client
            .get(&txn_url)
            .header(key, val)
            .send()
            .await
            .map_err(|e| ToolError::Http(format!("Actual Budget unreachable: {e}")))?;
        if !txn_resp.status().is_success() {
            // Fallback: not all Actual versions support budget-level transactions
            return Ok(format!(
                "Found {} accounts. Use ledger_transactions with a specific account_id and date range.",
                accounts.len()
            ));
        }
        let txn_body: Value = txn_resp.json().await.map_err(|e| ToolError::Http(e.to_string()))?;
        let mut txns = txn_body["data"].as_array().cloned().unwrap_or_default();
        // Sort by date descending (string comparison works for YYYY-MM-DD)
        txns.sort_by(|a, b| {
            let da = a["date"].as_str().unwrap_or("");
            let db = b["date"].as_str().unwrap_or("");
            db.cmp(da)
        });
        txns.truncate(limit);

        Ok(serde_json::to_string_pretty(&txns)
            .unwrap_or_else(|_| "[]".to_string()))
    }
}

// ──────────────────────────────────────────────
// Register all Ledger tools
// ──────────────────────────────────────────────

pub fn register(registry: &mut ToolRegistry) {
    registry.register_or_replace(Box::new(LedgerAccounts));
    registry.register_or_replace(Box::new(LedgerTransactions));
    registry.register_or_replace(Box::new(LedgerAddTransaction));
    registry.register_or_replace(Box::new(LedgerBudgetSummary));
    registry.register_or_replace(Box::new(LedgerCategorySpend));
    registry.register_or_replace(Box::new(LedgerBalance));
    registry.register_or_replace(Box::new(LedgerRecent));
}

// ──────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use httpmock::prelude::*;
    use std::sync::Mutex;

    /// Serialise all tests that touch ACTUAL_* env vars.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn set_env(url: &str) {
        std::env::set_var("ACTUAL_SERVER_URL", url);
        std::env::set_var("ACTUAL_HTTP_API_KEY", "test-key");
        std::env::set_var("ACTUAL_BUDGET_ID", "budget-123");
    }

    fn clear_env() {
        std::env::remove_var("ACTUAL_SERVER_URL");
        std::env::remove_var("ACTUAL_HTTP_API_KEY");
        std::env::remove_var("ACTUAL_BUDGET_ID");
    }

    // ── date / month validation ──

    #[test]
    fn test_valid_date_accepted() {
        assert!(validate_date("2026-06-07").is_ok());
    }

    #[test]
    fn test_invalid_date_rejected() {
        assert!(validate_date("2026/06/07").is_err());
        assert!(validate_date("06-07-2026").is_err());
        assert!(validate_date("not-a-date").is_err());
    }

    #[test]
    fn test_valid_month_accepted() {
        assert!(validate_month("2026-06").is_ok());
    }

    #[test]
    fn test_invalid_month_rejected() {
        assert!(validate_month("2026/06").is_err());
        assert!(validate_month("06-2026").is_err());
        assert!(validate_month("2026-6").is_err());
    }

    // ── NotConfigured when env not set ──

    #[tokio::test]
    #[serial]
    async fn test_not_configured_when_url_missing() {
        let _lock = ENV_LOCK.lock().unwrap();
        clear_env();
        let tool = LedgerAccounts;
        let err = tool.execute(json!({})).await.unwrap_err();
        assert!(matches!(err, ToolError::NotConfigured(_)));
    }

    #[tokio::test]
    #[serial]
    async fn test_not_configured_when_api_key_missing() {
        let _lock = ENV_LOCK.lock().unwrap();
        std::env::set_var("ACTUAL_SERVER_URL", "http://localhost");
        std::env::remove_var("ACTUAL_HTTP_API_KEY");
        std::env::remove_var("ACTUAL_BUDGET_ID");
        let tool = LedgerAccounts;
        let err = tool.execute(json!({})).await.unwrap_err();
        assert!(matches!(err, ToolError::NotConfigured(_)));
    }

    // ── ledger_accounts ──

    #[tokio::test]
    #[serial]
    async fn test_ledger_accounts_sends_correct_request() {
        let _lock = ENV_LOCK.lock().unwrap();
        let server = MockServer::start();
        set_env(&server.base_url());

        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/v1/budgets/budget-123/accounts")
                .header("x-api-key", "test-key");
            then.status(200).json_body(json!({"data": [{"id": "acc1", "name": "Checking"}]}));
        });

        let tool = LedgerAccounts;
        let result = tool.execute(json!({})).await.unwrap();
        assert!(result.contains("Checking"));
        mock.assert();
    }

    // ── ledger_transactions ──

    #[tokio::test]
    #[serial]
    async fn test_ledger_transactions_sends_correct_request() {
        let _lock = ENV_LOCK.lock().unwrap();
        let server = MockServer::start();
        set_env(&server.base_url());

        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/v1/budgets/budget-123/accounts/acc1/transactions");
            then.status(200).json_body(json!({"data": [{"id": "t1", "amount": -50000}]}));
        });

        let tool = LedgerTransactions;
        let result = tool.execute(json!({
            "account_id": "acc1",
            "start_date": "2026-06-01",
            "end_date":   "2026-06-30"
        })).await.unwrap();
        assert!(result.contains("t1"));
        mock.assert();
    }

    #[tokio::test]
    #[serial]
    async fn test_ledger_transactions_bad_date_rejected() {
        let _lock = ENV_LOCK.lock().unwrap();
        set_env("http://localhost:9");
        let tool = LedgerTransactions;
        let err = tool.execute(json!({
            "account_id": "acc1",
            "start_date": "06/01/2026",
            "end_date":   "2026-06-30"
        })).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    // ── ledger_add_transaction ──

    #[tokio::test]
    #[serial]
    async fn test_ledger_add_transaction_sends_correct_request() {
        let _lock = ENV_LOCK.lock().unwrap();
        let server = MockServer::start();
        set_env(&server.base_url());

        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/budgets/budget-123/transactions");
            then.status(200).json_body(json!({"data": {"duplicate_import_ids": [], "imported": 1}}));
        });

        let tool = LedgerAddTransaction;
        let result = tool.execute(json!({
            "account_id": "acc1",
            "amount":     -45.50,
            "payee":      "Grocery Store",
            "date":       "2026-06-07"
        })).await.unwrap();
        assert!(result.contains("Grocery Store"));
        assert!(result.contains("45.50"));
        mock.assert();
    }

    #[tokio::test]
    #[serial]
    async fn test_ledger_add_transaction_invalid_date_rejected() {
        let _lock = ENV_LOCK.lock().unwrap();
        set_env("http://localhost:9");
        let tool = LedgerAddTransaction;
        let err = tool.execute(json!({
            "account_id": "acc1",
            "amount":     -10.0,
            "payee":      "Test",
            "date":       "7th June 2026"
        })).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[tokio::test]
    #[serial]
    async fn test_ledger_add_transaction_nan_rejected() {
        let _lock = ENV_LOCK.lock().unwrap();
        set_env("http://localhost:9");
        let tool = LedgerAddTransaction;
        let err = tool.execute(json!({
            "account_id": "acc1",
            "amount":     null,
            "payee":      "Test",
            "date":       "2026-06-07"
        })).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    // ── ledger_budget_summary ──

    #[tokio::test]
    #[serial]
    async fn test_ledger_budget_summary_sends_correct_request() {
        let _lock = ENV_LOCK.lock().unwrap();
        let server = MockServer::start();
        set_env(&server.base_url());

        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/v1/budgets/budget-123/months/2026-06-01");
            then.status(200).json_body(json!({"data": {"month": "2026-06", "to_be_budgeted": 1000}}));
        });

        let tool = LedgerBudgetSummary;
        let result = tool.execute(json!({"month": "2026-06"})).await.unwrap();
        assert!(result.contains("2026-06"));
        mock.assert();
    }

    #[tokio::test]
    #[serial]
    async fn test_ledger_budget_summary_invalid_month_rejected() {
        let _lock = ENV_LOCK.lock().unwrap();
        set_env("http://localhost:9");
        let tool = LedgerBudgetSummary;
        let err = tool.execute(json!({"month": "June 2026"})).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    // ── ledger_balance ──

    #[tokio::test]
    #[serial]
    async fn test_ledger_balance_sends_correct_request() {
        let _lock = ENV_LOCK.lock().unwrap();
        let server = MockServer::start();
        set_env(&server.base_url());

        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/v1/budgets/budget-123/accounts/acc2");
            then.status(200).json_body(json!({"data": {"id": "acc2", "name": "Savings", "balance": 5000000}}));
        });

        let tool = LedgerBalance;
        let result = tool.execute(json!({"account_id": "acc2"})).await.unwrap();
        assert!(result.contains("Savings"));
        assert!(result.contains("5000.00"));
        mock.assert();
    }

    // ── ledger_recent ──

    #[tokio::test]
    #[serial]
    async fn test_ledger_recent_sends_correct_request() {
        let _lock = ENV_LOCK.lock().unwrap();
        let server = MockServer::start();
        set_env(&server.base_url());

        // accounts mock
        server.mock(|when, then| {
            when.method(GET).path("/v1/budgets/budget-123/accounts");
            then.status(200).json_body(json!({"data": [{"id": "a1"}]}));
        });

        // transactions mock
        let mock_txns = server.mock(|when, then| {
            when.method(GET).path("/v1/budgets/budget-123/transactions");
            then.status(200).json_body(json!({"data": [
                {"id": "t1", "date": "2026-06-07", "amount": -10000},
                {"id": "t2", "date": "2026-06-01", "amount": -5000}
            ]}));
        });

        let tool = LedgerRecent;
        let result = tool.execute(json!({"limit": 5})).await.unwrap();
        // t1 should come first (newer date)
        assert!(result.contains("t1"));
        mock_txns.assert();
    }

    // ── register ──

    #[test]
    fn test_register_adds_seven_tools() {
        let mut reg = ToolRegistry::new();
        register(&mut reg);
        assert_eq!(reg.len(), 7);
        assert!(reg.contains("ledger_accounts"));
        assert!(reg.contains("ledger_transactions"));
        assert!(reg.contains("ledger_add_transaction"));
        assert!(reg.contains("ledger_budget_summary"));
        assert!(reg.contains("ledger_category_spend"));
        assert!(reg.contains("ledger_balance"));
        assert!(reg.contains("ledger_recent"));
    }

    // ── string sanitization ──

    #[test]
    fn test_string_too_long_rejected() {
        let long = "x".repeat(501);
        assert!(sanitize_string(&long).is_err());
    }

    #[test]
    fn test_string_trimmed() {
        assert_eq!(sanitize_string("  hello  ").unwrap(), "hello");
    }
}
