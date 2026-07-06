//! Local JSON-file persistence for the SIMULATED Meridian paper-trading
//! portfolio.
//!
//! <host>'s Python `meridian_tools.py` SSH'd to <host> and shelled out to
//! `<path>/meridian/meridian.py`. That directory never existed on
//! <host> (verified live: every real call returned an SSH/shell error, and a
//! filesystem search of <host> turned up no `meridian.py` or `market_data.py`
//! anywhere) — there is no prior persistence design to port faithfully. This
//! module is a new, from-scratch design: a single whole-document JSON file
//! (one portfolio, id `"default"`), guarded by an in-process mutex and
//! written atomically (temp file + rename) so a reset can never be observed
//! half-written and two concurrent resets can't interleave their writes.
//!
//! SAFETY BOUNDARY — this is a paper-trading SIMULATION. No real money, no
//! real exchange orders, ever.

use serde::{Deserialize, Serialize};
use std::io::Write;
use std::sync::Mutex;

use crate::error::ToolError;

/// Guards every read-modify-write against the state file so two concurrent
/// tool calls (e.g. two `meridian_reset`s, or a reset racing a read) can't
/// interleave. File IO here is synchronous and fast (a few KB), so holding
/// this across the operation without any `.await` in between is fine even
/// from async tool handlers.
static STATE_LOCK: Mutex<()> = Mutex::new(());

pub const DEFAULT_STARTING_BALANCE: f64 = 10_000.0;
pub const MIN_BALANCE: f64 = 100.0;
pub const MAX_BALANCE: f64 = 1_000_000.0;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Position {
    pub symbol: String,
    pub quantity: f64,
    pub avg_price: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TradeRecord {
    pub timestamp: String,
    pub action: String,
    pub symbol: String,
    pub quantity: f64,
    pub price: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Portfolio {
    pub portfolio_id: String,
    pub cash: f64,
    pub starting_balance: f64,
    #[serde(default)]
    pub positions: Vec<Position>,
    #[serde(default)]
    pub trade_history: Vec<TradeRecord>,
    pub created_at: String,
    pub updated_at: String,
}

impl Portfolio {
    pub fn fresh(portfolio_id: &str, balance: f64) -> Self {
        let now = chrono::Utc::now().to_rfc3339();
        Portfolio {
            portfolio_id: portfolio_id.to_string(),
            cash: balance,
            starting_balance: balance,
            positions: Vec::new(),
            trade_history: Vec::new(),
            created_at: now.clone(),
            updated_at: now,
        }
    }
}

/// Load the portfolio from disk. A missing file or an unparsable file is
/// treated as "no portfolio yet" (returns a fresh default portfolio) rather
/// than an error — this is sandbox state, not a durability-critical ledger,
/// and a corrupt/partial file must never wedge every future call.
pub fn load(path: &str) -> Portfolio {
    let _guard = STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    load_locked(path)
}

fn load_locked(path: &str) -> Portfolio {
    match std::fs::read_to_string(path) {
        Ok(raw) => serde_json::from_str(&raw)
            .unwrap_or_else(|_| Portfolio::fresh("default", DEFAULT_STARTING_BALANCE)),
        Err(_) => Portfolio::fresh("default", DEFAULT_STARTING_BALANCE),
    }
}

/// Reset the portfolio at `path` to a fresh starting balance and persist it,
/// atomically (write to a sibling temp file, then rename over the target —
/// `rename` is atomic on the same filesystem, so a concurrent reader never
/// observes a half-written file). Returns the freshly written portfolio.
pub fn reset(path: &str, balance: f64) -> Result<Portfolio, ToolError> {
    let _guard = STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let portfolio = Portfolio::fresh("default", balance);
    save_locked(path, &portfolio)?;
    Ok(portfolio)
}

fn save_locked(path: &str, portfolio: &Portfolio) -> Result<(), ToolError> {
    let json = serde_json::to_string_pretty(portfolio)
        .map_err(|e| ToolError::Execution(format!("failed to serialize portfolio: {e}")))?;

    let tmp_path = format!("{path}.tmp-{}", std::process::id());
    {
        let mut f = std::fs::File::create(&tmp_path)
            .map_err(|e| ToolError::Execution(format!("cannot create {tmp_path}: {e}")))?;
        f.write_all(json.as_bytes())
            .map_err(|e| ToolError::Execution(format!("cannot write {tmp_path}: {e}")))?;
        f.sync_all().ok();
    }
    std::fs::rename(&tmp_path, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp_path);
        ToolError::Execution(format!("cannot finalize {path}: {e}"))
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn tmp_path() -> String {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir()
            .join(format!("meridian_state_test_{}_{n}.json", std::process::id()))
            .to_string_lossy()
            .into_owned()
    }

    #[test]
    fn load_missing_file_returns_fresh_default_portfolio() {
        let path = tmp_path();
        let p = load(&path);
        assert_eq!(p.portfolio_id, "default");
        assert_eq!(p.cash, DEFAULT_STARTING_BALANCE);
        assert_eq!(p.starting_balance, DEFAULT_STARTING_BALANCE);
        assert!(p.positions.is_empty());
        assert!(p.trade_history.is_empty());
    }

    #[test]
    fn load_corrupt_file_returns_fresh_default_portfolio() {
        let path = tmp_path();
        std::fs::write(&path, "{ not valid json").unwrap();
        let p = load(&path);
        assert_eq!(p.cash, DEFAULT_STARTING_BALANCE);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn reset_then_load_round_trips() {
        let path = tmp_path();
        let written = reset(&path, 25_000.0).unwrap();
        assert_eq!(written.cash, 25_000.0);
        let loaded = load(&path);
        assert_eq!(loaded.cash, 25_000.0);
        assert_eq!(loaded.starting_balance, 25_000.0);
        assert_eq!(loaded.portfolio_id, "default");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn reset_overwrites_prior_state_and_clears_positions() {
        let path = tmp_path();
        reset(&path, 5_000.0).unwrap();
        // Simulate some prior activity by writing directly.
        let mut with_position = load(&path);
        with_position.positions.push(Position {
            symbol: "BTC".into(),
            quantity: 0.5,
            avg_price: 50_000.0,
        });
        save_locked(&path, &with_position).unwrap();
        assert_eq!(load(&path).positions.len(), 1);

        reset(&path, 5_000.0).unwrap();
        let after = load(&path);
        assert!(after.positions.is_empty());
        assert!(after.trade_history.is_empty());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn reset_never_leaves_a_temp_file_behind() {
        let path = tmp_path();
        reset(&path, 1_000.0).unwrap();
        let tmp = format!("{path}.tmp-{}", std::process::id());
        assert!(!std::path::Path::new(&tmp).exists());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn concurrent_resets_do_not_corrupt_the_file() {
        use std::sync::Arc;
        use std::thread;

        let path = Arc::new(tmp_path());
        reset(&path, 1_000.0).unwrap();

        let mut handles = Vec::new();
        for i in 0..8 {
            let path = Arc::clone(&path);
            handles.push(thread::spawn(move || {
                reset(&path, 1_000.0 + i as f64).unwrap();
            }));
        }
        for h in handles {
            h.join().unwrap();
        }

        // Whatever the last writer was, the file must parse cleanly as a
        // complete, valid Portfolio — never a torn/partial write.
        let raw = std::fs::read_to_string(path.as_str()).unwrap();
        let parsed: Result<Portfolio, _> = serde_json::from_str(&raw);
        assert!(parsed.is_ok(), "state file corrupted by concurrent resets");
        std::fs::remove_file(path.as_str()).ok();
    }

    #[test]
    fn fresh_sets_starting_balance_equal_to_cash() {
        let p = Portfolio::fresh("default", 42_000.0);
        assert_eq!(p.cash, p.starting_balance);
        assert_eq!(p.cash, 42_000.0);
        assert!(p.positions.is_empty());
        assert!(p.trade_history.is_empty());
        assert_eq!(p.created_at, p.updated_at);
    }
}
