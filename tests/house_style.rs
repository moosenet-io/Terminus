//! CXEG-05: Stage-4 gate wiring for the `crate::house_style` checker.
//!
//! `cargo test -p terminus-rs` runs this file automatically (Cargo
//! auto-discovers files directly under `tests/`), so the deterministic
//! `syn`-AST house-style checks run on every test-gate pass without any
//! separate pipeline step. See `docs/house-style.md` for the rule catalog and
//! the `// house-style-allow: <reason>` waiver convention; `cargo run --bin
//! house_style_check` runs the same checker standalone for a faster local
//! loop.

#[test]
fn house_style_rules_hold() {
    let repo_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let violations = terminus_rs::house_style::check_tree(repo_root);
    assert!(
        violations.is_empty(),
        "house-style violations found ({} total) -- fix them or add a reasoned \
         `// house-style-allow: <reason>` waiver (see docs/house-style.md):\n\n{}",
        violations.len(),
        violations.iter().map(ToString::to_string).collect::<Vec<_>>().join("\n\n")
    );
}
