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

/// Resolve `name` to an absolute path of an executable file somewhere on
/// `$PATH`, or `None` if it isn't found. Returning (and the caller caching)
/// the absolute path -- rather than just a found/not-found boolean -- matters:
/// if only a boolean were cached, `Command::new(name)` would re-run PATH
/// search AGAIN at spawn time, re-opening the exact TOCTOU/PATH-mutation
/// window the "resolve once at startup" design is meant to close (a
/// directory could be added/reordered on PATH, or a file swapped in, between
/// startup and a later request). Spawning the cached absolute path instead
/// means the binary actually resolved at boot is the one that runs, every
/// time.
pub fn resolve_on_path(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate: PathBuf = dir.join(name);
        if is_executable_file(&candidate) {
            return Some(candidate);
        }
    }
    None
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
        let resolved = resolve_on_path("ls");
        assert!(resolved.is_some(), "expected `ls` to resolve on PATH");
        assert!(resolved.unwrap().is_absolute());
    }

    #[test]
    fn does_not_find_a_nonexistent_binary() {
        assert!(resolve_on_path("definitely-not-a-real-binary-xyz-123").is_none());
    }
}
