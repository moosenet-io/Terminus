//! Myelin cost-tracking tools — CHORD-10.
//!
//! Replaces the Python myelin_tools.py implementation which built SQL as strings
//! inside remote `python -c` shell commands (Grade D — worst SQL-via-shell offender).
//! This module uses sqlx with parameterized queries directly against Postgres.
//!
//! ## Configuration
//! Set `MYELIN_DATABASE_URL` to a valid Postgres connection string.
//! If the variable is absent all nine tools return `ToolError::NotConfigured`.
//!
//! ## Tools provided
//! | Tool name             | Description                                      |
//! |-----------------------|--------------------------------------------------|
//! | `myelin_status`       | Latest sync timestamp + total record count       |
//! | `myelin_today`        | Spend today (UTC)                                |
//! | `myelin_weekly`       | Daily spend for the last 7 days                  |
//! | `myelin_monthly`      | Total spend for the last 30 days                 |
//! | `myelin_runaway_check`| Requests exceeding a cost threshold in last hour |
//! | `myelin_burn_plan`    | Days remaining at current burn rate              |
//! | `myelin_by_model`     | Spend per model                                  |
//! | `myelin_by_user`      | Spend per user                                   |
//! | `myelin_cap_check`    | Compare today's spend to a daily cap             |

use std::env;

use async_trait::async_trait;
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};

use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::tool::RustTool;

// ─── helpers ─────────────────────────────────────────────────────────────────

/// Format a numeric cost value (f64) as a "$X.XX" dollar string.
pub fn fmt_cost(dollars: f64) -> String {
    format!("${:.2}", dollars)
}

/// Acquire a connection pool from `MYELIN_DATABASE_URL`.
/// Returns `ToolError::NotConfigured` if the variable is absent.
async fn pool() -> Result<PgPool, ToolError> {
    let url = env::var("MYELIN_DATABASE_URL").map_err(|_| {
        ToolError::NotConfigured(
            "MYELIN_DATABASE_URL is not set — Myelin cost tracking is not configured".into(),
        )
    })?;
    PgPoolOptions::new()
        .max_connections(3)
        .connect(&url)
        .await
        .map_err(|e| ToolError::Database(format!("Failed to connect to Myelin DB: {e}")))
}

// ─── MyelinStatus ─────────────────────────────────────────────────────────────

/// `myelin_status` — returns the latest sync timestamp and total record count.
pub struct MyelinStatus;

#[async_trait]
impl RustTool for MyelinStatus {
    fn name(&self) -> &str {
        "myelin_status"
    }

    fn description(&self) -> &str {
        "Show the latest Myelin sync timestamp and total usage record count."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }

    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        let pool = pool().await?;

        let row = sqlx::query(
            "SELECT COUNT(*) AS cnt, MAX(recorded_at) AS latest FROM myelin_usage",
        )
        .fetch_one(&pool)
        .await
        .map_err(|e| ToolError::Database(format!("myelin_status query failed: {e}")))?;

        let count: i64 = row.try_get("cnt").unwrap_or(0);
        let latest: Option<chrono::NaiveDateTime> = row.try_get("latest").ok().flatten();

        if count == 0 {
            return Ok("No usage data recorded".into());
        }

        let ts = latest
            .map(|dt| dt.format("%Y-%m-%d %H:%M:%S UTC").to_string())
            .unwrap_or_else(|| "unknown".into());

        Ok(format!(
            "Myelin status: {count} usage records. Latest sync: {ts}"
        ))
    }
}

// ─── MyelinToday ──────────────────────────────────────────────────────────────

/// `myelin_today` — total spend for today (UTC date).
pub struct MyelinToday;

#[async_trait]
impl RustTool for MyelinToday {
    fn name(&self) -> &str {
        "myelin_today"
    }

    fn description(&self) -> &str {
        "Show total AI cost spend for today (UTC)."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }

    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        let pool = pool().await?;

        let row = sqlx::query(
            "SELECT COALESCE(SUM(cost), 0.0) AS total \
             FROM myelin_usage \
             WHERE (recorded_at AT TIME ZONE 'UTC')::date = CURRENT_DATE",
        )
        .fetch_one(&pool)
        .await
        .map_err(|e| ToolError::Database(format!("myelin_today query failed: {e}")))?;

        let total: f64 = row.try_get("total").unwrap_or(0.0);

        if total == 0.0 {
            return Ok("No usage data recorded today".into());
        }

        Ok(format!("Today's spend: {}", fmt_cost(total)))
    }
}

// ─── MyelinWeekly ─────────────────────────────────────────────────────────────

/// `myelin_weekly` — daily spend breakdown for the last 7 days.
pub struct MyelinWeekly;

#[async_trait]
impl RustTool for MyelinWeekly {
    fn name(&self) -> &str {
        "myelin_weekly"
    }

    fn description(&self) -> &str {
        "Show daily AI cost spend for the last 7 days, grouped by date."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }

    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        let pool = pool().await?;

        let rows = sqlx::query(
            "SELECT recorded_at::date AS day, COALESCE(SUM(cost), 0.0) AS total \
             FROM myelin_usage \
             WHERE recorded_at >= NOW() - INTERVAL '7 days' \
             GROUP BY recorded_at::date \
             ORDER BY day DESC",
        )
        .fetch_all(&pool)
        .await
        .map_err(|e| ToolError::Database(format!("myelin_weekly query failed: {e}")))?;

        if rows.is_empty() {
            return Ok("No usage data recorded in the last 7 days".into());
        }

        let mut lines = vec!["Weekly spend (last 7 days):".to_string()];
        for row in &rows {
            let day: chrono::NaiveDate = row.try_get("day").unwrap_or_default();
            let total: f64 = row.try_get("total").unwrap_or(0.0);
            lines.push(format!("  {day}: {}", fmt_cost(total)));
        }

        Ok(lines.join("\n"))
    }
}

// ─── MyelinMonthly ────────────────────────────────────────────────────────────

/// `myelin_monthly` — total spend for the last 30 days.
pub struct MyelinMonthly;

#[async_trait]
impl RustTool for MyelinMonthly {
    fn name(&self) -> &str {
        "myelin_monthly"
    }

    fn description(&self) -> &str {
        "Show total AI cost spend for the last 30 days."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }

    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        let pool = pool().await?;

        let row = sqlx::query(
            "SELECT COALESCE(SUM(cost), 0.0) AS total \
             FROM myelin_usage \
             WHERE recorded_at >= NOW() - INTERVAL '30 days'",
        )
        .fetch_one(&pool)
        .await
        .map_err(|e| ToolError::Database(format!("myelin_monthly query failed: {e}")))?;

        let total: f64 = row.try_get("total").unwrap_or(0.0);

        if total == 0.0 {
            return Ok("No usage data recorded in the last 30 days".into());
        }

        Ok(format!("30-day spend: {}", fmt_cost(total)))
    }
}

// ─── MyelinRunawayCheck ───────────────────────────────────────────────────────

/// `myelin_runaway_check` — find requests exceeding a cost threshold in the last hour.
pub struct MyelinRunawayCheck;

#[async_trait]
impl RustTool for MyelinRunawayCheck {
    fn name(&self) -> &str {
        "myelin_runaway_check"
    }

    fn description(&self) -> &str {
        "Find individual requests that exceeded a cost threshold in the last hour. \
         Pass threshold as a dollar amount (e.g. 0.05 for $0.05)."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "threshold": {
                    "type": "number",
                    "description": "Minimum per-request cost to flag (in dollars, e.g. 0.05)"
                }
            },
            "required": ["threshold"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let threshold = args["threshold"]
            .as_f64()
            .ok_or_else(|| ToolError::InvalidArgument("threshold must be a number".into()))?;

        if threshold < 0.0 {
            return Err(ToolError::InvalidArgument(
                "threshold must be non-negative".into(),
            ));
        }

        let pool = pool().await?;

        // Parameterized — threshold bound as $1, not interpolated.
        let rows = sqlx::query(
            "SELECT model, user_id, cost, recorded_at \
             FROM myelin_usage \
             WHERE cost > $1 \
               AND recorded_at >= NOW() - INTERVAL '1 hour' \
             ORDER BY cost DESC",
        )
        .bind(threshold)
        .fetch_all(&pool)
        .await
        .map_err(|e| ToolError::Database(format!("myelin_runaway_check query failed: {e}")))?;

        if rows.is_empty() {
            return Ok(format!(
                "No runaway requests found (threshold: {})",
                fmt_cost(threshold)
            ));
        }

        let mut lines = vec![format!(
            "Runaway requests > {} in the last hour:",
            fmt_cost(threshold)
        )];
        for row in &rows {
            let model: String = row.try_get("model").unwrap_or_else(|_| "unknown".into());
            let user: String = row.try_get("user_id").unwrap_or_else(|_| "unknown".into());
            let cost: f64 = row.try_get("cost").unwrap_or(0.0);
            let ts: chrono::NaiveDateTime = row.try_get("recorded_at").unwrap_or_default();
            lines.push(format!(
                "  {} | user={} | {} | {}",
                model,
                user,
                fmt_cost(cost),
                ts.format("%H:%M:%S")
            ));
        }
        lines.push(format!("Total: {} requests", rows.len()));

        Ok(lines.join("\n"))
    }
}

// ─── MyelinBurnPlan ───────────────────────────────────────────────────────────

/// `myelin_burn_plan` — project days remaining at current burn rate.
pub struct MyelinBurnPlan;

#[async_trait]
impl RustTool for MyelinBurnPlan {
    fn name(&self) -> &str {
        "myelin_burn_plan"
    }

    fn description(&self) -> &str {
        "Project days remaining at the current AI spend burn rate given a total budget."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "budget": {
                    "type": "number",
                    "description": "Total remaining budget in dollars (e.g. 50.00)"
                }
            },
            "required": ["budget"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let budget = args["budget"]
            .as_f64()
            .ok_or_else(|| ToolError::InvalidArgument("budget must be a number".into()))?;

        if budget < 0.0 {
            return Err(ToolError::InvalidArgument(
                "budget must be non-negative".into(),
            ));
        }

        let pool = pool().await?;

        let row = sqlx::query(
            "SELECT COALESCE(SUM(cost), 0.0) / NULLIF(COUNT(DISTINCT recorded_at::date), 0) \
                    AS avg_daily \
             FROM myelin_usage \
             WHERE recorded_at >= NOW() - INTERVAL '7 days'",
        )
        .fetch_one(&pool)
        .await
        .map_err(|e| ToolError::Database(format!("myelin_burn_plan query failed: {e}")))?;

        let avg_daily: f64 = row.try_get("avg_daily").unwrap_or(0.0);

        if avg_daily <= 0.0 {
            return Ok("No usage data recorded — cannot project burn rate".into());
        }

        let days_remaining = budget / avg_daily;

        Ok(format!(
            "Burn plan: avg daily spend {} → budget {} lasts {:.1} days",
            fmt_cost(avg_daily),
            fmt_cost(budget),
            days_remaining
        ))
    }
}

// ─── MyelinByModel ────────────────────────────────────────────────────────────

/// `myelin_by_model` — spend per model.
pub struct MyelinByModel;

#[async_trait]
impl RustTool for MyelinByModel {
    fn name(&self) -> &str {
        "myelin_by_model"
    }

    fn description(&self) -> &str {
        "Show total AI cost spend grouped by model."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }

    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        let pool = pool().await?;

        let rows = sqlx::query(
            "SELECT model, COALESCE(SUM(cost), 0.0) AS total \
             FROM myelin_usage \
             GROUP BY model \
             ORDER BY total DESC",
        )
        .fetch_all(&pool)
        .await
        .map_err(|e| ToolError::Database(format!("myelin_by_model query failed: {e}")))?;

        if rows.is_empty() {
            return Ok("No usage data recorded".into());
        }

        let mut lines = vec!["Spend by model:".to_string()];
        for row in &rows {
            let model: String = row.try_get("model").unwrap_or_else(|_| "unknown".into());
            let total: f64 = row.try_get("total").unwrap_or(0.0);
            lines.push(format!("  {model}: {}", fmt_cost(total)));
        }

        Ok(lines.join("\n"))
    }
}

// ─── MyelinByUser ─────────────────────────────────────────────────────────────

/// `myelin_by_user` — spend per user.
pub struct MyelinByUser;

#[async_trait]
impl RustTool for MyelinByUser {
    fn name(&self) -> &str {
        "myelin_by_user"
    }

    fn description(&self) -> &str {
        "Show total AI cost spend grouped by user."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }

    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        let pool = pool().await?;

        let rows = sqlx::query(
            "SELECT user_id, COALESCE(SUM(cost), 0.0) AS total \
             FROM myelin_usage \
             GROUP BY user_id \
             ORDER BY total DESC",
        )
        .fetch_all(&pool)
        .await
        .map_err(|e| ToolError::Database(format!("myelin_by_user query failed: {e}")))?;

        if rows.is_empty() {
            return Ok("No usage data recorded".into());
        }

        let mut lines = vec!["Spend by user:".to_string()];
        for row in &rows {
            let user: String = row.try_get("user_id").unwrap_or_else(|_| "unknown".into());
            let total: f64 = row.try_get("total").unwrap_or(0.0);
            lines.push(format!("  {user}: {}", fmt_cost(total)));
        }

        Ok(lines.join("\n"))
    }
}

// ─── MyelinCapCheck ───────────────────────────────────────────────────────────

/// `myelin_cap_check` — compare today's spend to a daily cap.
pub struct MyelinCapCheck;

#[async_trait]
impl RustTool for MyelinCapCheck {
    fn name(&self) -> &str {
        "myelin_cap_check"
    }

    fn description(&self) -> &str {
        "Check whether today's AI spend has exceeded a daily cap. \
         Returns current spend, cap, and percentage used."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "daily_limit": {
                    "type": "number",
                    "description": "Daily spending cap in dollars (e.g. 5.00)"
                }
            },
            "required": ["daily_limit"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let daily_limit = args["daily_limit"]
            .as_f64()
            .ok_or_else(|| ToolError::InvalidArgument("daily_limit must be a number".into()))?;

        if daily_limit <= 0.0 {
            return Err(ToolError::InvalidArgument(
                "daily_limit must be positive".into(),
            ));
        }

        let pool = pool().await?;

        let row = sqlx::query(
            "SELECT COALESCE(SUM(cost), 0.0) AS today \
             FROM myelin_usage \
             WHERE (recorded_at AT TIME ZONE 'UTC')::date = CURRENT_DATE",
        )
        .fetch_one(&pool)
        .await
        .map_err(|e| ToolError::Database(format!("myelin_cap_check query failed: {e}")))?;

        let today: f64 = row.try_get("today").unwrap_or(0.0);
        let pct = (today / daily_limit) * 100.0;
        let status = if pct >= 100.0 {
            "EXCEEDED"
        } else if pct >= 80.0 {
            "WARNING"
        } else {
            "OK"
        };

        Ok(format!(
            "Cap check [{status}]: spent {} of {} ({:.1}% of daily limit)",
            fmt_cost(today),
            fmt_cost(daily_limit),
            pct
        ))
    }
}

// ─── Registration ─────────────────────────────────────────────────────────────

/// Register all nine Myelin tools into the given registry.
pub fn register(registry: &mut ToolRegistry) {
    for tool in tools() {
        registry.register_or_replace(tool);
    }
}

/// Return all nine Myelin tools as boxed trait objects.
pub fn tools() -> Vec<Box<dyn RustTool>> {
    vec![
        Box::new(MyelinStatus),
        Box::new(MyelinToday),
        Box::new(MyelinWeekly),
        Box::new(MyelinMonthly),
        Box::new(MyelinRunawayCheck),
        Box::new(MyelinBurnPlan),
        Box::new(MyelinByModel),
        Box::new(MyelinByUser),
        Box::new(MyelinCapCheck),
    ]
}

// ─── Unit tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::env;

    // ── fmt_cost formatting ──────────────────────────────────────────────────

    #[test]
    fn test_fmt_cost_zero() {
        assert_eq!(fmt_cost(0.0), "$0.00");
    }

    #[test]
    fn test_fmt_cost_small() {
        assert_eq!(fmt_cost(0.005), "$0.01"); // rounds up
    }

    #[test]
    fn test_fmt_cost_large() {
        assert_eq!(fmt_cost(123.456), "$123.46");
    }

    #[test]
    fn test_fmt_cost_whole_dollars() {
        assert_eq!(fmt_cost(5.0), "$5.00");
    }

    // ── NotConfigured when MYELIN_DATABASE_URL absent ─────────────────────────

    // Mutex to serialize tests that mutate MYELIN_DATABASE_URL env var.
    static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    // Helper: run an async closure with MYELIN_DATABASE_URL unset.
    // Acquires ENV_LOCK before mutation to prevent parallel races.
    // Awaits the future BEFORE restoring the var so pool() sees the removed value.
    async fn without_db_url<F, Fut, R>(f: F) -> R
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = R>,
    {
        let _guard = ENV_LOCK.lock().await;
        let saved = env::var("MYELIN_DATABASE_URL").ok();
        unsafe { env::remove_var("MYELIN_DATABASE_URL"); }
        let result = f().await;
        if let Some(v) = saved {
            unsafe { env::set_var("MYELIN_DATABASE_URL", v); }
        }
        result
    }

    macro_rules! not_configured_test {
        ($test_name:ident, $tool:expr) => {
            #[tokio::test]
            async fn $test_name() {
                without_db_url(|| async {
                    let tool = $tool;
                    let result = tool.execute(json!({})).await;
                    assert!(
                        matches!(result, Err(ToolError::NotConfigured(_))),
                        "expected NotConfigured, got {:?}",
                        result
                    );
                })
                .await;
            }
        };
    }

    not_configured_test!(test_status_not_configured, MyelinStatus);
    not_configured_test!(test_today_not_configured, MyelinToday);
    not_configured_test!(test_weekly_not_configured, MyelinWeekly);
    not_configured_test!(test_monthly_not_configured, MyelinMonthly);
    not_configured_test!(test_by_model_not_configured, MyelinByModel);
    not_configured_test!(test_by_user_not_configured, MyelinByUser);

    #[tokio::test]
    async fn test_runaway_check_not_configured() {
        without_db_url(|| async {
            let result = MyelinRunawayCheck
                .execute(json!({"threshold": 0.05}))
                .await;
            assert!(
                matches!(result, Err(ToolError::NotConfigured(_))),
                "expected NotConfigured, got {:?}",
                result
            );
        })
        .await;
    }

    #[tokio::test]
    async fn test_burn_plan_not_configured() {
        without_db_url(|| async {
            let result = MyelinBurnPlan.execute(json!({"budget": 50.0})).await;
            assert!(
                matches!(result, Err(ToolError::NotConfigured(_))),
                "expected NotConfigured, got {:?}",
                result
            );
        })
        .await;
    }

    #[tokio::test]
    async fn test_cap_check_not_configured() {
        without_db_url(|| async {
            let result = MyelinCapCheck
                .execute(json!({"daily_limit": 5.0}))
                .await;
            assert!(
                matches!(result, Err(ToolError::NotConfigured(_))),
                "expected NotConfigured, got {:?}",
                result
            );
        })
        .await;
    }

    // ── Argument validation (no DB needed) ────────────────────────────────────

    #[tokio::test]
    #[serial]
    async fn test_runaway_check_missing_threshold() {
        without_db_url(|| async {
            // Missing threshold arg → InvalidArgument (hits before DB check)
            // Actually hits NotConfigured first since env not set.
            // Test with a bad type instead — set a dummy URL so we get past env check.
            unsafe {
                env::set_var("MYELIN_DATABASE_URL", "postgres://invalid/invalid");
            }
            let result = MyelinRunawayCheck.execute(json!({})).await;
            // Should be InvalidArgument (threshold missing) or a DB error — not panic.
            assert!(result.is_err());
            unsafe {
                env::remove_var("MYELIN_DATABASE_URL");
            }
        })
        .await;
    }

    // These three validate args BEFORE pool() is reached, so the DB URL value
    // is irrelevant. Route through without_db_url so they acquire ENV_LOCK and
    // never leak MYELIN_DATABASE_URL into the parallel *_not_configured tests.
    #[tokio::test]
    async fn test_runaway_check_negative_threshold() {
        without_db_url(|| async {
            let result = MyelinRunawayCheck
                .execute(json!({"threshold": -1.0}))
                .await;
            assert!(matches!(result, Err(ToolError::InvalidArgument(_))));
        })
        .await;
    }

    #[tokio::test]
    async fn test_burn_plan_negative_budget() {
        without_db_url(|| async {
            let result = MyelinBurnPlan.execute(json!({"budget": -10.0})).await;
            assert!(matches!(result, Err(ToolError::InvalidArgument(_))));
        })
        .await;
    }

    #[tokio::test]
    async fn test_cap_check_zero_limit() {
        without_db_url(|| async {
            let result = MyelinCapCheck.execute(json!({"daily_limit": 0.0})).await;
            assert!(matches!(result, Err(ToolError::InvalidArgument(_))));
        })
        .await;
    }

    // ── Tool metadata ─────────────────────────────────────────────────────────

    #[test]
    fn test_tool_names_are_unique() {
        let all = tools();
        let mut names = std::collections::HashSet::new();
        for t in &all {
            assert!(
                names.insert(t.name().to_string()),
                "duplicate tool name: {}",
                t.name()
            );
        }
    }

    #[test]
    fn test_nine_tools_registered() {
        assert_eq!(tools().len(), 9);
    }

    #[test]
    fn test_all_tools_have_non_empty_description() {
        for t in tools() {
            assert!(!t.description().is_empty(), "tool {} has empty description", t.name());
        }
    }

    #[test]
    fn test_all_tools_parameters_are_objects() {
        for t in tools() {
            let params = t.parameters();
            assert_eq!(
                params["type"], "object",
                "tool {} parameters should have type=object",
                t.name()
            );
        }
    }

    #[test]
    fn test_register_adds_all_to_registry() {
        let mut reg = ToolRegistry::new();
        register(&mut reg);
        assert_eq!(reg.len(), 9);
        assert!(reg.contains("myelin_status"));
        assert!(reg.contains("myelin_today"));
        assert!(reg.contains("myelin_weekly"));
        assert!(reg.contains("myelin_monthly"));
        assert!(reg.contains("myelin_runaway_check"));
        assert!(reg.contains("myelin_burn_plan"));
        assert!(reg.contains("myelin_by_model"));
        assert!(reg.contains("myelin_by_user"));
        assert!(reg.contains("myelin_cap_check"));
    }

    // ── Cost formatting edge cases ────────────────────────────────────────────

    #[test]
    fn test_fmt_cost_two_decimal_places() {
        let s = fmt_cost(1.0);
        let parts: Vec<&str> = s.trim_start_matches('$').split('.').collect();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[1].len(), 2, "should have exactly 2 decimal places");
    }

    #[test]
    fn test_fmt_cost_dollar_prefix() {
        assert!(fmt_cost(42.0).starts_with('$'));
    }
}
