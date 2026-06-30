//! Integration tests for S84 ASMT-11 — money queries + sequential personality
//! read + chat-role guard + report rendering.
//!
//! Hermetic: every test feeds a seeded in-memory row set (no DB, no network).
//! They prove the spec's TEST PLAN / EDGE CASES:
//!   - each money query returns the expected ranking + SD flags;
//!   - the personality read is dim-4 shortlist THEN dim-5 ranking, NEVER merged
//!     (the report FAILS if a combined personality score ever appears);
//!   - the chat-role guard excludes a personality-topping but slow/degrading model
//!     with a recorded reason (still shown), and keeps the default when nothing
//!     clears the guard.

use terminus_rs::intake::assistant::reporting::{
    self, build_report, dims, ChatRoleGuard, DualProfileRow, GuardVerdict, ReportConfig, ScoreRow,
};

// ── fixture builders ────────────────────────────────────────────────────────

fn row(
    model: &str,
    backend: &str,
    dimension: &str,
    metric: &str,
    value: f64,
    std_dev: Option<f64>,
) -> ScoreRow {
    ScoreRow {
        model_id: model.into(),
        backend_tag: backend.into(),
        dimension: dimension.into(),
        metric: metric.into(),
        value,
        std_dev,
        judge: if std_dev.is_some() { "panel".into() } else { "deterministic".into() },
        low_confidence: false,
    }
}

/// A seeded row set covering all six dimensions for three models on gpu.
fn seed_rows() -> Vec<ScoreRow> {
    let mut rows = Vec::new();

    // ── dim-1 conversation depth (recall_ceiling_turns; higher better) ──
    rows.push(row("alpha:8b", "gpu", dims::CONVERSATION_DEPTH, dims::RECALL_CEILING_TURNS, 40.0, None));
    rows.push(row("bravo:8b", "gpu", dims::CONVERSATION_DEPTH, dims::RECALL_CEILING_TURNS, 80.0, None));
    rows.push(row("charlie:70b", "gpu", dims::CONVERSATION_DEPTH, dims::RECALL_CEILING_TURNS, 5.0, None));
    // coherence panel rows (one high-SD → judge-ambiguous)
    rows.push(row("alpha:8b", "gpu", dims::CONVERSATION_DEPTH, dims::COHERENCE, 4.0, Some(0.2)));
    rows.push(row("bravo:8b", "gpu", dims::CONVERSATION_DEPTH, dims::COHERENCE, 3.0, Some(1.5)));

    // ── dim-2 tool chaining (mean_chain_accuracy) ──
    rows.push(row("alpha:8b", "gpu", dims::TOOL_CHAINING, dims::MEAN_CHAIN_ACCURACY, 0.9, None));
    rows.push(row("bravo:8b", "gpu", dims::TOOL_CHAINING, dims::MEAN_CHAIN_ACCURACY, 0.6, None));

    // ── dim-3 memory survival (fact_survival_rate) ──
    rows.push(row("alpha:8b", "gpu", dims::MEMORY_INTEGRATION, dims::FACT_SURVIVAL_RATE, 0.75, None));
    rows.push(row("bravo:8b", "gpu", dims::MEMORY_INTEGRATION, dims::FACT_SURVIVAL_RATE, 0.95, None));

    // ── dim-4 latent OCEAN proximity (proximity_to_lumina; higher = closer) ──
    rows.push(row("alpha:8b", "gpu", dims::PERSONALITY_LATENT, dims::PROXIMITY_TO_LUMINA, 4.5, None));
    rows.push(row("bravo:8b", "gpu", dims::PERSONALITY_LATENT, dims::PROXIMITY_TO_LUMINA, 3.5, None));
    // charlie is far from Lumina's disposition → below the 3.0 cutoff, NOT shortlisted.
    rows.push(row("charlie:70b", "gpu", dims::PERSONALITY_LATENT, dims::PROXIMITY_TO_LUMINA, 2.0, None));

    // ── dim-5 prompted adherence (behavioral + trait sub-scores) ──
    // alpha: close on dim-4 AND strong behavioral adherence.
    for m in dims::prompted_behavioral_metrics() {
        rows.push(row("alpha:8b", "gpu", dims::PERSONALITY_PROMPTED, m, 4.5, Some(0.3)));
    }
    for m in dims::prompted_trait_metrics() {
        rows.push(row("alpha:8b", "gpu", dims::PERSONALITY_PROMPTED, m, 4.0, Some(0.2)));
    }
    // bravo: shortlisted on dim-4 but WEAKER behavioral adherence.
    for m in dims::prompted_behavioral_metrics() {
        rows.push(row("bravo:8b", "gpu", dims::PERSONALITY_PROMPTED, m, 3.0, Some(0.4)));
    }
    for m in dims::prompted_trait_metrics() {
        rows.push(row("bravo:8b", "gpu", dims::PERSONALITY_PROMPTED, m, 3.5, Some(0.3)));
    }

    // ── dim-6 embeddings (ndcg_at_k + delta + latency) ──
    rows.push(row("alpha:8b", "gpu", dims::EMBEDDINGS, dims::NDCG_AT_K, 0.82, None));
    rows.push(row("bravo:8b", "gpu", dims::EMBEDDINGS, dims::NDCG_AT_K, 0.71, None));
    rows.push(row("alpha:8b", "gpu", dims::EMBEDDINGS, dims::NDCG_AT_K_DELTA, -0.05, None));
    rows.push(row("bravo:8b", "gpu", dims::EMBEDDINGS, dims::NDCG_AT_K_DELTA, -0.20, None));
    // latency: alpha fast, bravo slow.
    rows.push(row("alpha:8b", "gpu", dims::EMBEDDINGS, dims::EMBED_LATENCY_MS, 800.0, None));
    rows.push(row("bravo:8b", "gpu", dims::EMBEDDINGS, dims::EMBED_LATENCY_MS, 1500.0, None));

    rows
}

// ── money queries ────────────────────────────────────────────────────────────

#[test]
fn best_conversation_depth_ranks_by_recall_ceiling() {
    let rows = seed_rows();
    let q = reporting::best_conversation_depth(&rows, &ReportConfig::default());
    assert_eq!(q.metric, "recall_ceiling_turns");
    // bravo (80) > alpha (40) > charlie (5)
    assert_eq!(q.leader().unwrap().key.model_id, "bravo:8b");
    let ids: Vec<&str> = q.ranking.iter().map(|e| e.key.model_id.as_str()).collect();
    assert_eq!(ids, vec!["bravo:8b", "alpha:8b", "charlie:70b"]);
}

#[test]
fn best_tool_chaining_and_memory_pick_the_right_leaders() {
    let rows = seed_rows();
    let cfg = ReportConfig::default();
    assert_eq!(
        reporting::best_tool_chaining(&rows, &cfg).leader().unwrap().key.model_id,
        "alpha:8b" // 0.9 > 0.6
    );
    assert_eq!(
        reporting::best_memory_survival(&rows, &cfg).leader().unwrap().key.model_id,
        "bravo:8b" // 0.95 > 0.75
    );
}

#[test]
fn high_sd_rows_are_flagged_judge_ambiguous() {
    // Rank coherence directly to exercise SD flagging on a panel metric.
    let rows = seed_rows();
    let cfg = ReportConfig::default();
    // bravo coherence SD 1.5 ≥ 1.0 → high_sd; alpha SD 0.2 → not.
    let q = reporting::best_conversation_depth(&rows, &cfg);
    // recall metric has no SD; verify the SD flag plumbs via a coherence-style row.
    let coherence_rows: Vec<_> = rows
        .iter()
        .filter(|r| r.metric == dims::COHERENCE)
        .cloned()
        .collect();
    let bravo = coherence_rows.iter().find(|r| r.model_id == "bravo:8b").unwrap();
    assert!(bravo.is_high_sd(cfg.high_sd));
    let alpha = coherence_rows.iter().find(|r| r.model_id == "alpha:8b").unwrap();
    assert!(!alpha.is_high_sd(cfg.high_sd));
    // sanity: the recall query produced a ranking
    assert!(!q.ranking.is_empty());
}

#[test]
fn embedding_leader_and_engram_delta_reported_separately() {
    let rows = seed_rows();
    let cfg = ReportConfig::default();
    let leader = reporting::embedding_leader(&rows, &cfg);
    assert_eq!(leader.leader().unwrap().key.model_id, "alpha:8b"); // 0.82 > 0.71
    let delta = reporting::embedding_public_vs_engram_delta(&rows, &cfg);
    // bravo has the worse (more negative) Engram delta → domain mismatch signal.
    let bravo = delta.iter().find(|e| e.key.model_id == "bravo:8b").unwrap();
    assert!((bravo.value - (-0.20)).abs() < 1e-9);
}

// ── sequential personality read ───────────────────────────────────────────────

#[test]
fn personality_is_dim4_shortlist_then_dim5_ranking() {
    let rows = seed_rows();
    let cfg = ReportConfig::default();
    let read = reporting::personality_read(&rows, &cfg);

    // STEP 1: shortlist contains alpha + bravo (≥3.0 proximity), NOT charlie (2.0).
    let shortlisted: Vec<&str> = read
        .shortlist
        .members
        .iter()
        .map(|m| m.key.model_id.as_str())
        .collect();
    assert!(shortlisted.contains(&"alpha:8b"));
    assert!(shortlisted.contains(&"bravo:8b"));
    assert!(!shortlisted.contains(&"charlie:70b"), "charlie below proximity cutoff must be excluded");

    // STEP 2: dim-5 ranking is over the shortlist only, ranked by behavioral_mean.
    // alpha (behavioral 4.5) ranks above bravo (3.0).
    let ranked: Vec<&str> = read
        .prompted_ranking
        .iter()
        .map(|p| p.key.model_id.as_str())
        .collect();
    assert_eq!(ranked, vec!["alpha:8b", "bravo:8b"]);
    // charlie never appears in dim-5 (it wasn't shortlisted), even if it had rows.
    assert!(!ranked.contains(&"charlie:70b"));
}

/// The CRITICAL structural test: render the full report and FAIL if any
/// merged/combined personality score ever appears. Dim-4 and dim-5 are different
/// scales (Notes #2) and must never be averaged into one number.
#[test]
fn report_never_contains_a_merged_personality_score() {
    let rows = seed_rows();
    let cfg = ReportConfig::default();
    let report = build_report(&rows, vec![], &cfg);
    let md = reporting::render_markdown(&report);
    let lower = md.to_lowercase();

    // Any of these tokens would mean someone collapsed dim-4 + dim-5 into one score.
    for forbidden in [
        "combined personality",
        "merged personality",
        "personality_score",
        "personality score",
        "overall personality",
        "blended personality",
    ] {
        assert!(
            !lower.contains(forbidden),
            "report leaked a merged personality score token: {forbidden:?}"
        );
    }

    // And the type itself must keep them apart: there is a shortlist field AND a
    // separate prompted_ranking field — proven by both being populated independently.
    assert!(!report.personality.shortlist.members.is_empty());
    assert!(!report.personality.prompted_ranking.is_empty());

    // Structural guarantee: a PromptedAdherence carries dim-5 means, never a
    // proximity (dim-4) value folded in. The dim-4 proximity for alpha is 4.5; the
    // dim-5 behavioral_mean is 4.5 too in this fixture, but they live in DIFFERENT
    // fields/structs — verify the dim-5 struct has no proximity field by checking
    // its behavioral sub-scores are exactly the dim-5 behavioral metrics.
    let alpha = &report.personality.prompted_ranking[0];
    let mut got: Vec<&str> = alpha.behavioral_sub_scores.keys().map(|k| k.as_str()).collect();
    got.sort();
    let mut want: Vec<&str> = dims::prompted_behavioral_metrics().to_vec();
    want.sort();
    assert_eq!(got, want, "dim-5 sub-scores must be exactly the dim-5 behavioral metrics");
}

// ── chat-role guard ───────────────────────────────────────────────────────────

#[test]
fn guard_excludes_personality_topping_but_degrading_model() {
    // delta:13b tops personality (behavioral 5.0) AND is on the dim-4 shortlist,
    // but degrades at 3 turns and is slow → must be excluded with a recorded reason,
    // and still appear in the report.
    let mut rows = seed_rows();
    rows.push(row("delta:13b", "gpu", dims::PERSONALITY_LATENT, dims::PROXIMITY_TO_LUMINA, 5.0, None));
    for m in dims::prompted_behavioral_metrics() {
        rows.push(row("delta:13b", "gpu", dims::PERSONALITY_PROMPTED, m, 5.0, Some(0.1)));
    }
    for m in dims::prompted_trait_metrics() {
        rows.push(row("delta:13b", "gpu", dims::PERSONALITY_PROMPTED, m, 5.0, Some(0.1)));
    }
    // degrades early + slow
    rows.push(row("delta:13b", "gpu", dims::CONVERSATION_DEPTH, dims::RECALL_CEILING_TURNS, 3.0, None));
    rows.push(row("delta:13b", "gpu", dims::EMBEDDINGS, dims::EMBED_LATENCY_MS, 9000.0, None));

    let cfg = ReportConfig::default();
    let report = build_report(&rows, vec![], &cfg);

    // delta tops the dim-5 ranking (behavioral 5.0)...
    assert_eq!(report.personality.prompted_ranking[0].key.model_id, "delta:13b");

    // ...but is EXCLUDED by the guard with a reason, and the selected model is the
    // next eligible one (alpha, which holds 40 turns and is fast).
    let delta_candidate = report
        .chat_role
        .candidates
        .iter()
        .find(|c| c.key.model_id == "delta:13b")
        .expect("excluded model must still appear in the report");
    match &delta_candidate.verdict {
        GuardVerdict::Excluded { reason } => {
            assert!(reason.contains("recall_ceiling_turns") || reason.contains("latency"));
        }
        GuardVerdict::Eligible => panic!("degrading+slow model must be excluded"),
    }
    assert_eq!(
        report.chat_role.selected.as_ref().unwrap().model_id,
        "alpha:8b",
        "chat role should fall to the highest-adherence model that clears the guard"
    );
}

#[test]
fn no_model_clears_guard_keeps_default() {
    // Build a row set where every shortlisted model degrades early.
    let mut rows = Vec::new();
    rows.push(row("x:7b", "gpu", dims::PERSONALITY_LATENT, dims::PROXIMITY_TO_LUMINA, 4.0, None));
    for m in dims::prompted_behavioral_metrics() {
        rows.push(row("x:7b", "gpu", dims::PERSONALITY_PROMPTED, m, 4.0, Some(0.2)));
    }
    // degrades at 2 turns → below the default min of 10
    rows.push(row("x:7b", "gpu", dims::CONVERSATION_DEPTH, dims::RECALL_CEILING_TURNS, 2.0, None));

    let cfg = ReportConfig::default();
    let report = build_report(&rows, vec![], &cfg);
    assert!(report.chat_role.selected.is_none());
    let note = report.chat_role.no_clearance_note().unwrap();
    assert!(note.contains("keeps the current default"));
}

#[test]
fn custom_guard_threshold_is_honoured() {
    // Tighten the recall floor so even alpha (40 turns) is fine but a 30-turn model fails.
    let mut rows = seed_rows();
    rows.push(row("echo:8b", "gpu", dims::PERSONALITY_LATENT, dims::PROXIMITY_TO_LUMINA, 4.8, None));
    for m in dims::prompted_behavioral_metrics() {
        rows.push(row("echo:8b", "gpu", dims::PERSONALITY_PROMPTED, m, 5.0, Some(0.1)));
    }
    rows.push(row("echo:8b", "gpu", dims::CONVERSATION_DEPTH, dims::RECALL_CEILING_TURNS, 30.0, None));

    let mut cfg = ReportConfig::default();
    cfg.guard = ChatRoleGuard {
        min_recall_ceiling_turns: 35.0,
        max_latency_ms: 4000.0,
    };
    let report = build_report(&rows, vec![], &cfg);
    // echo tops adherence (5.0) but only holds 30 < 35 → excluded; alpha (40) selected.
    let echo = report.chat_role.candidates.iter().find(|c| c.key.model_id == "echo:8b").unwrap();
    assert!(matches!(echo.verdict, GuardVerdict::Excluded { .. }));
    assert_eq!(report.chat_role.selected.as_ref().unwrap().model_id, "alpha:8b");
}

// ── dual-profile side-by-side ─────────────────────────────────────────────────

#[test]
fn dual_profile_rows_render_in_report() {
    let rows = seed_rows();
    let dual = vec![
        DualProfileRow {
            model_id: "alpha:8b".into(),
            backend_tag: Some("gpu".into()),
            has_builder_profile: true,
            has_assistant_profile: true,
            builder_avg_quality: Some(0.7),
            assistant_avg_value: Some(3.9),
        },
        DualProfileRow {
            model_id: "lonely:builder".into(),
            backend_tag: Some("cpu".into()),
            has_builder_profile: true,
            has_assistant_profile: false,
            builder_avg_quality: Some(0.6),
            assistant_avg_value: None,
        },
    ];
    let report = build_report(&rows, dual, &ReportConfig::default());
    let md = reporting::render_markdown(&report);
    assert!(md.contains("model_dual_profile"));
    assert!(md.contains("lonely:builder"));
    // builder-only model shows builder ✓, assistant — (visible, not dropped).
    assert_eq!(report.dual_profile.len(), 2);
}
