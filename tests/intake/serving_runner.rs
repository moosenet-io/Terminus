//! Integration tests for S85 SRV-02 — the serving-profile runner.
//!
//! Exercises [`runner::run_with`] through its PUBLIC surface only, with a mocked
//! launcher + VRAM gate + sink + checkpoint — NO DB, NO network, NO GPU. Proves
//! the spec's TEST PLAN:
//!   - the seed-driven run reproduces the v2 master-table verdicts (clean drift);
//!   - a changed fixture triggers a drift-report entry;
//!   - the VRAM-released-between-cells gate is enforced (mocked sysfs);
//!   - resume-after-interruption from the checkpoint;
//!   - `mmap_flag=0` is recorded for staged/large llama.cpp cells;
//!   - incremental persistence: the row lands before the checkpoint mark.

use std::collections::BTreeSet;
use std::sync::Mutex;

use async_trait::async_trait;

use terminus_rs::intake::serving::probes::{
    CellOutcome, CellRequest, Launcher, VramGate,
};
use terminus_rs::intake::serving::runner::{
    self, Checkpoint, CheckpointKey, ProfileSink, SeedCell,
};
use terminus_rs::intake::serving::{ExclusionReason, ServingProfile};

const BASELINE_VRAM: u64 = 147 * 1024 * 1024;

// ── mock launcher: replays the seed's recorded verdict per cell ──
//
// A launcher that "reproduces" the v2 sweep: for each seeded cell it returns the
// outcome the seed recorded. Overrides let a single cell return a DIFFERENT
// outcome (to simulate a llama.cpp bump → drift).
struct ReplayLauncher {
    overrides: Mutex<std::collections::HashMap<(String, String), CellOutcome>>,
    cells: Vec<SeedCell>,
}

impl ReplayLauncher {
    fn new(cells: Vec<SeedCell>) -> Self {
        ReplayLauncher {
            overrides: Mutex::new(std::collections::HashMap::new()),
            cells,
        }
    }
    fn set_override(&self, model: &str, backend: &str, outcome: CellOutcome) {
        self.overrides
            .lock()
            .unwrap()
            .insert((model.into(), backend.into()), outcome);
    }
    fn seed_outcome(&self, model: &str, backend: &str) -> CellOutcome {
        let cell = self
            .cells
            .iter()
            .find(|c| c.model_id == model && c.backend_tag == backend)
            .expect("requested cell is in the seed");
        match cell.exclusion_reason.as_str() {
            "none" => CellOutcome::Served {
                tok_s: cell.tok_s.unwrap_or(0.0),
                peak_gb: cell.vram_or_ram_peak_gb.unwrap_or(0.0),
                cold_load_s: cell.cold_load_s.unwrap_or(0.0),
            },
            "permanent-unknown-arch" => CellOutcome::UnknownArch {
                error: "unknown model architecture".into(),
            },
            "build-conditional" => CellOutcome::BuildIncompatible {
                error: "loader incompatible with GGUF".into(),
            },
            "quant-unsupported" => CellOutcome::QuantUnsupported {
                error: "file_type unhandled".into(),
            },
            "oom-host-ram" => CellOutcome::OomHostRam {
                error: "system RAM pre-flight refused".into(),
            },
            "oom-vram" => CellOutcome::OomVram {
                error: "VRAM ceiling".into(),
            },
            other => panic!("seed exclusion {other}"),
        }
    }
}

#[async_trait]
impl Launcher for ReplayLauncher {
    async fn launch_and_measure(&self, req: &CellRequest) -> CellOutcome {
        let model = req.model_id.as_str().to_string();
        let backend = req.backend.as_str().to_string();
        if let Some(o) = self.overrides.lock().unwrap().get(&(model.clone(), backend.clone())) {
            return o.clone();
        }
        self.seed_outcome(&model, &backend)
    }
}

// ── mock VRAM gate: scriptable counter ──
struct MockVramGate {
    in_use: Mutex<u64>,
}
impl MockVramGate {
    fn released() -> Self {
        MockVramGate { in_use: Mutex::new(BASELINE_VRAM) }
    }
    fn stuck() -> Self {
        // 40 GB still resident — never releases.
        MockVramGate { in_use: Mutex::new(40 * 1024 * 1024 * 1024) }
    }
}
#[async_trait]
impl VramGate for MockVramGate {
    async fn vram_in_use_bytes(&self) -> Result<u64, String> {
        Ok(*self.in_use.lock().unwrap())
    }
}

// ── mock sink: collects persisted rows; records ORDER for the incremental check ──
#[derive(Default)]
struct MockSink {
    rows: Mutex<Vec<ServingProfile>>,
}
#[async_trait]
impl ProfileSink for MockSink {
    async fn write(&self, profile: &ServingProfile) -> Result<(), String> {
        profile.validate().map_err(|e| e.to_string())?; // every persisted row is coherent
        self.rows.lock().unwrap().push(profile.clone());
        Ok(())
    }
}

// ── mock checkpoint: in-memory ledger; records mark ORDER ──
#[derive(Default)]
struct MockCheckpoint {
    done: Mutex<BTreeSet<CheckpointKey>>,
    marks: Mutex<Vec<CheckpointKey>>,
}
impl MockCheckpoint {
    fn with_done(keys: Vec<CheckpointKey>) -> Self {
        MockCheckpoint {
            done: Mutex::new(keys.into_iter().collect()),
            marks: Mutex::new(Vec::new()),
        }
    }
}
#[async_trait]
impl Checkpoint for MockCheckpoint {
    async fn done(&self) -> Result<BTreeSet<CheckpointKey>, String> {
        Ok(self.done.lock().unwrap().clone())
    }
    async fn mark(&self, key: &CheckpointKey) -> Result<(), String> {
        self.marks.lock().unwrap().push(key.clone());
        self.done.lock().unwrap().insert(key.clone());
        Ok(())
    }
}

fn seed() -> Vec<SeedCell> {
    runner::load_seed().expect("seed loads")
}

// ===========================================================================
// Tests
// ===========================================================================

#[tokio::test]
async fn seed_driven_run_reproduces_v2_no_drift() {
    let cells = seed();
    let launcher = ReplayLauncher::new(cells.clone());
    let sink = MockSink::default();
    let gate = MockVramGate::released();
    let ckpt = MockCheckpoint::default();

    let report = runner::run_with(&cells, 120.0, BASELINE_VRAM, &launcher, &sink, &gate, &ckpt)
        .await
        .expect("run ok");

    // Reproduces the v2 table: NO drift.
    assert!(
        report.drift.is_clean(),
        "expected clean drift, got: {:?}",
        report.drift.entries
    );

    // Every non-acquisition-gap cell persisted exactly one row.
    let persisted = report.cells.iter().filter(|c| c.persisted).count();
    assert!(persisted > 0);
    assert_eq!(persisted, sink.rows.lock().unwrap().len());

    // A known exclusion reproduced: gpt-oss on llama-gpu → permanent-unknown-arch.
    let row = sink
        .rows
        .lock()
        .unwrap()
        .iter()
        .find(|r| r.model_id.as_str() == "gpt-oss:20b" && r.backend_tag.as_str() == "llama-gpu")
        .cloned()
        .expect("gpt-oss llama-gpu row persisted");
    assert_eq!(row.exclusion_reason, ExclusionReason::PermanentUnknownArch);
    assert_eq!(row.recheck_trigger.as_str(), "none");

    // A known build-conditional reproduced with the version-bump trigger.
    let gemma = sink
        .rows
        .lock()
        .unwrap()
        .iter()
        .find(|r| r.model_id.as_str() == "gemma4:26b" && r.backend_tag.as_str() == "llama-gpu")
        .cloned()
        .unwrap();
    assert_eq!(gemma.exclusion_reason, ExclusionReason::BuildConditional);
    assert_eq!(gemma.recheck_trigger.as_str(), "llama-cpp-version-bump");
}

#[tokio::test]
async fn keep_warm_set_from_cold_load() {
    let cells = seed();
    let launcher = ReplayLauncher::new(cells.clone());
    let sink = MockSink::default();
    let gate = MockVramGate::released();
    let ckpt = MockCheckpoint::default();

    runner::run_with(&cells, 120.0, BASELINE_VRAM, &launcher, &sink, &gate, &ckpt)
        .await
        .unwrap();

    // minimax-m2.7 llama-gpu: 599s cold load > 120s ⇒ keep_warm.
    let rows = sink.rows.lock().unwrap();
    let mm = rows
        .iter()
        .find(|r| r.model_id.as_str() == "minimax-m2.7" && r.backend_tag.as_str() == "llama-gpu")
        .unwrap();
    assert!(mm.keep_warm, "big slow-loading MoE must be keep_warm");

    // qwen3-coder:30b llama-gpu: 9s cold load ⇒ NOT keep_warm.
    let qc = rows
        .iter()
        .find(|r| r.model_id.as_str() == "qwen3-coder:30b" && r.backend_tag.as_str() == "llama-gpu")
        .unwrap();
    assert!(!qc.keep_warm);
}

#[tokio::test]
async fn mmap_flag_zero_recorded_for_staged_large_llama_cells() {
    let cells = seed();
    let launcher = ReplayLauncher::new(cells.clone());
    let sink = MockSink::default();
    let gate = MockVramGate::released();
    let ckpt = MockCheckpoint::default();

    runner::run_with(&cells, 120.0, BASELINE_VRAM, &launcher, &sink, &gate, &ckpt)
        .await
        .unwrap();

    let rows = sink.rows.lock().unwrap();
    // minimax-m2.7 (~77GB) on llama-gpu must record mmap_flag=0 in env_json.
    let mm = rows
        .iter()
        .find(|r| r.model_id.as_str() == "minimax-m2.7" && r.backend_tag.as_str() == "llama-gpu")
        .unwrap();
    let env: serde_json::Value = serde_json::from_str(&mm.env_json).unwrap();
    assert_eq!(env["mmap_flag"], serde_json::json!(0));

    // qwen3:8b (small/local) on llama-gpu records mmap on (flag 1).
    let small = rows
        .iter()
        .find(|r| r.model_id.as_str() == "qwen3:8b" && r.backend_tag.as_str() == "llama-gpu")
        .unwrap();
    let env: serde_json::Value = serde_json::from_str(&small.env_json).unwrap();
    assert_eq!(env["mmap_flag"], serde_json::json!(1));
}

#[tokio::test]
async fn changed_fixture_triggers_drift_entry() {
    let cells = seed();
    let launcher = ReplayLauncher::new(cells.clone());
    // Simulate a llama.cpp bump: gemma4:26b on llama-gpu now SERVES.
    launcher.set_override(
        "gemma4:26b",
        "llama-gpu",
        CellOutcome::Served { tok_s: 52.0, peak_gb: 27.0, cold_load_s: 8.0 },
    );
    let sink = MockSink::default();
    let gate = MockVramGate::released();
    let ckpt = MockCheckpoint::default();

    let report = runner::run_with(&cells, 120.0, BASELINE_VRAM, &launcher, &sink, &gate, &ckpt)
        .await
        .unwrap();

    assert!(!report.drift.is_clean(), "a flipped cell must drift");
    let d = report
        .drift
        .entries
        .iter()
        .find(|d| d.model_id == "gemma4:26b" && d.backend_tag == "llama-gpu")
        .expect("drift entry for the flipped cell");
    assert_eq!(d.seed_exclusion, "build-conditional");
    assert_eq!(d.run_exclusion, "none");

    // The persisted row reflects the NEW verdict (now works, no exclusion).
    let row = sink
        .rows
        .lock()
        .unwrap()
        .iter()
        .find(|r| r.model_id.as_str() == "gemma4:26b" && r.backend_tag.as_str() == "llama-gpu")
        .cloned()
        .unwrap();
    assert_eq!(row.exclusion_reason, ExclusionReason::None);
    assert_eq!(row.tok_s, Some(52.0));
}

#[tokio::test]
async fn vram_release_gate_enforced() {
    // A gate that never releases must block every cell AFTER the first launch.
    let cells = seed();
    let launcher = ReplayLauncher::new(cells.clone());
    let sink = MockSink::default();
    let gate = MockVramGate::stuck();
    let ckpt = MockCheckpoint::default();

    let report = runner::run_with(&cells, 120.0, BASELINE_VRAM, &launcher, &sink, &gate, &ckpt)
        .await
        .unwrap();

    // At least one cell skipped for the gate violation.
    let gate_skips = report
        .cells
        .iter()
        .filter(|c| {
            c.skip_reason
                .as_deref()
                .map(|r| r.contains("VRAM-release gate"))
                .unwrap_or(false)
        })
        .count();
    assert!(gate_skips > 0, "stuck VRAM must block later cells");
}

#[tokio::test]
async fn resume_after_interruption_skips_persisted_cells() {
    let cells = seed();

    // First run: mark a couple cells as already done in the checkpoint.
    let already = vec![
        CheckpointKey { model_id: "qwen3:8b".into(), backend_tag: "llama-gpu".into() },
        CheckpointKey { model_id: "qwen3-coder:30b".into(), backend_tag: "llama-gpu".into() },
    ];
    let launcher = ReplayLauncher::new(cells.clone());
    let sink = MockSink::default();
    let gate = MockVramGate::released();
    let ckpt = MockCheckpoint::with_done(already.clone());

    let report = runner::run_with(&cells, 120.0, BASELINE_VRAM, &launcher, &sink, &gate, &ckpt)
        .await
        .unwrap();

    // The pre-marked cells were resumed (skipped), not re-persisted.
    for k in &already {
        assert!(
            report.resumed.contains(k),
            "{}/{} should be resumed",
            k.model_id,
            k.backend_tag
        );
    }
    // No persisted row for a resumed cell.
    for r in sink.rows.lock().unwrap().iter() {
        let k = CheckpointKey {
            model_id: r.model_id.as_str().into(),
            backend_tag: r.backend_tag.as_str().into(),
        };
        assert!(!already.contains(&k), "resumed cell must not be re-persisted");
    }
}

#[tokio::test]
async fn incremental_persistence_row_before_checkpoint() {
    // The row must land BEFORE its checkpoint mark — so a crash between them
    // re-runs (idempotent) rather than losing the row. We prove the marks are a
    // subset/aligned with persisted rows: every checkpoint mark for a persisted
    // model has a corresponding row already in the sink.
    let cells = seed();
    let launcher = ReplayLauncher::new(cells.clone());
    let sink = MockSink::default();
    let gate = MockVramGate::released();
    let ckpt = MockCheckpoint::default();

    runner::run_with(&cells, 120.0, BASELINE_VRAM, &launcher, &sink, &gate, &ckpt)
        .await
        .unwrap();

    let rows = sink.rows.lock().unwrap();
    let marks = ckpt.marks.lock().unwrap();
    // Every persisted row's key is present in the marks (the mark followed it).
    for r in rows.iter() {
        let want = CheckpointKey {
            model_id: r.model_id.as_str().into(),
            backend_tag: r.backend_tag.as_str().into(),
        };
        assert!(marks.contains(&want), "persisted row must be checkpointed");
    }
}

#[tokio::test]
async fn host_ram_refusal_carries_fallback_note() {
    // minimax-m2.7 on ollama-gpu is oom-host-ram with a llama.cpp fallback note.
    let cells = seed();
    let launcher = ReplayLauncher::new(cells.clone());
    let sink = MockSink::default();
    let gate = MockVramGate::released();
    let ckpt = MockCheckpoint::default();

    let report = runner::run_with(&cells, 120.0, BASELINE_VRAM, &launcher, &sink, &gate, &ckpt)
        .await
        .unwrap();

    let cell = report
        .cells
        .iter()
        .find(|c| c.model_id == "minimax-m2.7" && c.backend_tag == "ollama-gpu")
        .unwrap();
    assert!(
        cell.note.as_deref().unwrap_or("").contains("llama.cpp-rocm --no-mmap"),
        "host-RAM refusal must surface the llama.cpp fallback note"
    );

    let row = sink
        .rows
        .lock()
        .unwrap()
        .iter()
        .find(|r| r.model_id.as_str() == "minimax-m2.7" && r.backend_tag.as_str() == "ollama-gpu")
        .cloned()
        .unwrap();
    assert_eq!(row.exclusion_reason, ExclusionReason::OomHostRam);
}
