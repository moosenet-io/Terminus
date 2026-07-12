//! CXEG-05: standalone local runner for the `crate::house_style` checker.
//!
//! Same checker `tests/house_style.rs` runs as part of `cargo test -p
//! terminus-rs` (the Stage-4 gate) — this binary exists for a fast, isolated
//! local run without pulling in the rest of the test suite:
//!
//! ```text
//! cargo run --bin house_style_check
//! ```
//!
//! Exit code `0` when clean, `1` when any violation is found. See
//! `docs/house-style.md` for the rule catalog and the `// house-style-allow:
//! <reason>` waiver convention.

use std::path::Path;
use std::process::ExitCode;

fn main() -> ExitCode {
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let violations = terminus_rs::house_style::check_tree(repo_root);

    if violations.is_empty() {
        println!("house_style_check: clean (0 violations)");
        return ExitCode::SUCCESS;
    }

    eprintln!("house_style_check: {} violation(s):\n", violations.len());
    for v in &violations {
        eprintln!("{v}\n");
    }
    ExitCode::FAILURE
}
