//! Integration tests for S84 ASMT-09 — the consolidated profiling runner.
//!
//! These exercise the runner through its PUBLIC surface only ([`runner::run_with`]
//! with mocked acquisition, suite driver, score sink, checkpoint, and fleet
//! store) — no DB, no network, no GPU. They prove the spec's TEST PLAN:
//!   - nominations.json drives acquire → smoke → suite → fleet registration;
//!   - a dual-fleet model keeps SEPARATE Harmony/Lumina rows (a Lumina write
//!     never clobbers a Harmony row — the negative test);
//!   - hanging / unavailable / over-VRAM models skip with a recorded reason and
//!     the run continues;
//!   - incremental persistence allows resume after a simulated interruption.
//!
//! The live path wires the real dimension runners under the P5 backend override;
//! here deterministic mocks keep the test hermetic.

use std::collections::{BTreeSet, HashMap};
use std::sync::Mutex;

use async_trait::async_trait;

use terminus_rs::intake::assistant::acquire::{
    Acquirer, AcquisitionOutcome, Nomination, Nominations,
};
use terminus_rs::intake::assistant::fleet::{self, Fleet, FleetStore};
use terminus_rs::intake::assistant::runner::{
    self, Checkpoint, CheckpointKey, GpuLock, RunReport, ScoreSink, SuiteDriver, SUITE_DIMENSIONS,
};
use terminus_rs::intake::assistant::{BackendTag, DimensionScore, ModelId};

// ── mock acquirer: Ready unless the id is in `skip` ──

struct MockAcquirer {
    skip: BTreeSet<String>,
}
#[async_trait]
impl Acquirer for MockAcquirer {
    async fn acquire(&self, nom: &Nomination) -> AcquisitionOutcome {
        if self.skip.contains(&nom.id) {
            AcquisitionOutcome::Skipped {
                reason: "exceeds VRAM ceiling".into(),
            }
        } else {
            AcquisitionOutcome::Ready { local_path: None }
        }
    }
}

// ── mock driver: smoke ok, each dimension yields one deterministic row ──

#[derive(Default)]
struct MockDriver {
    smoke_fail: BTreeSet<String>,
}
#[async_trait]
impl SuiteDriver for MockDriver {
    async fn smoke(&self, model_id: &ModelId, _b: BackendTag, _o: &str) -> Result<(), String> {
        if self.smoke_fail.contains(model_id.as_str()) {
            Err("model hung on smoke".into())
        } else {
            Ok(())
        }
    }
    async fn run_dimension(
        &self,
        model_id: &ModelId,
        backend: BackendTag,
        _o: &str,
        dimension: &str,
        _yarn: Option<&terminus_rs::intake::assistant::acquire::YarnConfig>,
    ) -> Result<Vec<DimensionScore>, String> {
        Ok(vec![DimensionScore {
            model_id: model_id.clone(),
            backend_tag: backend,
            dimension: dimension.to_string(),
            metric: "score".into(),
            value: 4.0,
            std_dev: Some(0.5),
            judge: "panel".into(),
            low_confidence: false,
            raw_json: None,
        }])
    }
}

// ── mock checkpoint / sink ──

#[derive(Default)]
struct MemCheckpoint {
    keys: Mutex<Vec<CheckpointKey>>,
}
#[async_trait]
impl Checkpoint for MemCheckpoint {
    async fn done(&self) -> Result<BTreeSet<CheckpointKey>, String> {
        Ok(self.keys.lock().unwrap().iter().cloned().collect())
    }
    async fn mark(&self, key: &CheckpointKey) -> Result<(), String> {
        self.keys.lock().unwrap().push(key.clone());
        Ok(())
    }
}

#[derive(Default)]
struct MemSink {
    rows: Mutex<Vec<DimensionScore>>,
}
#[async_trait]
impl ScoreSink for MemSink {
    async fn write(&self, rows: &[DimensionScore]) -> Result<(), String> {
        self.rows.lock().unwrap().extend_from_slice(rows);
        Ok(())
    }
}

/// Append-only fleet store that PANICS on any clobber — the integration-level
/// proof that a Lumina write never overwrites a Harmony row. Rows are keyed on
/// the Postgres row identity `(model_id, backend_tag, dimension, metric)`.
#[derive(Default)]
struct NoClobberFleet {
    rows: Mutex<HashMap<(String, String, String, String), DimensionScore>>,
}
impl NoClobberFleet {
    fn key(r: &DimensionScore) -> (String, String, String, String) {
        (
            r.model_id.as_str().to_string(),
            r.backend_tag.as_str().to_string(),
            r.dimension.clone(),
            r.metric.clone(),
        )
    }
    fn seed_harmony(&self, model: &ModelId, backend: BackendTag) -> DimensionScore {
        let row = fleet::membership_row(model, backend, Fleet::Harmony, "S83 builder");
        self.rows.lock().unwrap().insert(Self::key(&row), row.clone());
        row
    }
    fn get(&self, k: &(String, String, String, String)) -> Option<DimensionScore> {
        self.rows.lock().unwrap().get(k).cloned()
    }
    fn count(&self) -> usize {
        self.rows.lock().unwrap().len()
    }
}
#[async_trait]
impl FleetStore for NoClobberFleet {
    async fn insert_membership(&self, row: &DimensionScore) -> Result<(), String> {
        let mut rows = self.rows.lock().unwrap();
        let key = NoClobberFleet::key(row);
        if let Some(existing) = rows.get(&key) {
            assert_eq!(
                existing, row,
                "CLOBBER: a fleet insert overwrote an existing row with a different payload"
            );
        }
        rows.insert(key, row.clone());
        Ok(())
    }
}

fn noms(json: &str) -> Nominations {
    Nominations::from_json(json).expect("nominations parse")
}

/// S86: grants immediately, zero pause — these tests exercise `run_with`'s
/// acquire/smoke/suite/fleet orchestration through its public surface, not
/// the GPU-lock fairness mechanism itself (that has dedicated unit tests in
/// `runner.rs` and `gpu_authority.rs`).
struct NoopGpuLock;
#[async_trait]
impl GpuLock for NoopGpuLock {
    async fn acquire(&self) -> Result<(), String> {
        Ok(())
    }
    fn release(&self) {}
    fn release_pause(&self) -> std::time::Duration {
        std::time::Duration::ZERO
    }
    async fn check_max_hold(&self) -> Result<bool, String> {
        Ok(false)
    }
}

fn run(
    n: &Nominations,
    acq: &dyn Acquirer,
    driver: &dyn SuiteDriver,
    sink: &MemSink,
    cp: &MemCheckpoint,
    fleet: &dyn FleetStore,
) -> RunReport {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap()
        .block_on(runner::run_with(n, acq, driver, sink, cp, fleet, &NoopGpuLock))
        .expect("run ok")
}

#[test]
fn end_to_end_acquire_smoke_suite_fleet() {
    let n = noms(
        r#"{"nominations":[{"id":"command-r:35b","size_b":35,"gfx1151_class":"confirmed","acquisition":"ollama_pull"}]}"#,
    );
    let acq = MockAcquirer { skip: BTreeSet::new() };
    let driver = MockDriver::default();
    let sink = MemSink::default();
    let cp = MemCheckpoint::default();
    let fl = NoClobberFleet::default();

    let report = run(&n, &acq, &driver, &sink, &cp, &fl);

    // 6 dims × 2 backends persisted + checkpointed.
    assert_eq!(sink.rows.lock().unwrap().len(), 12);
    assert_eq!(cp.keys.lock().unwrap().len(), 12);
    // Survived both backends → 2 Lumina fleet rows.
    assert_eq!(fl.count(), 2);
    assert!(report.models[0].backends.iter().all(|b| b.survived));
}

#[test]
fn dual_fleet_lumina_write_never_clobbers_harmony() {
    // The model is ALREADY in Harmony (S83) on both backends. After the assistant
    // run registers it into Lumina, the Harmony rows must be byte-identical and
    // BOTH fleet memberships must coexist as separate rows.
    let n = noms(
        r#"{"nominations":[{"id":"minimax:m2","size_b":40,"gfx1151_class":"confirmed","acquisition":"register_span"}]}"#,
    );
    let model = ModelId::from("minimax:m2");
    let fl = NoClobberFleet::default();
    let h_gpu = fl.seed_harmony(&model, BackendTag::Gpu);
    let h_cpu = fl.seed_harmony(&model, BackendTag::Cpu);
    let h_gpu_key = NoClobberFleet::key(&h_gpu);
    let h_cpu_key = NoClobberFleet::key(&h_cpu);
    let before_gpu = fl.get(&h_gpu_key).unwrap();
    let before_cpu = fl.get(&h_cpu_key).unwrap();

    let acq = MockAcquirer { skip: BTreeSet::new() };
    let driver = MockDriver::default();
    let sink = MemSink::default();
    let cp = MemCheckpoint::default();

    let _ = run(&n, &acq, &driver, &sink, &cp, &fl);

    // Harmony rows untouched (the NoClobberFleet would have panicked otherwise).
    assert_eq!(fl.get(&h_gpu_key).unwrap(), before_gpu);
    assert_eq!(fl.get(&h_cpu_key).unwrap(), before_cpu);
    // 2 Harmony + 2 Lumina = 4 distinct fleet rows.
    assert_eq!(fl.count(), 4);
    // Harmony stays "harmony"; Lumina rows are "lumina" on the same model/backend.
    let lumina_gpu = fl
        .get(&(
            "minimax:m2".into(),
            "gpu".into(),
            fleet::DIMENSION.into(),
            "lumina".into(),
        ))
        .expect("a Lumina gpu row exists alongside the Harmony one");
    assert_eq!(lumina_gpu.metric, "lumina");
    assert_eq!(h_gpu.metric, "harmony");
    assert_eq!(lumina_gpu.model_id, h_gpu.model_id);
    assert_eq!(lumina_gpu.backend_tag, h_gpu.backend_tag);
}

#[test]
fn over_vram_and_hang_skip_with_reason_run_continues() {
    let n = noms(
        r#"{"nominations":[
          {"id":"command-a-plus:218b","size_b":218,"gfx1151_class":"experimental","acquisition":"hf_fetch","hf_repo":"x/y"},
          {"id":"hangy:32b","size_b":32,"gfx1151_class":"experimental","acquisition":"ollama_pull"},
          {"id":"phi-4:14b","size_b":14,"gfx1151_class":"confirmed","acquisition":"ollama_pull"}
        ]}"#,
    );
    let acq = MockAcquirer {
        skip: ["command-a-plus:218b".to_string()].into_iter().collect(),
    };
    let mut driver = MockDriver::default();
    driver.smoke_fail.insert("hangy:32b".into());
    let sink = MemSink::default();
    let cp = MemCheckpoint::default();
    let fl = NoClobberFleet::default();

    let report = run(&n, &acq, &driver, &sink, &cp, &fl);

    // 1) Command A+ over VRAM → acquisition skip with reason, never profiled.
    let big = &report.models[0];
    assert!(big.acquisition_skip.as_ref().unwrap().contains("VRAM"));
    assert!(big.backends.is_empty());
    // 2) hangy hung on smoke → both backends skip-with-reason, no rows.
    let hangy = &report.models[1];
    assert!(hangy
        .backends
        .iter()
        .all(|b| b.smoke_skip.as_deref() == Some("model hung on smoke") && !b.survived));
    // 3) phi-4 ran fully — the run CONTINUED past the two skips.
    let phi = &report.models[2];
    assert!(phi.backends.iter().all(|b| b.survived));
    // Only phi-4's rows landed: 6 × 2 = 12.
    assert_eq!(sink.rows.lock().unwrap().len(), 12);
    // Only phi-4 entered the Lumina fleet (2 rows).
    assert_eq!(fl.count(), 2);
}

#[test]
fn resume_after_interruption_skips_completed_dimensions() {
    // Simulate a reboot mid-run: the gpu pass had completed dims 1-4 (their rows
    // are already in the DB and checkpointed). On resume, ONLY dims 5-6 on gpu
    // (and all of cpu) should run — completed work is not repeated.
    let n = noms(
        r#"{"nominations":[{"id":"granite:30b","size_b":30,"gfx1151_class":"confirmed","acquisition":"ollama_pull"}]}"#,
    );
    let model = ModelId::from("granite:30b");
    let cp = MemCheckpoint::default();
    for d in &SUITE_DIMENSIONS[..4] {
        cp.keys
            .lock()
            .unwrap()
            .push(CheckpointKey::new(&model, BackendTag::Gpu, d));
    }
    let acq = MockAcquirer { skip: BTreeSet::new() };
    let driver = MockDriver::default();
    let sink = MemSink::default();
    let fl = NoClobberFleet::default();

    let report = run(&n, &acq, &driver, &sink, &cp, &fl);

    let gpu = report.models[0]
        .backends
        .iter()
        .find(|b| b.backend_tag == BackendTag::Gpu)
        .unwrap();
    assert_eq!(gpu.resumed_dims.len(), 4);
    assert_eq!(gpu.persisted_dims.len(), 2); // dims 5,6 only
                                             // 2 new gpu rows + 6 cpu rows = 8 persisted this run.
    assert_eq!(sink.rows.lock().unwrap().len(), 8);
    // Survived on both backends despite the interruption → 2 fleet rows.
    assert_eq!(fl.count(), 2);
}
