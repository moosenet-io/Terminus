//! Meridian — SIMULATED paper-trading crypto portfolio sandbox.
//!
//! Ported from the legacy host's Python `meridian_tools.py` (5 MCP tools:
//! `meridian_portfolio`, `meridian_analysis`, `meridian_report`,
//! `meridian_market_data`, `meridian_reset`). The Python original was a thin
//! wrapper that SSH'd from the legacy host to the fleet host (`pct exec <id> ...`) and shelled
//! out to a `meridian.py` / `market_data.py` pair under
//! `<path>/meridian/` on the fleet host.
//!
//! ## What was actually observed (live, 2026-07-06) — pii-test-fixture
//! Calling the live legacy-host tools returned SSH/shell errors (`ssh: Could not
//! resolve hostname <redacted-host>`, shell syntax errors) rather than portfolio data. A
//! filesystem search of the fleet host (`find / -iname 'meridian*' -o -iname
//! 'market_data*'`) found **no** `meridian.py`, `market_data.py`, or
//! `<path>/meridian/` directory anywhere. The backend this wrapper
//! called was never deployed — there is no real prior behavior or
//! persistence design to port faithfully, only the Python wrapper's
//! documented *contract* (its docstrings, argument shapes, and the
//! `REAL_TRADING = False` safety invariant).
//!
//! ## Design choices made here (new, not ported)
//! - **State persistence**: a single whole-document JSON file (one
//!   `"default"` portfolio), guarded by an in-process mutex and written
//!   atomically (temp file + rename) — see [`state`]. Path is
//!   `config::meridian_state_path()` (env `MERIDIAN_STATE_PATH`).
//! - **Market data**: direct typed HTTP GETs to CoinGecko (crypto spot
//!   prices), alternative.me (Fear & Greed Index), and Stooq (SPY spot quote
//!   via its public CSV endpoint) — all public, unauthenticated, well-known
//!   APIs — see [`market`].
//! - **LLM synthesis**: mirrors `google::imap::summarize_via_llm`'s
//!   established pattern (`CHORD_LLM_URL`, OpenAI-compatible
//!   `/v1/chat/completions`, `Ok(None)` when unconfigured) with a
//!   deterministic rule-based fallback recommendation so `meridian_analysis`
//!   never hard-fails just because no LLM backend is wired up.
//! - **Reports**: HTML output uses `constellation.css` per the Lumina
//!   design-system rule, written to `config::meridian_report_path()`.
//!
//! SAFETY BOUNDARY — NEVER CHANGE: this is a paper-trading SIMULATION only.
//! There is no code path here that places a real order or moves real money.

mod market;
mod state;
mod tools;

pub use tools::register;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::ToolRegistry;

    #[test]
    fn meridian_module_registers_5_tools() {
        let mut registry = ToolRegistry::new();
        register(&mut registry);
        assert_eq!(registry.len(), 5);
    }
}
