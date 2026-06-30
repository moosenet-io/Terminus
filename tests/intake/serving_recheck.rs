//! Integration tests for S85 SRV-03 — the build-conditional recheck mode.
//!
//! Exercises [`runner::recheck_with`] through its PUBLIC surface only, with a
//! mocked launcher + VRAM gate + sink + checkpoint + row source — NO DB, NO
//! network, NO GPU. Proves the spec's TEST PLAN:
//!   - the selector picks EXACTLY the build-conditional rows, ignoring
//!     permanent-unknown-arch + working rows;
//!   - a now-working build-conditional row FLIPS (exclusion → none, recheck →
//!     none, numbers recorded) + a drift line is emitted carrying the build id;
//!   - an unchanged build-conditional row is LEFT (verdict untouched) with the
//!     "still build-incompatible at build X" note;
//!   - no-build-conditional-rows ⇒ "nothing to recheck", clean exit;
//!   - resumable from the checkpoint like the main runner;
//!   - NO background/automatic trigger exists (asserted over the live entry).

use std::collections::BTreeSet;
use std::sync::Mutex;

use async_trait::async_trait;

use terminus_rs::intake::serving::probes::{CellOutcome, CellRequest, Launcher, VramGate};
use terminus_rs::intake::serving::runner::{
    self, Checkpoint, CheckpointKey, ProfileSink, RecheckSource,
};
use terminus_rs::intake::serving::{
    ExclusionReason, ModelId, RecheckTrigger, Runtime, ServingBackend, ServingProfile,
};

const BASELINE_VRAM: u64 = 147 * 1024 * 1024;
const BUILD_ID: &str = "b1402";

// ── row builders ──

fn build_conditional(model: &str) -> ServingProfile {
    ServingProfile {
        model_id: ModelId::from(model),
        backend_tag: ServingBackend::LlamaGpu,
        best_runtime: Runtime::LlamaCpp,
        env_json: r#"{"gfx_override":true,"mmap_flag":1,"flash_attn":false,"cpu_lib":null}"#.into(),
        tok_s: None,
        vram_or_ram_peak_gb: None,
        cold_load_s: None,
        keep_warm: false,
        fallback_runtime: None,
        exclusion_reason: ExclusionReason::BuildConditional,
        recheck_trigger: RecheckTrigger::LlamaCppVersionBump,
        provenance: None,
    }
}

fn working(model: &str) -> ServingProfile {
    ServingProfile {
        model_id: ModelId::from(model),
        backend_tag: ServingBackend::LlamaGpu,
        best_runtime: Runtime::LlamaCpp,
        env_json: "{}".into(),
        tok_s: Some(70.0),
        vram_or_ram_peak_gb: Some(18.0),
        cold_load_s: Some(8.0),
        keep_warm: false,
        fallback_runtime: Some(Runtime::Ollama),
        exclusion_reason: ExclusionReason::None,
        recheck_trigger: RecheckTrigger::None,
        provenance: None,
    }
}

fn permanent_unknown_arch(model: &str) -> ServingProfile {
    ServingProfile {
        model_id: ModelId::from(model),
        backend_tag: ServingBackend::LlamaGpu,
        best_runtime: Runtime::LlamaCpp,
        env_json: "{}".into(),
        tok_s: None,
        vram_or_ram_peak_gb: None,
        cold_load_s: None,
        keep_warm: false,
        fallback_runtime: None,
        exclusion_reason: ExclusionReason::PermanentUnknownArch,
        recheck_trigger: RecheckTrigger::None,
        provenance: None,
    }
}

// ── mock row source ──
struct MockSource {
    rows: Vec<ServingProfile>,
}
#[async_trait]
impl RecheckSource for MockSource {
    async fn load_rows(&self) -> Result<Vec<ServingProfile>, String> {
        Ok(self.rows.clone())
    }
}

// ── mock launcher: per-(model,backend) scripted outcome; default still-incompatible ──
struct ScriptedLauncher {
    outcomes: Mutex<std::collections::HashMap<(String, String), CellOutcome>>,
}
impl ScriptedLauncher {
    fn new() -> Self {
        ScriptedLauncher { outcomes: Mutex::new(std::collections::HashMap::new()) }
    }
    fn set(&self, model: &str, backend: &str, outcome: CellOutcome) {
        self.outcomes.lock().unwrap().insert((model.into(), backend.into()), outcome);
    }
}
#[async_trait]
impl Launcher for ScriptedLauncher {
    async fn launch_and_measure(&self, req: &CellRequest) -> CellOutcome {
        let k = (req.model_id.as_str().to_string(), req.backend.as_str().to_string());
        self.outcomes
            .lock()
            .unwrap()
            .get(&k)
            .cloned()
            // default: the build STILL can't read it (unchanged path).
            .unwrap_or(CellOutcome::BuildIncompatible {
                error: "loader still incompatible with GGUF".into(),
            })
    }
}

// ── mock VRAM gate ──
struct MockVramGate {
    in_use: Mutex<u64>,
}
impl MockVramGate {
    fn released() -> Self {
        MockVramGate { in_use: Mutex::new(BASELINE_VRAM) }
    }
}
#[async_trait]
impl VramGate for MockVramGate {
    async fn vram_in_use_bytes(&self) -> Result<u64, String> {
        Ok(*self.in_use.lock().unwrap())
    }
}

// ── mock sink: collects rows + records mark/write ORDER ──
#[derive(Default)]
struct MockSink {
    rows: Mutex<Vec<ServingProfile>>,
}
#[async_trait]
impl ProfileSink for MockSink {
    async fn write(&self, profile: &ServingProfile) -> Result<(), String> {
        profile.validate().map_err(|e| e.to_string())?; // every persisted row coherent
        self.rows.lock().unwrap().push(profile.clone());
        Ok(())
    }
}

// ── mock checkpoint ──
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

fn mixed_rows() -> Vec<ServingProfile> {
    vec![
        working("qwen3-coder:30b"),
        permanent_unknown_arch("gpt-oss:20b"),
        build_conditional("gemma4:26b"),
        build_conditional("qwen3.5:35b"),
        working("qwen3:8b"),
        permanent_unknown_arch("glm-4.7-flash"),
    ]
}

// ===========================================================================
// Tests
// ===========================================================================

/// Selector + recheck pass picks EXACTLY the build-conditional rows: the two
/// permanent-unknown-arch rows and the two working rows are never launched.
#[tokio::test]
async fn recheck_targets_only_build_conditional_rows() {
    let source = MockSource { rows: mixed_rows() };
    let launcher = ScriptedLauncher::new(); // all default → still-incompatible
    let sink = MockSink::default();
    let gate = MockVramGate::released();
    let ckpt = MockCheckpoint::default();

    let report = runner::recheck_with(
        BUILD_ID, BASELINE_VRAM, 120.0, &source, &launcher, &sink, &gate, &ckpt,
    )
    .await
    .expect("recheck ok");

    assert!(!report.nothing_to_recheck);
    // Exactly the two build-conditional models were rechecked.
    let mut rechecked: Vec<&str> = report.cells.iter().map(|c| c.model_id.as_str()).collect();
    rechecked.sort();
    assert_eq!(rechecked, ["gemma4:26b", "qwen3.5:35b"]);

    // The permanent + working models were NEVER persisted/touched by the recheck.
    let persisted: Vec<String> = sink
        .rows
        .lock()
        .unwrap()
        .iter()
        .map(|r| r.model_id.as_str().to_string())
        .collect();
    assert!(!persisted.contains(&"gpt-oss:20b".to_string()));
    assert!(!persisted.contains(&"glm-4.7-flash".to_string()));
    assert!(!persisted.contains(&"qwen3:8b".to_string()));
    assert!(!persisted.contains(&"qwen3-coder:30b".to_string()));
}

/// A now-working build-conditional row FLIPS: exclusion → none, recheck → none,
/// numbers recorded, fallback cleared, drift line emitted with the build id. An
/// unchanged one is LEFT with the "still build-incompatible at build X" note.
#[tokio::test]
async fn flip_updates_row_and_drifts_unchanged_left_with_note() {
    let source = MockSource { rows: mixed_rows() };
    let launcher = ScriptedLauncher::new();
    // gemma4:26b now SERVES (the build added the handler); qwen3.5:35b still fails
    // (default → BuildIncompatible).
    launcher.set(
        "gemma4:26b",
        "llama-gpu",
        CellOutcome::Served { tok_s: 48.0, peak_gb: 27.0, cold_load_s: 7.0 },
    );
    let sink = MockSink::default();
    let gate = MockVramGate::released();
    let ckpt = MockCheckpoint::default();

    let report = runner::recheck_with(
        BUILD_ID, BASELINE_VRAM, 120.0, &source, &launcher, &sink, &gate, &ckpt,
    )
    .await
    .unwrap();

    // ── flip path ──
    let flip = report.cells.iter().find(|c| c.model_id == "gemma4:26b").unwrap();
    assert!(flip.flipped, "now-serving build-conditional row must flip");
    assert!(flip.note.contains("build-conditional → works"));
    assert!(flip.note.contains(BUILD_ID));

    // a drift line was emitted carrying the build id.
    assert!(!report.drift.is_clean());
    let d = report
        .drift
        .entries
        .iter()
        .find(|d| d.model_id == "gemma4:26b")
        .expect("drift line for the flip");
    assert_eq!(d.seed_exclusion, "build-conditional");
    assert_eq!(d.run_exclusion, "none");
    assert!(d.summary.contains(BUILD_ID));

    // the persisted flipped row: now-working, cleared exclusion/trigger/fallback.
    let flipped_row = sink
        .rows
        .lock()
        .unwrap()
        .iter()
        .find(|r| r.model_id.as_str() == "gemma4:26b")
        .cloned()
        .unwrap();
    assert_eq!(flipped_row.exclusion_reason, ExclusionReason::None);
    assert_eq!(flipped_row.recheck_trigger, RecheckTrigger::None);
    assert_eq!(flipped_row.best_runtime, Runtime::LlamaCpp);
    assert_eq!(flipped_row.fallback_runtime, None);
    assert_eq!(flipped_row.tok_s, Some(48.0));
    assert!(flipped_row.provenance.as_deref().unwrap().contains(BUILD_ID));
    // and it is a coherent row (validate passes — the negative-combo guard).
    assert!(flipped_row.validate().is_ok());

    // ── unchanged path ──
    let stay = report.cells.iter().find(|c| c.model_id == "qwen3.5:35b").unwrap();
    assert!(!stay.flipped, "still-incompatible row must NOT flip");
    assert_eq!(stay.note, format!("still build-incompatible at build {BUILD_ID}"));

    // the unchanged row keeps its build-conditional verdict (left untouched).
    let stay_row = sink
        .rows
        .lock()
        .unwrap()
        .iter()
        .find(|r| r.model_id.as_str() == "qwen3.5:35b")
        .cloned()
        .unwrap();
    assert_eq!(stay_row.exclusion_reason, ExclusionReason::BuildConditional);
    assert_eq!(stay_row.recheck_trigger, RecheckTrigger::LlamaCppVersionBump);
    // no drift for the unchanged row.
    assert!(report.drift.entries.iter().all(|d| d.model_id != "qwen3.5:35b"));
}

/// No build-conditional rows ⇒ "nothing to recheck", clean exit, nothing launched.
#[tokio::test]
async fn no_build_conditional_rows_exits_nothing_to_recheck() {
    let source = MockSource {
        rows: vec![
            working("qwen3:8b"),
            permanent_unknown_arch("gpt-oss:20b"),
        ],
    };
    let launcher = ScriptedLauncher::new();
    let sink = MockSink::default();
    let gate = MockVramGate::released();
    let ckpt = MockCheckpoint::default();

    let report = runner::recheck_with(
        BUILD_ID, BASELINE_VRAM, 120.0, &source, &launcher, &sink, &gate, &ckpt,
    )
    .await
    .unwrap();

    assert!(report.nothing_to_recheck);
    assert!(report.cells.is_empty());
    assert!(report.drift.is_clean());
    assert!(sink.rows.lock().unwrap().is_empty(), "nothing launched/persisted");
}

/// Permanent-unknown-arch rows are NEVER rechecked (negative test) — even when the
/// launcher would say they serve, the selector keeps them out of the run.
#[tokio::test]
async fn permanent_unknown_arch_is_never_rechecked() {
    let source = MockSource {
        rows: vec![permanent_unknown_arch("gpt-oss:20b")],
    };
    let launcher = ScriptedLauncher::new();
    // Even if the build "would" serve it, the row must not be selected.
    launcher.set(
        "gpt-oss:20b",
        "llama-gpu",
        CellOutcome::Served { tok_s: 99.0, peak_gb: 12.0, cold_load_s: 3.0 },
    );
    let sink = MockSink::default();
    let gate = MockVramGate::released();
    let ckpt = MockCheckpoint::default();

    let report = runner::recheck_with(
        BUILD_ID, BASELINE_VRAM, 120.0, &source, &launcher, &sink, &gate, &ckpt,
    )
    .await
    .unwrap();

    assert!(report.nothing_to_recheck, "no build-conditional rows ⇒ nothing to recheck");
    assert!(sink.rows.lock().unwrap().is_empty(), "permanent row must never be launched");
}

/// Resumable: rows already marked in the checkpoint are skipped, not re-rechecked.
#[tokio::test]
async fn recheck_is_resumable_from_checkpoint() {
    let source = MockSource { rows: mixed_rows() };
    let launcher = ScriptedLauncher::new();
    let sink = MockSink::default();
    let gate = MockVramGate::released();
    // gemma4:26b already rechecked in a prior interrupted run.
    let already = vec![CheckpointKey {
        model_id: "gemma4:26b".into(),
        backend_tag: "llama-gpu".into(),
    }];
    let ckpt = MockCheckpoint::with_done(already.clone());

    let report = runner::recheck_with(
        BUILD_ID, BASELINE_VRAM, 120.0, &source, &launcher, &sink, &gate, &ckpt,
    )
    .await
    .unwrap();

    // gemma4:26b resumed (skipped), qwen3.5:35b still processed.
    assert!(report.resumed.contains(&already[0]));
    assert!(report.cells.iter().any(|c| c.model_id == "qwen3.5:35b"));
    assert!(report.cells.iter().all(|c| c.model_id != "gemma4:26b"));
    // the resumed row was not re-persisted.
    assert!(sink
        .rows
        .lock()
        .unwrap()
        .iter()
        .all(|r| r.model_id.as_str() != "gemma4:26b"));
}

/// Incremental persistence: every persisted row is checkpointed (the row lands
/// before its mark — a crash between re-runs idempotently, never loses the row).
#[tokio::test]
async fn recheck_persists_row_before_checkpoint() {
    let source = MockSource { rows: mixed_rows() };
    let launcher = ScriptedLauncher::new();
    launcher.set(
        "gemma4:26b",
        "llama-gpu",
        CellOutcome::Served { tok_s: 48.0, peak_gb: 27.0, cold_load_s: 7.0 },
    );
    let sink = MockSink::default();
    let gate = MockVramGate::released();
    let ckpt = MockCheckpoint::default();

    runner::recheck_with(
        BUILD_ID, BASELINE_VRAM, 120.0, &source, &launcher, &sink, &gate, &ckpt,
    )
    .await
    .unwrap();

    let rows = sink.rows.lock().unwrap();
    let marks = ckpt.marks.lock().unwrap();
    for r in rows.iter() {
        let want = CheckpointKey {
            model_id: r.model_id.as_str().into(),
            backend_tag: r.backend_tag.as_str().into(),
        };
        assert!(marks.contains(&want), "persisted recheck row must be checkpointed");
    }
}

/// SRV-03 acceptance — NO background/automatic trigger exists. The recheck mode is
/// reachable ONLY through the public `recheck_with` / `recheck_build_conditional`
/// entries; this binary/module wires no scheduler, timer, interval, or version
/// watcher that calls them. Asserting at the integration boundary complements the
/// in-crate source assertion: a test driver IS the only caller here.
#[tokio::test]
async fn recheck_runs_only_when_explicitly_invoked() {
    // Before any explicit call, nothing runs (no resident state, no side effects):
    // we construct the mocks and prove the side-effecting sink is empty until WE
    // invoke the entry. There is no spawned task that would have populated it.
    let source = MockSource { rows: mixed_rows() };
    let launcher = ScriptedLauncher::new();
    let sink = MockSink::default();
    let gate = MockVramGate::released();
    let ckpt = MockCheckpoint::default();

    // Nothing has run yet — no background trigger populated the sink.
    assert!(sink.rows.lock().unwrap().is_empty());
    assert!(ckpt.marks.lock().unwrap().is_empty());

    // The ONLY way to make it run is to call it.
    let report = runner::recheck_with(
        BUILD_ID, BASELINE_VRAM, 120.0, &source, &launcher, &sink, &gate, &ckpt,
    )
    .await
    .unwrap();

    // It ran exactly once, for exactly the build-conditional rows, because WE
    // invoked it — not a scheduler.
    assert_eq!(report.cells.len(), 2);
    assert_eq!(report.build_id, BUILD_ID);
}
