//! Read-only external market-data fetches for the SIMULATED Meridian sandbox:
//! CoinGecko (crypto spot prices), alternative.me (Fear & Greed Index), and
//! Stooq (SPY spot quote via its public CSV quote endpoint).
//!
//! All three are public, unauthenticated, well-known APIs — no API keys
//! required. Every parse is tolerant: a malformed/unexpected shape from any
//! of these third-party services (outside our control) degrades to a `None`
//! field or an in-band error message, never a panic.

use serde::Deserialize;
use std::collections::BTreeMap;

use crate::config;
use crate::error::ToolError;

/// Symbols the sandbox understands, and their CoinGecko coin ids.
pub fn coingecko_id(symbol: &str) -> Option<&'static str> {
    match symbol.to_ascii_uppercase().as_str() {
        "BTC" => Some("bitcoin"),
        "ETH" => Some("ethereum"),
        "SOL" => Some("solana"),
        "BNB" => Some("binancecoin"),
        "AVAX" => Some("avalanche-2"),
        _ => None,
    }
}

pub fn client() -> Result<reqwest::Client, ToolError> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .user_agent("MooseNet-MCP/1.0")
        .build()
        .map_err(|e| ToolError::Http(e.to_string()))
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct CryptoPrice {
    pub usd: Option<f64>,
    pub usd_24h_change: Option<f64>,
}

/// Fetch spot USD prices (+ 24h change) for the given symbols from CoinGecko.
/// Symbols not in [`coingecko_id`]'s supported set are reported back in
/// `unsupported` rather than silently dropped or erroring the whole call.
pub async fn get_crypto_prices(
    symbols: &[String],
) -> Result<(BTreeMap<String, CryptoPrice>, Vec<String>), ToolError> {
    let mut ids_to_symbol: BTreeMap<&'static str, String> = BTreeMap::new();
    let mut unsupported = Vec::new();
    for s in symbols {
        match coingecko_id(s) {
            Some(id) => {
                ids_to_symbol.insert(id, s.to_ascii_uppercase());
            }
            None => unsupported.push(s.clone()),
        }
    }

    if ids_to_symbol.is_empty() {
        return Ok((BTreeMap::new(), unsupported));
    }

    let ids_param = ids_to_symbol.keys().cloned().collect::<Vec<_>>().join(",");
    let base = config::meridian_coingecko_url();
    let url = format!("{}/api/v3/simple/price", base.trim_end_matches('/'));

    let resp = client()?
        .get(&url)
        .query(&[
            ("ids", ids_param.as_str()),
            ("vs_currencies", "usd"),
            ("include_24hr_change", "true"),
        ])
        .send()
        .await
        .map_err(|e| ToolError::Http(format!("CoinGecko request failed: {e}")))?;

    if !resp.status().is_success() {
        return Err(ToolError::Http(format!(
            "CoinGecko returned HTTP {}",
            resp.status()
        )));
    }

    #[derive(Debug, Deserialize, Default)]
    struct RawEntry {
        #[serde(default)]
        usd: Option<f64>,
        #[serde(default)]
        usd_24h_change: Option<f64>,
    }

    let raw: BTreeMap<String, RawEntry> = resp
        .json()
        .await
        .map_err(|e| ToolError::Http(format!("Failed to parse CoinGecko response: {e}")))?;

    let mut out = BTreeMap::new();
    for (id, symbol) in &ids_to_symbol {
        let entry = raw.get(*id).map(|r| CryptoPrice {
            usd: r.usd,
            usd_24h_change: r.usd_24h_change,
        });
        out.insert(symbol.clone(), entry.unwrap_or_default());
    }

    Ok((out, unsupported))
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct FearGreed {
    pub value: Option<i64>,
    pub classification: Option<String>,
}

/// Fetch the current Fear & Greed Index from alternative.me. Returns a
/// default (all-`None`) value rather than erroring if the response shape is
/// unexpected — this is a sentiment input, not a required field.
pub async fn get_fear_greed() -> Result<FearGreed, ToolError> {
    #[derive(Debug, Deserialize, Default)]
    struct Entry {
        #[serde(default)]
        value: Option<String>,
        #[serde(default)]
        value_classification: Option<String>,
    }
    #[derive(Debug, Deserialize, Default)]
    struct FngResponse {
        #[serde(default)]
        data: Vec<Entry>,
    }

    let base = config::meridian_feargreed_url();
    let url = format!("{}/fng/", base.trim_end_matches('/'));

    let resp = client()?
        .get(&url)
        .query(&[("limit", "1"), ("format", "json")])
        .send()
        .await
        .map_err(|e| ToolError::Http(format!("Fear & Greed request failed: {e}")))?;

    if !resp.status().is_success() {
        return Err(ToolError::Http(format!(
            "Fear & Greed API returned HTTP {}",
            resp.status()
        )));
    }

    let parsed: Result<FngResponse, _> = resp.json().await;
    let entry = parsed.ok().and_then(|r| r.data.into_iter().next());
    match entry {
        Some(e) => Ok(FearGreed {
            value: e.value.and_then(|v| v.parse().ok()),
            classification: e.value_classification,
        }),
        None => Ok(FearGreed::default()),
    }
}

/// Fetch a spot quote for a stock/ETF ticker via Stooq's public CSV quote
/// endpoint. Returns `None` for the close price if Stooq has no data for the
/// symbol (it reports `N/D` in that case) rather than erroring.
pub async fn get_stock_quote(symbol: &str) -> Result<Option<f64>, ToolError> {
    let base = config::meridian_stooq_url();
    let url = format!("{}/q/l/", base.trim_end_matches('/'));
    let stooq_symbol = format!("{}.us", symbol.to_ascii_lowercase());

    let resp = client()?
        .get(&url)
        .query(&[("s", stooq_symbol.as_str()), ("f", "sd2t2ohlcv"), ("e", "csv")])
        .send()
        .await
        .map_err(|e| ToolError::Http(format!("Stooq request failed: {e}")))?;

    if !resp.status().is_success() {
        return Err(ToolError::Http(format!(
            "Stooq returned HTTP {}",
            resp.status()
        )));
    }

    let body = resp
        .text()
        .await
        .map_err(|e| ToolError::Http(format!("Failed to read Stooq response: {e}")))?;

    Ok(parse_stooq_close(&body))
}

/// Parse the `Close` field out of Stooq's CSV quote format:
/// `Symbol,Date,Time,Open,High,Low,Close,Volume` (header + one data row).
/// Tolerant of any unexpected shape — returns `None` rather than panicking.
fn parse_stooq_close(csv: &str) -> Option<f64> {
    let mut lines = csv.lines();
    let _header = lines.next()?;
    let data_line = lines.next()?;
    let fields: Vec<&str> = data_line.split(',').collect();
    // Symbol,Date,Time,Open,High,Low,Close,Volume -> Close is index 6.
    let close = fields.get(6)?.trim();
    if close.is_empty() || close.eq_ignore_ascii_case("N/D") {
        return None;
    }
    close.parse::<f64>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;
    use serial_test::serial;

    #[test]
    fn coingecko_id_known_symbols() {
        assert_eq!(coingecko_id("BTC"), Some("bitcoin"));
        assert_eq!(coingecko_id("btc"), Some("bitcoin"));
        assert_eq!(coingecko_id("eth"), Some("ethereum"));
        assert_eq!(coingecko_id("sol"), Some("solana"));
        assert_eq!(coingecko_id("bnb"), Some("binancecoin"));
        assert_eq!(coingecko_id("avax"), Some("avalanche-2"));
    }

    #[test]
    fn coingecko_id_unknown_symbol_is_none() {
        assert_eq!(coingecko_id("DOGE"), None);
        assert_eq!(coingecko_id(""), None);
    }

    #[test]
    fn parse_stooq_close_normal_row() {
        let csv = "Symbol,Date,Time,Open,High,Low,Close,Volume\nSPY.US,2024-01-01,21:00:00,470.1,472.0,469.5,471.3,12345678\n";
        assert_eq!(parse_stooq_close(csv), Some(471.3));
    }

    #[test]
    fn parse_stooq_close_no_data_symbol() {
        let csv = "Symbol,Date,Time,Open,High,Low,Close,Volume\nZZZZ.US,N/D,N/D,N/D,N/D,N/D,N/D,N/D\n";
        assert_eq!(parse_stooq_close(csv), None);
    }

    #[test]
    fn parse_stooq_close_malformed_missing_fields() {
        assert_eq!(parse_stooq_close("just,a,header\n"), None);
        assert_eq!(parse_stooq_close(""), None);
        assert_eq!(parse_stooq_close("only one line no newline"), None);
    }

    #[test]
    fn parse_stooq_close_garbage_value_does_not_panic() {
        let csv = "h\nSPY.US,d,t,o,h,l,not-a-number,v\n";
        assert_eq!(parse_stooq_close(csv), None);
    }

    #[tokio::test]
    #[serial]
    async fn get_crypto_prices_happy_path() {
        let server = MockServer::start();
        std::env::set_var("MERIDIAN_COINGECKO_URL", server.base_url());

        let m = server.mock(|when, then| {
            when.method(GET).path("/api/v3/simple/price");
            then.status(200)
                .header("content-type", "application/json")
                .body(r#"{"bitcoin":{"usd":67000.5,"usd_24h_change":1.2},"ethereum":{"usd":3500.1,"usd_24h_change":-0.5}}"#);
        });

        let (prices, unsupported) = get_crypto_prices(&["BTC".into(), "ETH".into()])
            .await
            .unwrap();
        m.assert();
        assert!(unsupported.is_empty());
        assert_eq!(prices["BTC"].usd, Some(67000.5));
        assert_eq!(prices["ETH"].usd_24h_change, Some(-0.5));

        std::env::remove_var("MERIDIAN_COINGECKO_URL");
    }

    #[tokio::test]
    #[serial]
    async fn get_crypto_prices_reports_unsupported_symbols() {
        let server = MockServer::start();
        std::env::set_var("MERIDIAN_COINGECKO_URL", server.base_url());
        server.mock(|when, then| {
            when.method(GET).path("/api/v3/simple/price");
            then.status(200)
                .header("content-type", "application/json")
                .body(r#"{"bitcoin":{"usd":1.0}}"#);
        });

        let (_prices, unsupported) = get_crypto_prices(&["BTC".into(), "DOGE".into()])
            .await
            .unwrap();
        assert_eq!(unsupported, vec!["DOGE".to_string()]);
        std::env::remove_var("MERIDIAN_COINGECKO_URL");
    }

    #[tokio::test]
    #[serial]
    async fn get_crypto_prices_all_unsupported_skips_http_call() {
        // No mock registered at all -- if the code tried to call out, this
        // would fail to connect / panic. It must short-circuit instead.
        std::env::set_var("MERIDIAN_COINGECKO_URL", "http://127.0.0.1:1");
        let (prices, unsupported) = get_crypto_prices(&["DOGE".into()]).await.unwrap();
        assert!(prices.is_empty());
        assert_eq!(unsupported, vec!["DOGE".to_string()]);
        std::env::remove_var("MERIDIAN_COINGECKO_URL");
    }

    #[tokio::test]
    #[serial]
    async fn get_crypto_prices_malformed_response_errors_cleanly() {
        let server = MockServer::start();
        std::env::set_var("MERIDIAN_COINGECKO_URL", server.base_url());
        server.mock(|when, then| {
            when.method(GET).path("/api/v3/simple/price");
            then.status(200)
                .header("content-type", "application/json")
                .body("not json at all");
        });

        let result = get_crypto_prices(&["BTC".into()]).await;
        assert!(result.is_err());
        std::env::remove_var("MERIDIAN_COINGECKO_URL");
    }

    #[tokio::test]
    #[serial]
    async fn get_fear_greed_happy_path() {
        let server = MockServer::start();
        std::env::set_var("MERIDIAN_FEARGREED_URL", server.base_url());
        server.mock(|when, then| {
            when.method(GET).path("/fng/");
            then.status(200)
                .header("content-type", "application/json")
                .body(r#"{"data":[{"value":"55","value_classification":"Neutral"}]}"#);
        });

        let fg = get_fear_greed().await.unwrap();
        assert_eq!(fg.value, Some(55));
        assert_eq!(fg.classification.as_deref(), Some("Neutral"));
        std::env::remove_var("MERIDIAN_FEARGREED_URL");
    }

    #[tokio::test]
    #[serial]
    async fn get_fear_greed_malformed_response_degrades_to_default() {
        let server = MockServer::start();
        std::env::set_var("MERIDIAN_FEARGREED_URL", server.base_url());
        server.mock(|when, then| {
            when.method(GET).path("/fng/");
            then.status(200)
                .header("content-type", "application/json")
                .body(r#"{"totally": "unexpected"}"#);
        });

        let fg = get_fear_greed().await.unwrap();
        assert_eq!(fg.value, None);
        assert_eq!(fg.classification, None);
        std::env::remove_var("MERIDIAN_FEARGREED_URL");
    }

    #[tokio::test]
    #[serial]
    async fn get_fear_greed_non_success_status_errors() {
        let server = MockServer::start();
        std::env::set_var("MERIDIAN_FEARGREED_URL", server.base_url());
        server.mock(|when, then| {
            when.method(GET).path("/fng/");
            then.status(503);
        });

        let result = get_fear_greed().await;
        assert!(result.is_err());
        std::env::remove_var("MERIDIAN_FEARGREED_URL");
    }

    #[tokio::test]
    #[serial]
    async fn get_stock_quote_happy_path() {
        let server = MockServer::start();
        std::env::set_var("MERIDIAN_STOOQ_URL", server.base_url());
        server.mock(|when, then| {
            when.method(GET).path("/q/l/");
            then.status(200)
                .header("content-type", "text/csv")
                .body("Symbol,Date,Time,Open,High,Low,Close,Volume\nSPY.US,2024-01-01,21:00:00,470.1,472.0,469.5,471.3,12345678\n");
        });

        let quote = get_stock_quote("SPY").await.unwrap();
        assert_eq!(quote, Some(471.3));
        std::env::remove_var("MERIDIAN_STOOQ_URL");
    }

    #[tokio::test]
    #[serial]
    async fn get_stock_quote_no_data_returns_none_not_error() {
        let server = MockServer::start();
        std::env::set_var("MERIDIAN_STOOQ_URL", server.base_url());
        server.mock(|when, then| {
            when.method(GET).path("/q/l/");
            then.status(200)
                .header("content-type", "text/csv")
                .body("Symbol,Date,Time,Open,High,Low,Close,Volume\nZZZZ.US,N/D,N/D,N/D,N/D,N/D,N/D,N/D\n");
        });

        let quote = get_stock_quote("ZZZZ").await.unwrap();
        assert_eq!(quote, None);
        std::env::remove_var("MERIDIAN_STOOQ_URL");
    }
}
