//! Pure-Rust `PATH` resolution for provider binaries.
//!
//! Deliberately does NOT shell out to `which` (that would itself be a process
//! spawn, and would need to happen per-request if not cached correctly).
//! Instead it scans `$PATH` directories directly with `std::fs`, exactly once
//! at daemon startup, and the resulting presence/absence is cached in
//! `AppState` for the life of the process. A provider whose binary is not
//! found on startup reports `binary_not_found` for every subsequent request
//! rather than re-checking the filesystem per call.

use std::path::PathBuf;

/// Return `true` if `name` resolves to an executable file somewhere on `$PATH`.
pub fn resolve_on_path(name: &str) -> bool {
    let Some(path_var) = std::env::var_os("PATH") else {
        return false;
    };
    for dir in std::env::split_paths(&path_var) {
        let candidate: PathBuf = dir.join(name);
        if is_executable_file(&candidate) {
            return true;
        }
    }
    false
}

#[cfg(unix)]
fn is_executable_file(path: &PathBuf) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(path) {
        Ok(meta) => meta.is_file() && (meta.permissions().mode() & 0o111 != 0),
        Err(_) => false,
    }
}

#[cfg(not(unix))]
fn is_executable_file(path: &PathBuf) -> bool {
    path.is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_a_binary_known_to_exist() {
        // `ls` exists on every unix CI/dev box this daemon targets.
        assert!(resolve_on_path("ls"), "expected `ls` to resolve on PATH");
    }

    #[test]
    fn does_not_find_a_nonexistent_binary() {
        assert!(!resolve_on_path("definitely-not-a-real-binary-xyz-123"));
    }
}
