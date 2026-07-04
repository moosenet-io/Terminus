//! Generic file-backed, JSON-lines, append-on-mark resume checkpoint.
//!
//! Extracted (Phase 2, item 1) from two near-identical implementations that
//! had drifted apart only in their key shape:
//!   - the coder sweep's `CodeCheckpoint` (`coder_sweep.rs`), keyed on
//!     `(model_id, backend_tag)`;
//!   - the assistant runner's `FileCheckpoint` (`assistant/runner.rs`), keyed
//!     on `(model_id, backend_tag, dimension)`.
//!
//! Both are the SAME durability pattern: a small JSON-lines file on the
//! reliable NAS staging dir, read once at startup into a `BTreeSet` (`done`),
//! and appended to (never rewritten) the instant a unit of work's real rows
//! land durably elsewhere (Postgres) — so a crash between "rows persisted"
//! and "checkpoint marked" can only ever cause a harmless re-run, never a
//! checkpoint that claims work the DB doesn't actually have. This type is
//! that pattern, parameterized over the caller's own key struct `K`.
//!
//! This is a pure refactor: callers still resolve their own on-disk path
//! (from `INTAKE_STAGING_DIR` et al, via `config.rs`) and still own their own
//! key struct/`open()`-style constructor; only the read/append/dedup
//! mechanics are shared.

use std::collections::BTreeSet;
use std::fs::OpenOptions;
use std::io::Write;
use std::marker::PhantomData;

use serde::de::DeserializeOwned;
use serde::Serialize;

/// A JSON-lines, append-only, file-backed set of completed-work keys `K`.
///
/// `K` needs `Ord` (not `Hash`) because the in-memory "done" set is a
/// `BTreeSet` — both original implementations used `BTreeSet` (their key
/// structs already derive `Ord`/`PartialOrd`), so this preserves that exact
/// shape rather than introducing a `HashSet` + `Hash` bound that neither
/// caller asked for.
pub struct FileCheckpoint<K> {
    path: String,
    _key: PhantomData<fn() -> K>,
}

impl<K> FileCheckpoint<K>
where
    K: Serialize + DeserializeOwned + Eq + Ord,
{
    /// Wrap an already-resolved path. Never fails and never touches the
    /// filesystem — a missing file simply reads back as "nothing done yet"
    /// (see [`Self::done`]); the file itself is created lazily on first
    /// [`Self::mark`]. Callers resolve `path` themselves (typically from
    /// `INTAKE_STAGING_DIR` via `config.rs`) so this type carries no
    /// env-reading or `ToolError`-shaped config-resolution logic of its own.
    pub fn at(path: impl Into<String>) -> Self {
        FileCheckpoint {
            path: path.into(),
            _key: PhantomData,
        }
    }

    /// The resolved on-disk path (for logging — mirrors what both original
    /// implementations printed at startup).
    pub fn path(&self) -> &str {
        &self.path
    }

    /// All keys already marked complete (empty on a fresh/missing file).
    /// Lines that fail to parse (e.g. a partial line from a crash mid-write)
    /// are silently skipped rather than failing the whole read — matches
    /// both original implementations' tolerance.
    pub fn done(&self) -> BTreeSet<K> {
        std::fs::read_to_string(&self.path)
            .map(|s| {
                s.lines()
                    .filter(|l| !l.trim().is_empty())
                    .filter_map(|l| serde_json::from_str::<K>(l).ok())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Record one completed key. Append-only (never rewrites or dedups the
    /// file on write) — durable BEFORE the caller's next unit of work starts.
    /// The CALLER is responsible for only calling this AFTER the work `key`
    /// represents is itself durably persisted elsewhere (e.g. DB rows); this
    /// type has no visibility into that and cannot enforce the ordering
    /// itself, only preserve it once the caller does.
    ///
    /// A key marked twice (e.g. a resumed unit re-marked) simply appears
    /// twice on disk; [`Self::done`]'s `BTreeSet` collapses duplicates on
    /// read, so double-marking is harmless.
    pub fn mark(&self, key: &K) -> Result<(), String> {
        let line =
            serde_json::to_string(key).map_err(|e| format!("serialize checkpoint key: {e}"))?;
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|e| format!("open checkpoint {}: {e}", self.path))?;
        writeln!(f, "{line}").map_err(|e| format!("append checkpoint: {e}"))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use std::sync::Arc;
    use std::thread;

    #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
    struct TestKey {
        model: String,
        tag: String,
    }

    fn tmp_path(name: &str) -> String {
        let dir = std::env::temp_dir();
        format!(
            "{}/terminus-checkpoint-test-{name}-{}-{}.jsonl",
            dir.display(),
            std::process::id(),
            name
        )
    }

    #[test]
    fn missing_file_reads_back_as_empty() {
        let path = tmp_path("missing");
        let _ = std::fs::remove_file(&path);
        let cp: FileCheckpoint<TestKey> = FileCheckpoint::at(&path);
        assert!(cp.done().is_empty());
    }

    #[test]
    fn mark_then_done_roundtrips() {
        let path = tmp_path("roundtrip");
        let _ = std::fs::remove_file(&path);
        let cp: FileCheckpoint<TestKey> = FileCheckpoint::at(&path);

        let k1 = TestKey { model: "m1".into(), tag: "gpu".into() };
        let k2 = TestKey { model: "m2".into(), tag: "cpu".into() };
        cp.mark(&k1).unwrap();
        cp.mark(&k2).unwrap();

        let done = cp.done();
        assert!(done.contains(&k1));
        assert!(done.contains(&k2));
        assert_eq!(done.len(), 2);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn on_disk_format_is_json_lines() {
        // Exact on-disk format must stay JSON-lines (one JSON object per
        // line) — this is what makes a reboot resume rather than restart:
        // a fresh process must be able to parse each line independently.
        let path = tmp_path("jsonlines");
        let _ = std::fs::remove_file(&path);
        let cp: FileCheckpoint<TestKey> = FileCheckpoint::at(&path);
        cp.mark(&TestKey { model: "m1".into(), tag: "gpu".into() }).unwrap();
        cp.mark(&TestKey { model: "m2".into(), tag: "cpu".into() }).unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = raw.lines().collect();
        assert_eq!(lines.len(), 2);
        for line in &lines {
            let parsed: TestKey = serde_json::from_str(line).expect("each line is standalone JSON");
            assert!(parsed.model == "m1" || parsed.model == "m2");
        }

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn mark_is_append_only_never_rewrites() {
        let path = tmp_path("append-only");
        let _ = std::fs::remove_file(&path);
        let cp: FileCheckpoint<TestKey> = FileCheckpoint::at(&path);
        let k = TestKey { model: "m1".into(), tag: "gpu".into() };
        cp.mark(&k).unwrap();
        cp.mark(&k).unwrap(); // double-mark (e.g. a resumed unit re-marked)

        let raw = std::fs::read_to_string(&path).unwrap();
        assert_eq!(raw.lines().count(), 2, "mark never rewrites/dedups the file itself");

        // Dedup happens on READ, in the BTreeSet done() returns.
        let done = cp.done();
        assert_eq!(done.len(), 1);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn reload_after_partial_write_skips_unparseable_lines() {
        // Simulates a crash mid-write leaving a truncated/garbage line: it
        // must be skipped, not fail the whole read (a fresh process resuming
        // from this file must still see every OTHER, well-formed key).
        let path = tmp_path("partial");
        let _ = std::fs::remove_file(&path);
        let cp: FileCheckpoint<TestKey> = FileCheckpoint::at(&path);
        cp.mark(&TestKey { model: "good".into(), tag: "gpu".into() }).unwrap();
        // Append a garbage line directly (bypassing `mark`).
        {
            use std::io::Write as _;
            let mut f = OpenOptions::new().append(true).open(&path).unwrap();
            writeln!(f, "{{not valid json").unwrap();
        }
        cp.mark(&TestKey { model: "also-good".into(), tag: "cpu".into() }).unwrap();

        let done = cp.done();
        assert_eq!(done.len(), 2);
        assert!(done.contains(&TestKey { model: "good".into(), tag: "gpu".into() }));
        assert!(done.contains(&TestKey { model: "also-good".into(), tag: "cpu".into() }));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn concurrent_marks_from_multiple_threads_all_land() {
        // Concurrent-safety expectation: neither original implementation had
        // any explicit locking, relying on the OS's append-mode `O_APPEND`
        // writes (atomic per `write(2)` syscall for a single short line on a
        // local filesystem) to keep concurrent small appends from
        // interleaving mid-line. This test proves that expectation holds for
        // this shared implementation: N threads each marking a DISTINCT key
        // must all be durably readable back afterward, with no line
        // corruption dropping/merging any of them.
        let path = tmp_path("concurrent");
        let _ = std::fs::remove_file(&path);
        let cp: Arc<FileCheckpoint<TestKey>> = Arc::new(FileCheckpoint::at(&path));

        let handles: Vec<_> = (0..16)
            .map(|i| {
                let cp = Arc::clone(&cp);
                thread::spawn(move || {
                    let key = TestKey {
                        model: format!("m{i}"),
                        tag: "gpu".into(),
                    };
                    cp.mark(&key).unwrap();
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        let done = cp.done();
        assert_eq!(done.len(), 16, "every concurrently-marked key must survive intact");
        for i in 0..16 {
            assert!(done.contains(&TestKey { model: format!("m{i}"), tag: "gpu".into() }));
        }

        let _ = std::fs::remove_file(&path);
    }
}
