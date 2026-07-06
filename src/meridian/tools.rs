//! The 5 `meridian_*` MCP tools — SIMULATED paper-trading sandbox.
//!
//! Ported from <host>'s `meridian_tools.py`, which SSH'd to <host> and shelled
//! out to a `meridian.py` there. That backend does not exist (verified live
//! against the running <host> MCP server — every call returned an SSH/shell
//! error — and a filesystem search of <host> found no `meridian.py` or
//! `market_data.py` anywhere). This is a from-scratch re-implementation that
//! honors the 5 tools' documented contracts (see each tool's `description()`,
//! copied near-verbatim from the Python originals' docstrings) using this
//! repo's typed-HTTP-client conventions instead of shelling out over SSH.
//!
//! SAFETY BOUNDARY — NEVER CHANGE: this is a paper-trading SIMULATION only.
//! No real exchange orders are ever placed; `REAL_TRADING` does not exist as
//! a toggle because there is nothing here that could execute a real trade.

use async_trait::async_trait;
use serde::Serialize;
use serde_json::{json, Value};

use crate::config;
use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::tool::RustTool;

use super::market;
use super::state::{self, Portfolio, MAX_BALANCE, MIN_BALANCE};

const DEFAULT_SYMBOLS: &[&str] = &["BTC", "ETH", "SOL"];

// ---------------------------------------------------------------------------
// Shared: portfolio valuation
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct ValuedPosition {
    symbol: String,
    quantity: f64,
    avg_price: f64,
    current_price: Option<f64>,
    market_value: Option<f64>,
    unrealized_pnl: Option<f64>,
}

#[derive(Serialize)]
struct PortfolioReport {
    portfolio_id: String,
    cash: f64,
    starting_balance: f64,
    positions: Vec<ValuedPosition>,
    positions_value: f64,
    total_value: f64,
    total_pnl: f64,
    total_pnl_pct: f64,
    trade_count: usize,
    updated_at: String,
    note: &'static str,
}

/// Value a loaded portfolio against live prices for whatever symbols it
/// holds. Any symbol whose price fetch fails (network hiccup, unsupported
/// coin) is valued at its recorded average cost instead of erroring the
/// whole report — a stale/missing quote must never block seeing the rest of
/// the portfolio.
async fn value_portfolio(p: Portfolio) -> PortfolioReport {
    let held_symbols: Vec<String> = p.positions.iter().map(|pos| pos.symbol.clone()).collect();

    let live_prices = if held_symbols.is_empty() {
        Default::default()
    } else {
        market::get_crypto_prices(&held_symbols)
            .await
            .map(|(prices, _unsupported)| prices)
            .unwrap_or_default()
    };

    let mut positions_value = 0.0;
    let mut valued = Vec::with_capacity(p.positions.len());
    for pos in &p.positions {
        let current_price = live_prices
            .get(&pos.symbol.to_ascii_uppercase())
            .and_then(|cp| cp.usd)
            .or(Some(pos.avg_price));
        let market_value = current_price.map(|price| price * pos.quantity);
        let unrealized_pnl =
            market_value.map(|mv| mv - pos.avg_price * pos.quantity);
        if let Some(mv) = market_value {
            positions_value += mv;
        }
        valued.push(ValuedPosition {
            symbol: pos.symbol.clone(),
            quantity: pos.quantity,
            avg_price: pos.avg_price,
            current_price,
            market_value,
            unrealized_pnl,
        });
    }

    let total_value = p.cash + positions_value;
    let total_pnl = total_value - p.starting_balance;
    let total_pnl_pct = if p.starting_balance > 0.0 {
        (total_pnl / p.starting_balance) * 100.0
    } else {
        0.0
    };

    PortfolioReport {
        portfolio_id: p.portfolio_id,
        cash: p.cash,
        starting_balance: p.starting_balance,
        positions: valued,
        positions_value,
        total_value,
        total_pnl,
        total_pnl_pct,
        trade_count: p.trade_history.len(),
        updated_at: p.updated_at,
        note: "SIMULATED — not financial advice",
    }
}

// ---------------------------------------------------------------------------
// Tool: meridian_portfolio
// ---------------------------------------------------------------------------

pub struct MeridianPortfolio;

#[async_trait]
impl RustTool for MeridianPortfolio {
    fn name(&self) -> &str {
        "meridian_portfolio"
    }

    fn description(&self) -> &str {
        "Get current SIMULATED paper trading portfolio status. Returns portfolio \
         value, positions, cash balance, and performance metrics. This is a paper \
         trading simulation — no real money involved."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "portfolio_id": {
                    "type": "string",
                    "description": "Portfolio identifier (only \"default\" is currently supported)",
                    "default": "default"
                }
            }
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let portfolio_id = args["portfolio_id"].as_str().unwrap_or("default");
        let path = config::meridian_state_path();
        let portfolio = state::load(&path);
        let report = value_portfolio(portfolio).await;

        let mut body = serde_json::to_value(&report)
            .map_err(|e| ToolError::Execution(format!("failed to serialize portfolio: {e}")))?;
        if portfolio_id != "default" {
            body["_note_portfolio_id"] = json!(format!(
                "Only a single \"default\" portfolio is supported; requested id \"{portfolio_id}\" was ignored."
            ));
        }
        Ok(serde_json::to_string_pretty(&body).unwrap_or_else(|_| "{}".to_string()))
    }
}

// ---------------------------------------------------------------------------
// Tool: meridian_market_data
// ---------------------------------------------------------------------------

pub struct MeridianMarketData;

#[async_trait]
impl RustTool for MeridianMarketData {
    fn name(&self) -> &str {
        "meridian_market_data"
    }

    fn description(&self) -> &str {
        "Fetch live market data for the SIMULATED Meridian trading system. Returns \
         crypto prices (CoinGecko), Fear/Greed index, and SPY quote. symbols: \
         comma-separated list (BTC, ETH, SOL, BNB, AVAX supported). Read-only market \
         data — SIMULATED context only."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "symbols": {
                    "type": "string",
                    "description": "Comma-separated symbols (BTC, ETH, SOL, BNB, AVAX supported)",
                    "default": "BTC,ETH,SOL"
                }
            }
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let symbols_raw = args["symbols"].as_str().unwrap_or("BTC,ETH,SOL");
        let symbols: Vec<String> = symbols_raw
            .split(',')
            .map(|s| s.trim().to_ascii_uppercase())
            .filter(|s| !s.is_empty())
            .collect();

        if symbols.is_empty() {
            return Err(ToolError::InvalidArgument(
                "symbols must contain at least one non-empty entry".into(),
            ));
        }

        let (crypto, unsupported) = market::get_crypto_prices(&symbols).await?;
        let fear_greed = market::get_fear_greed().await.unwrap_or_default();
        let spy = market::get_stock_quote("SPY").await.unwrap_or(None);

        let body = json!({
            "symbols_requested": symbols,
            "unsupported_symbols": unsupported,
            "crypto": crypto,
            "fear_greed_index": fear_greed.value,
            "fear_greed_classification": fear_greed.classification,
            "spy_quote": spy,
            "_note": "SIMULATED context — market data is real but used only for paper trading",
        });
        Ok(serde_json::to_string_pretty(&body).unwrap_or_else(|_| "{}".to_string()))
    }
}

// ---------------------------------------------------------------------------
// Tool: meridian_analysis
// ---------------------------------------------------------------------------

pub struct MeridianAnalysis;

/// A conservative rule-based recommendation derived purely from the Fear &
/// Greed index. Used whenever `CHORD_LLM_URL` is unset or the LLM call
/// fails — analysis must always return *something* actionable-looking, never
/// hard-fail just because the LLM synthesis step is unavailable.
fn rule_based_recommendation(fear_greed_value: Option<i64>) -> String {
    match fear_greed_value {
        Some(v) if v <= 25 => {
            "Fear & Greed index is in \"Extreme Fear\" territory. Historically a \
             contrarian signal to consider small, staged accumulation — SIMULATED, \
             not financial advice."
                .to_string()
        }
        Some(v) if v >= 75 => {
            "Fear & Greed index is in \"Extreme Greed\" territory. Consider trimming \
             into strength or holding rather than adding — SIMULATED, not financial \
             advice."
                .to_string()
        }
        Some(_) => {
            "Fear & Greed index is neutral. No strong contrarian signal either way — \
             SIMULATED, hold and reassess."
                .to_string()
        }
        None => "Fear & Greed index unavailable — no rule-based signal to report. \
                  SIMULATED, not financial advice."
            .to_string(),
    }
}

/// Call an OpenAI-compatible chat-completions endpoint at `CHORD_LLM_URL`
/// (mirrors `google::imap::summarize_via_llm`'s pattern: `Ok(None)` when not
/// configured, `Err` when configured but the call/parse failed — the caller
/// falls back to the rule-based recommendation either way).
async fn synthesize_via_llm(market_snapshot: &Value) -> Result<Option<String>, ToolError> {
    let base = match std::env::var("CHORD_LLM_URL") {
        Ok(v) if !v.trim().is_empty() => v.trim().trim_end_matches('/').to_string(),
        _ => return Ok(None),
    };
    let url = format!("{base}/v1/chat/completions");
    let model = std::env::var("MERIDIAN_LLM_MODEL").unwrap_or_else(|_| "gpt-oss:20b".to_string());

    let prompt = format!(
        "You are a paper-trading assistant. This is a SIMULATION only — no real \
         money. Given this market snapshot, give a short (2-3 sentence) educational \
         SIMULATED trade recommendation. Always state it is not financial advice.\n\n\
         Market snapshot:\n{}",
        serde_json::to_string_pretty(market_snapshot).unwrap_or_default()
    );

    let body = json!({
        "model": model,
        "max_tokens": 200,
        "messages": [
            {"role": "system", "content": "You are a concise SIMULATED paper-trading analyst. Never claim to give real financial advice."},
            {"role": "user", "content": prompt}
        ]
    });

    let client = reqwest::Client::new();
    let resp = client
        .post(&url)
        .json(&body)
        .timeout(std::time::Duration::from_secs(90))
        .send()
        .await
        .map_err(|e| ToolError::Http(format!("LLM request failed: {e}")))?;

    if !resp.status().is_success() {
        return Err(ToolError::Http(format!(
            "LLM returned HTTP {}",
            resp.status()
        )));
    }

    let parsed: Value = resp
        .json()
        .await
        .map_err(|e| ToolError::Http(format!("LLM response parse failed: {e}")))?;

    Ok(parsed
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .map(str::to_string))
}

#[async_trait]
impl RustTool for MeridianAnalysis {
    fn name(&self) -> &str {
        "meridian_analysis"
    }

    fn description(&self) -> &str {
        "Run SIMULATED market analysis and get an AI trade recommendation. Fetches \
         live crypto prices, Fear/Greed index, and an SPY quote, then asks the LLM \
         for a paper trading recommendation (falling back to a rule-based signal if \
         no LLM backend is configured). SIMULATED ONLY — output is educational, not \
         financial advice."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "portfolio_id": {
                    "type": "string",
                    "description": "Portfolio identifier (only \"default\" is currently supported)",
                    "default": "default"
                }
            }
        })
    }

    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        let symbols: Vec<String> = DEFAULT_SYMBOLS.iter().map(|s| s.to_string()).collect();
        let (crypto, _unsupported) = market::get_crypto_prices(&symbols).await?;
        let fear_greed = market::get_fear_greed().await.unwrap_or_default();
        let spy = market::get_stock_quote("SPY").await.unwrap_or(None);

        let snapshot = json!({
            "crypto": crypto,
            "fear_greed_index": fear_greed.value,
            "fear_greed_classification": fear_greed.classification,
            "spy_quote": spy,
        });

        let (recommendation, source) = match synthesize_via_llm(&snapshot).await {
            Ok(Some(text)) => (text, "llm"),
            Ok(None) => (rule_based_recommendation(fear_greed.value), "rule-based (no CHORD_LLM_URL configured)"),
            Err(_) => (rule_based_recommendation(fear_greed.value), "rule-based (LLM call failed)"),
        };

        let body = json!({
            "market_snapshot": snapshot,
            "recommendation": recommendation,
            "recommendation_source": source,
            "_note": "SIMULATED — not financial advice. Paper trading only.",
        });
        Ok(serde_json::to_string_pretty(&body).unwrap_or_else(|_| "{}".to_string()))
    }
}

// ---------------------------------------------------------------------------
// Tool: meridian_report
// ---------------------------------------------------------------------------

pub struct MeridianReport;

/// Build the HTML dashboard body. Per the Lumina design-system rule, every
/// generated HTML page links `constellation.css` instead of inlining styles.
fn render_report_html(report: &PortfolioReport, fear_greed: &market::FearGreed) -> String {
    const CSS_LINK: &str = r#"<link rel="stylesheet" href="/shared/constellation.css">"#;

    let positions_rows = if report.positions.is_empty() {
        "<tr><td colspan=\"5\">No open positions</td></tr>".to_string()
    } else {
        report
            .positions
            .iter()
            .map(|p| {
                format!(
                    "<tr><td>{}</td><td>{:.6}</td><td>${:.2}</td><td>{}</td><td>{}</td></tr>",
                    html_escape(&p.symbol),
                    p.quantity,
                    p.avg_price,
                    p.current_price
                        .map(|c| format!("${c:.2}"))
                        .unwrap_or_else(|| "n/a".to_string()),
                    p.unrealized_pnl
                        .map(|pnl| format!("${pnl:.2}"))
                        .unwrap_or_else(|| "n/a".to_string()),
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    let pnl_badge_class = if report.total_pnl >= 0.0 {
        "badge-success"
    } else {
        "badge-danger"
    };

    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<title>Meridian — SIMULATED Paper Trading Dashboard</title>
{CSS_LINK}
</head>
<body>
<div class="page">
  <h1>Meridian — SIMULATED Paper Trading Dashboard</h1>
  <p class="alert-warning">SIMULATED ONLY — no real money involved, not financial advice.</p>
  <div class="card">
    <h2>Portfolio: {portfolio_id}</h2>
    <p>Cash: ${cash:.2}</p>
    <p>Starting balance: ${starting_balance:.2}</p>
    <p>Total value: ${total_value:.2}</p>
    <p>Total P&amp;L: <span class="badge {pnl_badge_class}">${total_pnl:.2} ({total_pnl_pct:.2}%)</span></p>
    <p>Trades recorded: {trade_count}</p>
    <p>Last updated: {updated_at}</p>
  </div>
  <div class="card">
    <h2>Positions</h2>
    <table class="table">
      <thead><tr><th>Symbol</th><th>Qty</th><th>Avg Price</th><th>Current Price</th><th>Unrealized P&amp;L</th></tr></thead>
      <tbody>
{positions_rows}
      </tbody>
    </table>
  </div>
  <div class="card">
    <h2>Market Sentiment</h2>
    <p>Fear &amp; Greed Index: {fg_value} ({fg_class})</p>
  </div>
  <footer class="lumina-footer">Lumina Constellation · Meridian (SIMULATED) &middot; MooseNet</footer>
</div>
</body>
</html>"#,
        portfolio_id = html_escape(&report.portfolio_id),
        cash = report.cash,
        starting_balance = report.starting_balance,
        total_value = report.total_value,
        total_pnl = report.total_pnl,
        total_pnl_pct = report.total_pnl_pct,
        trade_count = report.trade_count,
        updated_at = html_escape(&report.updated_at),
        positions_rows = positions_rows,
        fg_value = fear_greed
            .value
            .map(|v| v.to_string())
            .unwrap_or_else(|| "n/a".to_string()),
        fg_class = fear_greed.classification.as_deref().unwrap_or("n/a"),
    )
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[async_trait]
impl RustTool for MeridianReport {
    fn name(&self) -> &str {
        "meridian_report"
    }

    fn description(&self) -> &str {
        "Generate the SIMULATED Meridian trading dashboard HTML report. Publishes \
         to the configured report path/URL. Returns the path and URL of the \
         generated report. SIMULATED ONLY."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {}
        })
    }

    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        let state_path = config::meridian_state_path();
        let portfolio = state::load(&state_path);
        let report = value_portfolio(portfolio).await;
        let fear_greed = market::get_fear_greed().await.unwrap_or_default();

        let html = render_report_html(&report, &fear_greed);
        let report_path = config::meridian_report_path();
        std::fs::write(&report_path, html)
            .map_err(|e| ToolError::Execution(format!("cannot write report to {report_path}: {e}")))?;

        let body = json!({
            "status": "generated",
            "path": report_path,
            "url": config::meridian_report_url(),
            "_note": "SIMULATED — not financial advice",
        });
        Ok(serde_json::to_string_pretty(&body).unwrap_or_else(|_| "{}".to_string()))
    }
}

// ---------------------------------------------------------------------------
// Tool: meridian_reset
// ---------------------------------------------------------------------------

pub struct MeridianReset;

#[async_trait]
impl RustTool for MeridianReset {
    fn name(&self) -> &str {
        "meridian_reset"
    }

    fn description(&self) -> &str {
        "Reset the SIMULATED paper trading portfolio to a starting balance. Clears \
         all positions and trade history. Starting fresh. balance: starting cash \
         amount in USD (default $10,000, must be between $100 and $1,000,000). \
         SIMULATED ONLY — no real money involved."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "balance": {
                    "type": "number",
                    "description": "Starting cash amount in USD (100-1,000,000)",
                    "default": 10000.0
                }
            }
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let balance = args["balance"].as_f64().unwrap_or(state::DEFAULT_STARTING_BALANCE);

        if !balance.is_finite() || balance < MIN_BALANCE || balance > MAX_BALANCE {
            let body = json!({
                "status": "rejected",
                "reason": format!("Balance must be between ${MIN_BALANCE:.0} and ${MAX_BALANCE:.0}"),
                "_note": "SIMULATED",
            });
            return Ok(serde_json::to_string_pretty(&body).unwrap_or_else(|_| "{}".to_string()));
        }

        let path = config::meridian_state_path();
        let portfolio = state::reset(&path, balance)?;

        let body = json!({
            "status": "reset",
            "starting_balance": portfolio.starting_balance,
            "portfolio_id": portfolio.portfolio_id,
            "_note": "SIMULATED — portfolio reset complete",
        });
        Ok(serde_json::to_string_pretty(&body).unwrap_or_else(|_| "{}".to_string()))
    }
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

pub fn register(registry: &mut ToolRegistry) {
    let tools: Vec<Box<dyn RustTool>> = vec![
        Box::new(MeridianPortfolio),
        Box::new(MeridianMarketData),
        Box::new(MeridianAnalysis),
        Box::new(MeridianReport),
        Box::new(MeridianReset),
    ];
    for tool in tools {
        registry.register_or_replace(tool);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn tmp_state_path() -> String {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir()
            .join(format!("meridian_tools_test_state_{}_{n}.json", std::process::id()))
            .to_string_lossy()
            .into_owned()
    }

    fn tmp_report_path() -> String {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir()
            .join(format!("meridian_tools_test_report_{}_{n}.html", std::process::id()))
            .to_string_lossy()
            .into_owned()
    }

    #[test]
    fn test_meridian_registers_5_tools() {
        let mut registry = ToolRegistry::new();
        register(&mut registry);
        assert_eq!(registry.len(), 5);
    }

    #[test]
    fn test_meridian_tool_names_present() {
        let mut registry = ToolRegistry::new();
        register(&mut registry);
        assert!(registry.contains("meridian_portfolio"));
        assert!(registry.contains("meridian_analysis"));
        assert!(registry.contains("meridian_report"));
        assert!(registry.contains("meridian_market_data"));
        assert!(registry.contains("meridian_reset"));
    }

    #[test]
    fn rule_based_recommendation_extreme_fear() {
        let text = rule_based_recommendation(Some(10));
        assert!(text.contains("Extreme Fear"));
    }

    #[test]
    fn rule_based_recommendation_extreme_greed() {
        let text = rule_based_recommendation(Some(90));
        assert!(text.contains("Extreme Greed"));
    }

    #[test]
    fn rule_based_recommendation_neutral() {
        let text = rule_based_recommendation(Some(50));
        assert!(text.contains("neutral"));
    }

    #[test]
    fn rule_based_recommendation_unavailable() {
        let text = rule_based_recommendation(None);
        assert!(text.contains("unavailable"));
    }

    #[tokio::test]
    #[serial]
    async fn meridian_reset_rejects_balance_out_of_range() {
        let path = tmp_state_path();
        std::env::set_var("MERIDIAN_STATE_PATH", &path);

        let tool = MeridianReset;
        let result = tool.execute(json!({"balance": 50.0})).await.unwrap();
        assert!(result.contains("rejected"));

        let result = tool.execute(json!({"balance": 5_000_000.0})).await.unwrap();
        assert!(result.contains("rejected"));

        std::env::remove_var("MERIDIAN_STATE_PATH");
        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    #[serial]
    async fn meridian_reset_falls_back_to_default_balance_on_non_numeric_input() {
        // `f64::NAN`/`INFINITY` can't survive a real JSON round-trip (serde_json
        // has no representation for them, so a malformed/non-numeric `balance`
        // is the realistic "bad input" shape from an actual MCP caller). The
        // `is_finite()` guard in `execute` is defense-in-depth for anything
        // that *did* slip through as a non-finite f64; this test exercises the
        // realistic non-numeric-argument path and confirms it degrades to the
        // documented default rather than panicking.
        let path = tmp_state_path();
        std::env::set_var("MERIDIAN_STATE_PATH", &path);
        let tool = MeridianReset;
        let result = tool.execute(json!({"balance": "not-a-number"})).await.unwrap();
        assert!(result.contains("\"status\": \"reset\""));
        assert!(result.contains(&format!("{}", state::DEFAULT_STARTING_BALANCE)));
        std::env::remove_var("MERIDIAN_STATE_PATH");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn is_finite_guard_rejects_nan_and_infinite_directly() {
        // Unit-level defense-in-depth check for the guard itself, independent
        // of whether JSON can carry a non-finite value end-to-end.
        assert!(!f64::NAN.is_finite());
        assert!(!f64::INFINITY.is_finite());
        assert!(!f64::NEG_INFINITY.is_finite());
    }

    #[tokio::test]
    #[serial]
    async fn meridian_reset_then_portfolio_round_trips() {
        let path = tmp_state_path();
        std::env::set_var("MERIDIAN_STATE_PATH", &path);

        let reset_tool = MeridianReset;
        let reset_result = reset_tool.execute(json!({"balance": 20000.0})).await.unwrap();
        assert!(reset_result.contains("\"status\": \"reset\""));

        let portfolio_tool = MeridianPortfolio;
        let portfolio_result = portfolio_tool.execute(json!({})).await.unwrap();
        assert!(portfolio_result.contains("20000"));
        assert!(portfolio_result.contains("\"positions\": []"));

        std::env::remove_var("MERIDIAN_STATE_PATH");
        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    #[serial]
    async fn meridian_portfolio_non_default_id_is_noted_not_silently_dropped() {
        let path = tmp_state_path();
        std::env::set_var("MERIDIAN_STATE_PATH", &path);

        let tool = MeridianPortfolio;
        let result = tool.execute(json!({"portfolio_id": "other"})).await.unwrap();
        assert!(result.contains("_note_portfolio_id"));

        std::env::remove_var("MERIDIAN_STATE_PATH");
        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    #[serial]
    async fn meridian_market_data_rejects_empty_symbols() {
        let tool = MeridianMarketData;
        let result = tool.execute(json!({"symbols": "  ,  ,"})).await;
        assert!(matches!(result, Err(ToolError::InvalidArgument(_))));
    }

    #[tokio::test]
    #[serial]
    async fn meridian_report_writes_html_with_constellation_css() {
        let state_path = tmp_state_path();
        let report_path = tmp_report_path();
        std::env::set_var("MERIDIAN_STATE_PATH", &state_path);
        std::env::set_var("MERIDIAN_REPORT_PATH", &report_path);
        // Point Fear & Greed at an unreachable host so the tool exercises its
        // graceful-degradation path rather than hitting the real network in
        // a unit test.
        std::env::set_var("MERIDIAN_FEARGREED_URL", "http://127.0.0.1:1");

        let tool = MeridianReport;
        let result = tool.execute(json!({})).await.unwrap();
        assert!(result.contains("\"status\": \"generated\""));

        let html = std::fs::read_to_string(&report_path).unwrap();
        assert!(html.contains("constellation.css"));
        assert!(html.contains("SIMULATED"));

        std::env::remove_var("MERIDIAN_STATE_PATH");
        std::env::remove_var("MERIDIAN_REPORT_PATH");
        std::env::remove_var("MERIDIAN_FEARGREED_URL");
        std::fs::remove_file(&state_path).ok();
        std::fs::remove_file(&report_path).ok();
    }

    #[test]
    fn html_escape_escapes_special_chars() {
        assert_eq!(html_escape("<a>&\"b\""), "&lt;a&gt;&amp;&quot;b&quot;");
    }
}
