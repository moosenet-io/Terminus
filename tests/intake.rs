//! Integration-test harness for the S84 intake assistant-profile dimensions.
//!
//! Cargo compiles each top-level file in `tests/` as its own test binary but
//! treats files in subdirectories as plain modules. This harness pulls the
//! spec-located `tests/intake/dim5_prompted.rs` into the `intake` test binary so
//! it runs under `cargo test -p terminus-rs`.

#[path = "intake/dim5_prompted.rs"]
mod dim5_prompted;
