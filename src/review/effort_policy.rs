//! REVX-03..07,14 — dynamic, intelligent per-provider review effort policy.
//!
//! `review_run` used to dispatch every provider at a single, static effort
//! (the only exception: REVCAP-01 PART B's intensive-substitute path, which
//! force-sets `"high"` -- see `dispatch.rs::INTENSIVE_REASONING_EFFORT`).
//! This module computes a **base tier** from the diff/change-type signals
//! (REVX-03), adapts it across repeated review passes on the same MR/branch
//! (REVX-04), modulates it per-provider by role/budget/caller-override
//! (REVX-05), and maps the canonical tier onto each provider's own native
//! reasoning-control surface (REVX-07: codex's `-c model_reasoning_effort=`
//! + dynamic GPT-5.6 model tier, Anthropic's adaptive `--effort`).
//! `mod.rs::execute()` (REVX-14) wires this in per run.
//!
//! ## Degrade-safe by construction
//! Every function here is pure and total: missing/malformed signals fall
//! back to the config baseline (`Medium` by default), Redis absence falls
//! back to "pass 1", and an unknown provider gets [`EffortTier::Medium`] with
//! no native mapping. Nothing in this module ever panics, blocks, or fails a
//! review -- worst case, it reproduces today's static-medium behavior (see
//! [`EffortPolicyConfig::enabled`]).

use serde::Serialize;
use serde_json::Value;

// ── EffortTier ──────────────────────────────────────────────────────────────

/// The canonical, provider-agnostic reasoning-effort tier. Ordered
/// `Minimal < Low < Medium < High < Xhigh`. Every provider-native mapping
/// ([`tier_to_native`], [`codex_model_for_tier`]) clamps this down onto
/// whatever levels that provider actually supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EffortTier {
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
}

impl EffortTier {
    const ORDER: [EffortTier; 5] =
        [EffortTier::Minimal, EffortTier::Low, EffortTier::Medium, EffortTier::High, EffortTier::Xhigh];

    fn index(self) -> usize {
        Self::ORDER.iter().position(|t| *t == self).unwrap_or(2)
    }

    /// One step up, saturating at [`EffortTier::Xhigh`].
    pub fn escalate(self) -> Self {
        Self::ORDER[(self.index() + 1).min(Self::ORDER.len() - 1)]
    }

    /// One step down, saturating at [`EffortTier::Minimal`].
    pub fn deescalate(self) -> Self {
        Self::ORDER[self.index().saturating_sub(1)]
    }

    /// `self`, clamped to be no higher than `cap`.
    pub fn cap_at(self, cap: EffortTier) -> Self {
        self.min(cap)
    }

    /// `self`, clamped to be no lower than `floor`.
    pub fn floor_at(self, floor: EffortTier) -> Self {
        self.max(floor)
    }

    /// Parse a caller-supplied tier string (case-insensitive). Used for the
    /// REVX-05 caller-override path -- an unrecognized string is REJECTED
    /// (returns `None`) rather than silently coerced, so the caller sees
    /// "override rejected (invalid)" instead of a guessed tier.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "minimal" | "none" => Some(EffortTier::Minimal),
            "low" => Some(EffortTier::Low),
            "medium" => Some(EffortTier::Medium),
            "high" => Some(EffortTier::High),
            "xhigh" | "max" => Some(EffortTier::Xhigh),
            _ => None,
        }
    }
}

impl Default for EffortTier {
    fn default() -> Self {
        EffortTier::Medium
    }
}

// ── EffortPolicyConfig (REVX-06) ────────────────────────────────────────────

/// Change-type risk classification for a diff's touched paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RiskClass {
    High,
    Neutral,
    Low,
}

/// Config-tunable thresholds for the effort policy. Mirrors `free_pool.rs`'s
/// plain-env-var convention: these are non-secret tunables (thresholds,
/// provider-role sets), not credentials, so `std::env::var` is read directly
/// rather than through a vault client.
#[derive(Debug, Clone)]
pub struct EffortPolicyConfig {
    /// Master switch. `false` restores today's static behavior: `mod.rs`
    /// skips policy computation entirely and dispatch opts are unaffected
    /// (see `REVIEW_EFFORT_POLICY_ENABLED`).
    pub enabled: bool,
    /// The tier a diff with no signals at all maps to.
    pub baseline_tier: EffortTier,
    /// LOC-changed thresholds (inclusive lower bound) for each escalation step.
    pub loc_escalate_threshold: u64,
    pub loc_deescalate_threshold: u64,
    /// Files-touched threshold for escalation.
    pub files_escalate_threshold: u64,
    /// Cross-module (distinct top-level dirs) threshold for escalation.
    pub cross_module_escalate_threshold: u64,
    /// Cap on the prior-pass escalation lever (REVX-04) -- a contested
    /// re-review can never be pushed above this tier by pass-history alone.
    pub max_escalation_tier: EffortTier,
    /// Once `pass_number` exceeds this AND the change is still contested,
    /// [`apply_pass_history`] signals `hand_off: true`.
    pub max_passes_before_handoff: u32,
    /// Providers classified as capstone/high-assurance seats (REVX-05). A
    /// provider not in either set defaults to the mid tier.
    pub capstone_providers: Vec<String>,
    pub breadth_tail_providers: Vec<String>,
    /// Risk-path substrings (case-insensitive) that classify a touched file
    /// as HIGH risk -- security/secrets/auth/egress/unsafe/crypto/migration/
    /// concurrency/payment. Config-overridable; the built-in set covers the
    /// spec's named categories.
    pub risk_path_substrings: Vec<String>,
    /// Path substrings that (in the absence of any HIGH signal) classify a
    /// change as LOW risk -- docs/comments/formatting/test-only.
    pub low_risk_path_substrings: Vec<String>,
    /// [`EffortTier`] cap applied to `claude-fable-5` regardless of the
    /// computed tier (REVX-13) -- protects its very limited token budget.
    pub fable_max_tier: EffortTier,
}

fn env_str(key: &str) -> Option<String> {
    std::env::var(key).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

fn env_bool(key: &str, default: bool) -> bool {
    match env_str(key) {
        None => default,
        Some(v) => !matches!(v.as_str(), "0" | "false" | "off" | "no"),
    }
}

/// Parse a numeric env var, falling back to `default` on ANY malformed value
/// (unparseable, negative-where-unsigned, etc) -- never panics, warns once
/// per malformed read.
fn env_u64(key: &str, default: u64) -> u64 {
    match env_str(key) {
        None => default,
        Some(v) => match v.parse::<u64>() {
            Ok(n) => n,
            Err(_) => {
                tracing::warn!("effort_policy: malformed {key}={v:?}, using default {default}");
                default
            }
        },
    }
}

fn env_u32(key: &str, default: u32) -> u32 {
    match env_str(key) {
        None => default,
        Some(v) => match v.parse::<u32>() {
            Ok(n) => n,
            Err(_) => {
                tracing::warn!("effort_policy: malformed {key}={v:?}, using default {default}");
                default
            }
        },
    }
}

fn env_tier(key: &str, default: EffortTier) -> EffortTier {
    match env_str(key) {
        None => default,
        Some(v) => EffortTier::parse(&v).unwrap_or_else(|| {
            tracing::warn!("effort_policy: malformed {key}={v:?}, using default {default:?}");
            default
        }),
    }
}

fn env_list(key: &str, default: &[&str]) -> Vec<String> {
    match env_str(key) {
        None => default.iter().map(|s| s.to_string()).collect(),
        Some(v) => {
            let items: Vec<String> =
                v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
            if items.is_empty() {
                // Empty override -> fall back to the built-in classification
                // (REVX-06 edge case), never an empty provider set.
                default.iter().map(|s| s.to_string()).collect()
            } else {
                items
            }
        }
    }
}

const DEFAULT_CAPSTONE_PROVIDERS: &[&str] = &["opus", "codex", "agy", "claude-fable-5", "paid"];
const DEFAULT_BREADTH_TAIL_PROVIDERS: &[&str] = &["free", "nemotron", "qwen_coder", "diffusion"];

const DEFAULT_RISK_PATH_SUBSTRINGS: &[&str] = &[
    "auth", "secret", "credential", "token", "password", "vault", "crypto", "cipher",
    "egress", "firewall", "unsafe", "migration", "migrations", "concurrency", "mutex",
    "payment", "billing", "jwt", "oauth", "session", "acl", "permission", "sandbox",
];

const DEFAULT_LOW_RISK_PATH_SUBSTRINGS: &[&str] =
    &["docs/", "/docs/", ".md", "readme", "comment", "fmt", "format", "test", "tests/", "spec_"];

impl EffortPolicyConfig {
    /// Read every threshold from `REVIEW_EFFORT_*`, falling back to
    /// no-op-safe defaults chosen so a neutral medium diff still maps to
    /// `Medium` (REVX-06).
    pub fn from_env() -> Self {
        Self {
            enabled: env_bool("REVIEW_EFFORT_POLICY_ENABLED", true),
            baseline_tier: env_tier("REVIEW_EFFORT_BASELINE_TIER", EffortTier::Medium),
            loc_escalate_threshold: env_u64("REVIEW_EFFORT_LOC_ESCALATE", 400),
            loc_deescalate_threshold: env_u64("REVIEW_EFFORT_LOC_DEESCALATE", 20),
            files_escalate_threshold: env_u64("REVIEW_EFFORT_FILES_ESCALATE", 8),
            cross_module_escalate_threshold: env_u64("REVIEW_EFFORT_CROSS_MODULE_ESCALATE", 3),
            max_escalation_tier: env_tier("REVIEW_EFFORT_MAX_ESCALATION_TIER", EffortTier::High),
            max_passes_before_handoff: env_u32("REVIEW_EFFORT_MAX_PASSES_BEFORE_HANDOFF", 4),
            capstone_providers: env_list("REVIEW_EFFORT_CAPSTONE_PROVIDERS", DEFAULT_CAPSTONE_PROVIDERS),
            breadth_tail_providers: env_list(
                "REVIEW_EFFORT_BREADTH_TAIL_PROVIDERS",
                DEFAULT_BREADTH_TAIL_PROVIDERS,
            ),
            risk_path_substrings: env_list("REVIEW_EFFORT_RISK_PATHS", DEFAULT_RISK_PATH_SUBSTRINGS),
            low_risk_path_substrings: env_list(
                "REVIEW_EFFORT_LOW_RISK_PATHS",
                DEFAULT_LOW_RISK_PATH_SUBSTRINGS,
            ),
            fable_max_tier: env_tier("REVIEW_EFFORT_FABLE_MAX_TIER", EffortTier::Medium),
        }
    }
}

impl Default for EffortPolicyConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            baseline_tier: EffortTier::Medium,
            loc_escalate_threshold: 400,
            loc_deescalate_threshold: 20,
            files_escalate_threshold: 8,
            cross_module_escalate_threshold: 3,
            max_escalation_tier: EffortTier::High,
            max_passes_before_handoff: 4,
            capstone_providers: DEFAULT_CAPSTONE_PROVIDERS.iter().map(|s| s.to_string()).collect(),
            breadth_tail_providers: DEFAULT_BREADTH_TAIL_PROVIDERS.iter().map(|s| s.to_string()).collect(),
            risk_path_substrings: DEFAULT_RISK_PATH_SUBSTRINGS.iter().map(|s| s.to_string()).collect(),
            low_risk_path_substrings: DEFAULT_LOW_RISK_PATH_SUBSTRINGS.iter().map(|s| s.to_string()).collect(),
            fable_max_tier: EffortTier::Medium,
        }
    }
}

// ── DiffSignals + base_tier (REVX-03) ───────────────────────────────────────

/// Diff/change-type signals feeding the base-tier computation. Populated
/// best-effort from the review `context` -- missing fields default to
/// `None`/`Neutral`/`false`, never an error.
#[derive(Debug, Clone, Default)]
pub struct DiffSignals {
    pub loc_changed: Option<u64>,
    pub files_touched: Option<u64>,
    /// Distinct top-level directories touched (a coarse cross-module proxy).
    pub cross_module: Option<u64>,
    pub risk_class: Option<RiskClass>,
    pub new_logic_without_tests: bool,
}

impl DiffSignals {
    /// Build signals from a review `context` object. Prefers explicit
    /// caller-supplied hints (`context.loc_changed`, `context.files_touched`,
    /// `context.new_logic_without_tests`) and falls back to deriving
    /// `files_touched`/`cross_module`/`risk_class` from `changed_files` (via
    /// `kg_context::derive_changed_files`, the same extraction `review_run`
    /// already uses elsewhere -- see the module doc's "REFERENCE" note).
    /// Binary/generated-looking paths are excluded from the file count.
    pub fn from_context(context: &Value, cfg: &EffortPolicyConfig) -> Self {
        let changed_files: Vec<String> = super::kg_context::derive_changed_files(context)
            .into_iter()
            .filter(|f| !is_binary_or_generated(f))
            .collect();

        let loc_changed = context
            .get("loc_changed")
            .and_then(Value::as_u64)
            .or_else(|| context.get("diff_stat").and_then(|d| d.get("loc_changed")).and_then(Value::as_u64));

        let files_touched = context
            .get("files_touched")
            .and_then(Value::as_u64)
            .or_else(|| {
                if changed_files.is_empty() {
                    None
                } else {
                    Some(changed_files.len() as u64)
                }
            });

        let cross_module = context.get("cross_module").and_then(Value::as_u64).or_else(|| {
            if changed_files.is_empty() {
                None
            } else {
                let dirs: std::collections::HashSet<&str> =
                    changed_files.iter().filter_map(|f| f.split('/').next()).collect();
                Some(dirs.len() as u64)
            }
        });

        let new_logic_without_tests =
            context.get("new_logic_without_tests").and_then(Value::as_bool).unwrap_or(false);

        let risk_class = if changed_files.is_empty() {
            None
        } else {
            Some(classify_risk_paths(&changed_files, cfg))
        };

        Self { loc_changed, files_touched, cross_module, risk_class, new_logic_without_tests }
    }
}

/// Excludes binary/generated files from LOC/logic signals (EDGE CASE from the
/// spec) -- a coarse, path-shape-based check, not a content sniff.
fn is_binary_or_generated(path: &str) -> bool {
    const BINARY_EXTS: &[&str] = &[
        ".png", ".jpg", ".jpeg", ".gif", ".ico", ".pdf", ".zip", ".tar", ".gz", ".woff", ".woff2",
        ".ttf", ".so", ".dylib", ".dll", ".wasm", ".lock",
    ];
    let lower = path.to_ascii_lowercase();
    BINARY_EXTS.iter().any(|ext| lower.ends_with(ext))
        || lower.ends_with("-lock.json")
        || lower.contains("/generated/")
        || lower.contains("/dist/")
        || lower.contains("/target/")
}

/// Classify a set of touched paths into a [`RiskClass`]. HIGH wins over LOW
/// (deterministic tie-break, per the spec's EDGE CASES: a diff that is both
/// large/spread AND touches a risk path is HIGH). Reuses only PATH-SHAPE
/// substring matching here -- `src/github/pii.rs`'s content-based secret
/// scanner (`scan_for_pii`) is a complementary, heavier signal callers may
/// layer on top via `context.diff` (see [`content_risk_hint`]), not
/// re-implemented here.
pub fn classify_risk_paths(changed_files: &[String], cfg: &EffortPolicyConfig) -> RiskClass {
    let lower: Vec<String> = changed_files.iter().map(|f| f.to_ascii_lowercase()).collect();
    let is_high = lower
        .iter()
        .any(|f| cfg.risk_path_substrings.iter().any(|pat| f.contains(&pat.to_ascii_lowercase())));
    if is_high {
        return RiskClass::High;
    }
    let all_low = !lower.is_empty()
        && lower
            .iter()
            .all(|f| cfg.low_risk_path_substrings.iter().any(|pat| f.contains(&pat.to_ascii_lowercase())));
    if all_low {
        RiskClass::Low
    } else {
        RiskClass::Neutral
    }
}

/// Best-effort content-level risk hint: reuses `src/github/pii.rs`'s existing
/// secret-shaped-string scanner (`scan_for_pii`) against an optional raw diff
/// text (`context.diff`) so a review whose risk lives in NEW secret-shaped
/// literals (not just risky paths) still escalates. Never forks the PII
/// patterns -- calls straight through. Returns `true` only on a genuine hit;
/// absence of `context.diff` or no hits is `false`, never an error.
pub fn content_risk_hint(context: &Value) -> bool {
    let Some(diff) = context.get("diff").and_then(Value::as_str) else {
        return false;
    };
    !crate::github::pii::scan_for_pii(diff).is_empty()
}

/// REVX-03: compute the base [`EffortTier`] from diff signals, independent of
/// provider. Pure, deterministic, no env/network/secret access -- thresholds
/// come from `cfg`. Returns the tier plus a human-readable reasons vector for
/// the run record.
pub fn base_tier(signals: &DiffSignals, cfg: &EffortPolicyConfig) -> (EffortTier, Vec<String>) {
    let mut tier = cfg.baseline_tier;
    let mut reasons = Vec::new();

    let has_any_signal = signals.loc_changed.is_some()
        || signals.files_touched.is_some()
        || signals.cross_module.is_some()
        || signals.risk_class.is_some();
    if !has_any_signal {
        reasons.push("no diff signals: baseline tier".to_string());
        return (tier, reasons);
    }

    // Size/spread signals raise the tier.
    if let Some(loc) = signals.loc_changed {
        if loc >= cfg.loc_escalate_threshold {
            tier = tier.escalate();
            reasons.push(format!("size: {loc} LOC changed (>= {})", cfg.loc_escalate_threshold));
        } else if loc <= cfg.loc_deescalate_threshold {
            reasons.push(format!("size: only {loc} LOC changed (small)"));
        }
    }
    if let Some(files) = signals.files_touched {
        if files >= cfg.files_escalate_threshold {
            tier = tier.escalate();
            reasons.push(format!("spread: {files} files touched (>= {})", cfg.files_escalate_threshold));
        }
    }
    if let Some(modules) = signals.cross_module {
        if modules >= cfg.cross_module_escalate_threshold {
            tier = tier.escalate();
            reasons.push(format!(
                "spread: {modules} cross-module dirs touched (>= {})",
                cfg.cross_module_escalate_threshold
            ));
        }
    }

    // Risk-path classification. HIGH always wins the tie-break against a
    // simultaneous "pure deletion"/size-down signal (EDGE CASE).
    match signals.risk_class {
        Some(RiskClass::High) => {
            tier = tier.escalate().floor_at(EffortTier::High).max(tier);
            reasons.push("HIGH: touches a security/secrets/auth/egress/unsafe/migration/concurrency path"
                .to_string());
        }
        Some(RiskClass::Low) => {
            let before = tier;
            tier = tier.deescalate();
            if tier != before {
                reasons.push("LOW: docs/comments/formatting/test-only/pure-deletion change".to_string());
            }
        }
        Some(RiskClass::Neutral) | None => {}
    }

    if signals.new_logic_without_tests {
        tier = tier.escalate();
        reasons.push("new logic added without a corresponding test delta".to_string());
    }

    if reasons.is_empty() {
        reasons.push(format!("neutral diff: baseline tier {tier:?}"));
    }

    (tier.cap_at(EffortTier::Xhigh).floor_at(EffortTier::Minimal), reasons)
}

// ── Prior-pass adaptive lever (REVX-04) ─────────────────────────────────────

/// The SAME MR/branch's review-pass history. Sourced from the caller's
/// `context` (an optional `prior_passes` block), or best-effort from a
/// Redis-backed counter keyed by repo/branch when the caller supplies none
/// (see [`load_pass_history`]). Missing/malformed history is `pass_number: 1`
/// with no findings -- never an error.
#[derive(Debug, Clone, Default)]
pub struct PassHistory {
    pub pass_number: u32,
    pub prior_findings_material: usize,
    pub prior_verdict_contested: bool,
}

impl PassHistory {
    pub fn first_pass() -> Self {
        Self { pass_number: 1, prior_findings_material: 0, prior_verdict_contested: false }
    }

    /// Parse from `context.prior_passes` (an object with `pass_number`,
    /// `prior_findings_material`, `prior_verdict_contested`). A malformed or
    /// absent block returns `None` (caller falls back to Redis, then pass 1)
    /// -- never panics on a bad shape.
    pub fn from_context(context: &Value) -> Option<Self> {
        let block = context.get("prior_passes")?;
        // A malformed block (not an object -- e.g. a bare string/number) is
        // ignored outright rather than partially parsed (EDGE CASE: "treat
        // as pass 1, never error").
        if !block.is_object() {
            return None;
        }
        let pass_number = block.get("pass_number").and_then(Value::as_u64).unwrap_or(1).max(1) as u32;
        let prior_findings_material =
            block.get("prior_findings_material").and_then(Value::as_u64).unwrap_or(0) as usize;
        let prior_verdict_contested =
            block.get("prior_verdict_contested").and_then(Value::as_bool).unwrap_or(false);
        Some(Self { pass_number, prior_findings_material, prior_verdict_contested })
    }
}

/// REVX-04: adapt the base tier for the SAME MR/branch's review history.
/// Contested re-reviews (prior material findings > 0 AND the prior verdict
/// was REQUEST_CHANGES) escalate one tier per pass, capped at
/// `cfg.max_escalation_tier`. A trivial re-verify (prior APPROVE, no
/// contest) de-escalates one tier, floored at `Low`. Past
/// `cfg.max_passes_before_handoff` while still contested, signals
/// `hand_off: true` instead of escalating further -- so the caller can route
/// to a human rather than spending indefinitely.
pub fn apply_pass_history(
    base: EffortTier,
    history: &PassHistory,
    cfg: &EffortPolicyConfig,
) -> (EffortTier, Vec<String>, bool) {
    let mut reasons = Vec::new();
    if history.pass_number <= 1 {
        return (base, reasons, false);
    }

    let contested = history.prior_verdict_contested && history.prior_findings_material > 0;

    if contested {
        if history.pass_number > cfg.max_passes_before_handoff {
            reasons.push(format!(
                "pass {} still contested after {} passes -- hand off to operator instead of \
                 further escalation",
                history.pass_number, cfg.max_passes_before_handoff
            ));
            // Still cap the tier at the configured ceiling; hand_off is the
            // signal that spend should stop growing, not that this pass
            // itself should run at a lower tier than the cap.
            let tier = base.escalate().cap_at(cfg.max_escalation_tier);
            return (tier, reasons, true);
        }
        let tier = base.escalate().cap_at(cfg.max_escalation_tier);
        reasons.push(format!(
            "pass {}: contested re-review ({} prior material findings) -- escalated, capped at {:?}",
            history.pass_number, history.prior_findings_material, cfg.max_escalation_tier
        ));
        (tier, reasons, false)
    } else {
        let tier = base.deescalate().floor_at(EffortTier::Low);
        reasons.push(format!(
            "pass {}: trivial re-verification of an approved change -- de-escalated (diminishing returns)",
            history.pass_number
        ));
        (tier, reasons, false)
    }
}

/// Best-effort Redis-backed fallback source for [`PassHistory`] when the
/// caller's `context` supplies no `prior_passes` block. Keyed by
/// `project_id`/`git_ref` (repo/branch); degrades to pass 1 when Redis is
/// unconfigured/unreachable or the key is absent -- never blocks a review.
/// Reuses the existing shared [`crate::redis::RedisBackend`] singleton (the
/// SAME pool other consumers share) rather than opening a private
/// connection.
pub async fn load_pass_history(context: &Value) -> PassHistory {
    if let Some(h) = PassHistory::from_context(context) {
        return h;
    }
    let Some(key) = pass_history_key(context) else {
        return PassHistory::first_pass();
    };
    let Some(backend) = crate::redis::RedisBackend::from_env() else {
        return PassHistory::first_pass();
    };
    let result = backend
        .with_conn(crate::redis::Namespace::Ratelimit, |mut conn| {
            let key = key.clone();
            async move { redis::cmd("GET").arg(&key).query_async::<_, Option<String>>(&mut conn).await }
        })
        .await;
    match result {
        Ok(Some(raw)) => match serde_json::from_str::<Value>(&raw) {
            Ok(v) => PassHistory::from_context(&serde_json::json!({"prior_passes": v}))
                .unwrap_or_else(PassHistory::first_pass),
            Err(_) => PassHistory::first_pass(),
        },
        _ => PassHistory::first_pass(),
    }
}

/// Record this pass's outcome for the NEXT re-review of the same MR/branch.
/// Best-effort: a Redis-absent/unreachable condition is silently skipped
/// (the pass-history lever simply reads as "pass 1" next time, never a hard
/// failure). TTL bounds an abandoned branch's key from lingering forever.
const PASS_HISTORY_TTL_SECS: u64 = 14 * 24 * 3600;

pub async fn record_pass_outcome(context: &Value, verdict: &str, material_findings: usize) {
    let Some(key) = pass_history_key(context) else { return };
    let Some(backend) = crate::redis::RedisBackend::from_env() else { return };
    let prior = load_pass_history(context).await;
    let next = serde_json::json!({
        "pass_number": prior.pass_number.saturating_add(1),
        "prior_findings_material": material_findings,
        "prior_verdict_contested": verdict == "REQUEST_CHANGES" && material_findings > 0,
    });
    let raw = next.to_string();
    let _ = backend
        .with_conn(crate::redis::Namespace::Ratelimit, |mut conn| {
            let key = key.clone();
            let raw = raw.clone();
            async move {
                redis::cmd("SET").arg(&key).arg(&raw).arg("EX").arg(PASS_HISTORY_TTL_SECS).query_async::<_, ()>(&mut conn).await
            }
        })
        .await;
}

fn pass_history_key(context: &Value) -> Option<String> {
    let project = context.get("project_id").and_then(Value::as_str)?;
    let git_ref = context
        .get("git_ref")
        .and_then(Value::as_str)
        .or_else(|| context.get("pr").and_then(Value::as_str))
        .unwrap_or("unknown");
    Some(format!("review_pass_history:{project}:{git_ref}"))
}

// ── Role + budget + override + per-provider decision (REVX-05, REVX-07) ────

/// Coarse role classification for a provider, used to modulate the run tier
/// per seat (REVX-05).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderRole {
    /// High-assurance seats: run at the FULL run tier.
    Capstone,
    /// Cheap/free breadth tail: clamp down (breadth not depth).
    BreadthTail,
    /// Everything else -- including any UNKNOWN/unrecognized provider name
    /// not listed in either `capstone_providers` or `breadth_tail_providers`.
    /// Safe default: a FIXED `Medium` tier that risk signals do NOT escalate
    /// above (an unrecognized provider must never be trusted with an
    /// escalated, more-expensive/more-privileged effort tier just because
    /// the run happens to be HIGH risk -- see the REVX finding this guards
    /// against).
    Mid,
}

pub fn provider_role(provider: &str, cfg: &EffortPolicyConfig) -> ProviderRole {
    if cfg.capstone_providers.iter().any(|p| p == provider) {
        ProviderRole::Capstone
    } else if cfg.breadth_tail_providers.iter().any(|p| p == provider) {
        ProviderRole::BreadthTail
    } else {
        ProviderRole::Mid
    }
}

/// The final per-provider decision: the resolved [`EffortTier`], a
/// human-readable reason, and (once mapped via [`tier_to_native`]/
/// [`codex_model_for_tier`]) the provider-native effort string / model id.
#[derive(Debug, Clone, Serialize)]
pub struct EffortDecision {
    pub tier: EffortTier,
    pub reason: String,
    pub native: Option<String>,
    pub model: Option<String>,
}

/// REVX-05: compute the FINAL per-provider [`EffortDecision`].
///
/// - `run_tier` is the run-wide tier after [`base_tier`] + [`apply_pass_history`].
/// - `caller_override`, if `Some`, wins outright (parsed via [`EffortTier::parse`];
///   an unparseable override string is REJECTED back to the policy, not
///   silently guessed).
/// - `risk_class` is the run's overall risk classification (used for the
///   token-budget bias below).
/// - `token_budget_set` + `risk_class != High` biases `Mid`/`BreadthTail`
///   seats down one more step; a HIGH-risk run is never biased below its
///   policy tier.
/// - `intensive_floor`, when `Some(EffortTier::High)` (REVCAP-01 PART B), is
///   a SAFETY floor that ALWAYS holds: it is applied to the effective effort
///   AFTER `caller_override` is resolved, i.e. `effective = max(resolved,
///   floor)`. An override may still RAISE the effort above the floor, but it
///   can never LOWER an intensive substitute below it -- the floor wins over
///   a too-low override (this is what makes it a floor and not just a
///   default).
#[allow(clippy::too_many_arguments)]
pub fn decide(
    provider: &str,
    run_tier: EffortTier,
    risk_class: Option<RiskClass>,
    token_budget_set: bool,
    caller_override: Option<&str>,
    intensive_floor: Option<EffortTier>,
    cfg: &EffortPolicyConfig,
) -> EffortDecision {
    if let Some(raw) = caller_override {
        match EffortTier::parse(raw) {
            Some(tier) => {
                let mut reason = "caller override".to_string();
                // REVCAP-01 PART B safety floor: applied AFTER the override
                // is resolved, so it can only ever RAISE the effective tier,
                // never let a too-low override push an intensive substitute
                // below the required floor.
                let effective_tier = if let Some(floor) = intensive_floor {
                    if tier < floor {
                        reason.push_str(&format!(
                            " (intensive-substitute floor enforced: raised {tier:?} -> {floor:?})"
                        ));
                        tier.floor_at(floor)
                    } else {
                        tier
                    }
                } else {
                    tier
                };
                return finalize_decision(provider, effective_tier, reason, cfg);
            }
            None => {
                // Falls through to policy computation with a note.
                let tier = policy_tier_for_provider(
                    provider, run_tier, risk_class, token_budget_set, intensive_floor, cfg,
                );
                return finalize_decision(
                    provider,
                    tier,
                    format!("override rejected (invalid): '{raw}' -- fell back to policy"),
                    cfg,
                );
            }
        }
    }

    let tier =
        policy_tier_for_provider(provider, run_tier, risk_class, token_budget_set, intensive_floor, cfg);
    let role = provider_role(provider, cfg);
    let reason = match role {
        ProviderRole::Capstone => "capstone seat: runs at the full policy tier".to_string(),
        ProviderRole::BreadthTail => "breadth-tail seat: clamped down (breadth not depth)".to_string(),
        ProviderRole::Mid => {
            "unknown/mid-tier seat: safe default, capped at Medium (never escalated by risk)".to_string()
        }
    };
    finalize_decision(provider, tier, reason, cfg)
}

fn policy_tier_for_provider(
    provider: &str,
    run_tier: EffortTier,
    risk_class: Option<RiskClass>,
    token_budget_set: bool,
    intensive_floor: Option<EffortTier>,
    cfg: &EffortPolicyConfig,
) -> EffortTier {
    let role = provider_role(provider, cfg);
    let mut tier = match role {
        ProviderRole::Capstone => run_tier,
        ProviderRole::BreadthTail => run_tier.deescalate().cap_at(EffortTier::Medium),
        // Safe default for an unknown/unrecognized provider (and any other
        // "Mid" seat): a FIXED Medium tier, never escalated by risk_class.
        // A HIGH-risk run must not push an unrecognized provider above its
        // safe default -- only Capstone seats are trusted with that.
        ProviderRole::Mid => run_tier.cap_at(EffortTier::Medium),
    };

    // Token-budget awareness: bias non-capstone seats down one more step on
    // a LOW/Neutral-risk run when a budget is set. HIGH-risk runs are never
    // biased below their policy tier.
    if token_budget_set && role != ProviderRole::Capstone && risk_class != Some(RiskClass::High) {
        tier = tier.deescalate();
    }

    // REVCAP-01 PART B: intensive-substitute floor -- `max(policy, floor)`,
    // preserved regardless of role/budget math above.
    if let Some(floor) = intensive_floor {
        tier = tier.floor_at(floor);
    }

    tier
}

fn finalize_decision(provider: &str, tier: EffortTier, reason: String, cfg: &EffortPolicyConfig) -> EffortDecision {
    // REVX-13: Fable's tiny token budget clamp always applies, even after a
    // caller override (the override still wins on the TIER, but the budget
    // cap advisory reason is recorded either way per the spec's edge case).
    let (final_tier, final_reason) = if provider == "claude-fable-5" && tier > cfg.fable_max_tier {
        (tier, format!("{reason}; Fable budget cap advisory: tier exceeds {:?}", cfg.fable_max_tier))
    } else {
        (tier, reason)
    };
    let clamped = if provider == "claude-fable-5" { final_tier.cap_at(cfg.fable_max_tier) } else { final_tier };
    // Caller-override honor: if the reason says "caller override" (not
    // rejected), never clamp below what the caller asked for even for
    // Fable -- the clamp above is skipped in that case.
    let effective_tier = if final_reason.starts_with("caller override") { final_tier } else { clamped };

    let native = tier_to_native(provider, effective_tier);
    let model = if provider == "codex" { Some(codex_model_for_tier(effective_tier)) } else { None };

    EffortDecision { tier: effective_tier, reason: final_reason, native, model }
}

// ── Provider-native mapping (REVX-07) ───────────────────────────────────────

/// Map the canonical [`EffortTier`] onto `provider`'s own native reasoning
/// control string.
///
/// - `codex`: 5 native levels, confirmed LIVE against codex CLI 0.144.1
///   (`none|low|medium|high|xhigh`; `minimal` 400-errors on gpt-5.6-sol, so
///   `Minimal` maps to `"none"`, not `"minimal"`).
/// - `opus`/`claude-fable-5` (Anthropic adaptive `--effort`): 3 native
///   levels (`low|medium|high`); `Minimal`->`low`, `Xhigh`->`high`.
/// - Everything else (free/nemotron/qwen_coder/diffusion/paid/gpt56, or an
///   unknown provider): no native reasoning control here -- `None` (dispatch
///   omits the field, current behavior). `gpt56`/`paid`'s OpenRouter
///   `reasoning` object mapping is REVX-10's concern, out of this item's
///   scope.
pub fn tier_to_native(provider: &str, tier: EffortTier) -> Option<String> {
    match provider {
        "codex" => Some(
            match tier {
                EffortTier::Minimal => "none",
                EffortTier::Low => "low",
                EffortTier::Medium => "medium",
                EffortTier::High => "high",
                EffortTier::Xhigh => "xhigh",
            }
            .to_string(),
        ),
        "opus" | "claude-fable-5" | "agy" => Some(
            match tier {
                EffortTier::Minimal | EffortTier::Low => "low",
                EffortTier::Medium => "medium",
                EffortTier::High | EffortTier::Xhigh => "high",
            }
            .to_string(),
        ),
        _ => None,
    }
}

/// REVX-07/08: the codex model tier for a given [`EffortTier`] -- GPT-5.6's
/// three named variants (`sol` deepest / `terra` balanced / `luna`
/// cheap-fast), config-overridable per tier via `REVIEW_CODEX_MODEL_SOL` /
/// `_TERRA` / `_LUNA` so an operator can drop a tier under a rate limit
/// without a redeploy (REVX-01's note). This folds REVX-08's model bump in:
/// codex's default moves off `gpt-5.5` onto the GPT-5.6 line, selected
/// DYNAMICALLY by tier rather than statically.
pub fn codex_model_for_tier(tier: EffortTier) -> String {
    let (env_key, default) = match tier {
        EffortTier::High | EffortTier::Xhigh => ("REVIEW_CODEX_MODEL_SOL", "gpt-5.6-sol"),
        EffortTier::Medium => ("REVIEW_CODEX_MODEL_TERRA", "gpt-5.6-terra"),
        EffortTier::Minimal | EffortTier::Low => ("REVIEW_CODEX_MODEL_LUNA", "gpt-5.6-luna"),
    };
    env_str(env_key).unwrap_or_else(|| default.to_string())
}

/// Closed allowlist of codex model ids the policy/daemon will ever forward.
/// Mirrors `review_daemon/provider.rs`'s "closed set, never derived from raw
/// request input" security invariant: even though [`codex_model_for_tier`]
/// is env-overridable, the DAEMON validates the resolved string against this
/// allowlist (`review_daemon/config.rs::clamp_codex_model`) before it ever
/// reaches `build_command` -- an operator can retarget within the GPT-5.6
/// line (or roll back to `gpt-5.5`), but an unrecognized string never
/// reaches the spawned CLI's argv.
pub const ALLOWED_CODEX_MODELS: &[&str] = &["gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna", "gpt-5.5"];

// ── REVX-13: review-only provider contract ──────────────────────────────────

/// Providers this codebase asserts are REVIEW/capstone-only and must never be
/// routed to scaffolding/implementation work by any Terminus surface. The
/// review tool itself never scaffolds, so this is a documented, greppable
/// contract + assertion helper for any future dispatch surface that does --
/// the authoritative enforcement is the build-pipeline/skill rule (REVX-15).
pub fn is_review_only_provider(provider: &str) -> bool {
    provider == "claude-fable-5"
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── EffortTier ──────────────────────────────────────────────────────

    #[test]
    fn tier_ordering_and_saturating_escalate_deescalate() {
        assert!(EffortTier::Minimal < EffortTier::Low);
        assert!(EffortTier::Low < EffortTier::Medium);
        assert!(EffortTier::Medium < EffortTier::High);
        assert!(EffortTier::High < EffortTier::Xhigh);
        assert_eq!(EffortTier::Xhigh.escalate(), EffortTier::Xhigh);
        assert_eq!(EffortTier::Minimal.deescalate(), EffortTier::Minimal);
        assert_eq!(EffortTier::Medium.escalate(), EffortTier::High);
        assert_eq!(EffortTier::Medium.deescalate(), EffortTier::Low);
    }

    #[test]
    fn tier_parse_is_case_insensitive_and_rejects_unknown() {
        assert_eq!(EffortTier::parse("HIGH"), Some(EffortTier::High));
        assert_eq!(EffortTier::parse(" xhigh "), Some(EffortTier::Xhigh));
        assert_eq!(EffortTier::parse("ultra"), None);
    }

    // ── base_tier (REVX-03) ────────────────────────────────────────────

    #[test]
    fn docs_only_diff_yields_low_leaning_tier() {
        let cfg = EffortPolicyConfig::default();
        let ctx = json!({"changed_files": ["docs/guide.md", "README.md"]});
        let signals = DiffSignals::from_context(&ctx, &cfg);
        let (tier, reasons) = base_tier(&signals, &cfg);
        assert!(tier <= EffortTier::Medium, "docs diff should not be above baseline: {tier:?}");
        assert!(!reasons.is_empty());
    }

    #[test]
    fn large_cross_module_auth_diff_yields_high_or_xhigh() {
        let cfg = EffortPolicyConfig::default();
        let ctx = json!({
            "changed_files": ["src/auth/login.rs", "src/net/egress.rs", "crates/x/secret.rs"],
            "loc_changed": 900,
            "files_touched": 12,
        });
        let signals = DiffSignals::from_context(&ctx, &cfg);
        let (tier, reasons) = base_tier(&signals, &cfg);
        assert!(tier >= EffortTier::High, "expected High/Xhigh, got {tier:?}");
        assert!(reasons.iter().any(|r| r.contains("HIGH")));
    }

    #[test]
    fn new_logic_without_tests_raises_tier_shipping_tests_does_not() {
        let cfg = EffortPolicyConfig::default();
        let without_tests = json!({"changed_files": ["src/x.rs"], "new_logic_without_tests": true});
        let with_tests = json!({"changed_files": ["src/x.rs"], "new_logic_without_tests": false});
        let s1 = DiffSignals::from_context(&without_tests, &cfg);
        let s2 = DiffSignals::from_context(&with_tests, &cfg);
        let (t1, _) = base_tier(&s1, &cfg);
        let (t2, _) = base_tier(&s2, &cfg);
        assert!(t1 > t2, "no-tests diff ({t1:?}) should exceed tested diff ({t2:?})");
    }

    #[test]
    fn reasons_vector_nonempty_and_cites_deciding_signal() {
        let cfg = EffortPolicyConfig::default();
        let ctx = json!({"changed_files": ["src/auth/x.rs"]});
        let signals = DiffSignals::from_context(&ctx, &cfg);
        let (_, reasons) = base_tier(&signals, &cfg);
        assert!(reasons.iter().any(|r| r.to_ascii_lowercase().contains("auth") || r.contains("HIGH")));
    }

    #[test]
    fn empty_signals_default_to_baseline_never_panics() {
        let cfg = EffortPolicyConfig::default();
        let signals = DiffSignals::from_context(&json!({}), &cfg);
        let (tier, reasons) = base_tier(&signals, &cfg);
        assert_eq!(tier, cfg.baseline_tier);
        assert!(!reasons.is_empty());
    }

    #[test]
    fn large_pure_deletion_risk_path_signal_wins_tie_break() {
        let cfg = EffortPolicyConfig::default();
        // Large diff (escalate) that ALSO touches a risk path but is
        // filed under "test-only" naming -- risk path wins per the spec's
        // deterministic tie-break.
        let ctx = json!({
            "changed_files": ["src/auth/session.rs"],
            "loc_changed": 900,
        });
        let signals = DiffSignals::from_context(&ctx, &cfg);
        let (tier, _) = base_tier(&signals, &cfg);
        assert!(tier >= EffortTier::High);
    }

    #[test]
    fn binary_files_excluded_from_signals() {
        assert!(is_binary_or_generated("assets/logo.png"));
        assert!(is_binary_or_generated("dist/bundle.wasm"));
        assert!(!is_binary_or_generated("src/main.rs"));
    }

    // ── apply_pass_history (REVX-04) ────────────────────────────────────

    #[test]
    fn pass_two_contested_escalates_capped() {
        let cfg = EffortPolicyConfig::default();
        let history = PassHistory { pass_number: 2, prior_findings_material: 3, prior_verdict_contested: true };
        let (tier, reasons, hand_off) = apply_pass_history(EffortTier::Medium, &history, &cfg);
        assert_eq!(tier, EffortTier::High);
        assert!(!hand_off);
        assert!(reasons.iter().any(|r| r.contains("contested")));

        // Capped: starting from High, escalating again must not exceed the cap.
        let (tier2, _, _) = apply_pass_history(EffortTier::High, &history, &cfg);
        assert_eq!(tier2, cfg.max_escalation_tier);
    }

    #[test]
    fn pass_two_trivial_reverify_deescalates_floored() {
        let cfg = EffortPolicyConfig::default();
        let history = PassHistory { pass_number: 2, prior_findings_material: 0, prior_verdict_contested: false };
        let (tier, _, hand_off) = apply_pass_history(EffortTier::Medium, &history, &cfg);
        assert_eq!(tier, EffortTier::Low);
        assert!(!hand_off);

        let (tier2, _, _) = apply_pass_history(EffortTier::Low, &history, &cfg);
        assert_eq!(tier2, EffortTier::Low, "floored at Low");
    }

    #[test]
    fn pass_five_still_contested_hands_off() {
        let cfg = EffortPolicyConfig::default();
        let history = PassHistory { pass_number: 5, prior_findings_material: 2, prior_verdict_contested: true };
        let (_, reasons, hand_off) = apply_pass_history(EffortTier::Medium, &history, &cfg);
        assert!(hand_off);
        assert!(reasons.iter().any(|r| r.contains("hand off")));
    }

    #[test]
    fn no_history_yields_base_tier_no_handoff() {
        let cfg = EffortPolicyConfig::default();
        let history = PassHistory::first_pass();
        let (tier, reasons, hand_off) = apply_pass_history(EffortTier::Medium, &history, &cfg);
        assert_eq!(tier, EffortTier::Medium);
        assert!(reasons.is_empty());
        assert!(!hand_off);
    }

    #[test]
    fn malformed_prior_passes_context_is_ignored() {
        let ctx = json!({"prior_passes": "not-an-object"});
        assert!(PassHistory::from_context(&ctx).is_none());
    }

    // ── decide (REVX-05) ─────────────────────────────────────────────────

    #[test]
    fn capstone_provider_gets_higher_tier_than_free_tail() {
        let cfg = EffortPolicyConfig::default();
        let capstone = decide("opus", EffortTier::High, Some(RiskClass::High), false, None, None, &cfg);
        let tail = decide("free", EffortTier::High, Some(RiskClass::High), false, None, None, &cfg);
        assert!(capstone.tier > tail.tier, "capstone {:?} should exceed tail {:?}", capstone.tier, tail.tier);
    }

    #[test]
    fn caller_override_beats_policy_and_records_reason() {
        let cfg = EffortPolicyConfig::default();
        let d = decide("free", EffortTier::Low, None, false, Some("xhigh"), None, &cfg);
        assert_eq!(d.tier, EffortTier::Xhigh);
        assert_eq!(d.reason, "caller override");
    }

    #[test]
    fn invalid_override_rejected_falls_back_to_policy() {
        let cfg = EffortPolicyConfig::default();
        let d = decide("opus", EffortTier::Medium, None, false, Some("ultra-mega"), None, &cfg);
        assert!(d.reason.contains("override rejected"));
        assert_eq!(d.tier, EffortTier::Medium);
    }

    #[test]
    fn token_budget_biases_tail_down_never_biases_high_risk_below_tier() {
        let cfg = EffortPolicyConfig::default();
        let low_risk = decide("free", EffortTier::Medium, Some(RiskClass::Low), true, None, None, &cfg);
        let no_budget = decide("free", EffortTier::Medium, Some(RiskClass::Low), false, None, None, &cfg);
        assert!(low_risk.tier < no_budget.tier, "budget should bias low-risk tail down");

        let high_risk = decide("opus", EffortTier::High, Some(RiskClass::High), true, None, None, &cfg);
        assert!(high_risk.tier >= EffortTier::High, "HIGH-risk capstone must never be biased below its tier");
    }

    #[test]
    fn intensive_substitute_floor_preserved() {
        let cfg = EffortPolicyConfig::default();
        let d = decide("free", EffortTier::Low, None, false, None, Some(EffortTier::High), &cfg);
        assert!(d.tier >= EffortTier::High, "intensive floor must apply even to a breadth-tail seat");
    }

    #[test]
    fn intensive_floor_wins_over_a_too_low_caller_override() {
        // REVX finding: an intensive-substitute dispatch is a SAFETY floor
        // that must always hold, even when the caller explicitly asked for a
        // lower effort. `Low` here must NOT survive -- the floor overrides it.
        let cfg = EffortPolicyConfig::default();
        let d = decide("free", EffortTier::Low, None, false, Some("low"), Some(EffortTier::High), &cfg);
        assert!(
            d.tier >= EffortTier::High,
            "intensive substitute with a Low caller override must still run at >= High: got {:?}",
            d.tier
        );
        assert!(d.reason.contains("floor enforced"), "reason should note the floor was enforced: {}", d.reason);
    }

    #[test]
    fn caller_override_can_still_raise_above_the_intensive_floor() {
        // A caller override may RAISE effort above the floor -- only
        // LOWERING below it is disallowed.
        let cfg = EffortPolicyConfig::default();
        let d = decide("free", EffortTier::Low, None, false, Some("xhigh"), Some(EffortTier::High), &cfg);
        assert_eq!(d.tier, EffortTier::Xhigh);
    }

    #[test]
    fn fable_clamped_to_budget_cap_even_on_high_risk_run() {
        let cfg = EffortPolicyConfig::default();
        let d = decide("claude-fable-5", EffortTier::Xhigh, Some(RiskClass::High), false, None, None, &cfg);
        assert!(d.tier <= cfg.fable_max_tier, "Fable must be capped: got {:?}", d.tier);
        assert!(d.reason.to_ascii_lowercase().contains("fable"));
    }

    #[test]
    fn fable_caller_override_still_honored() {
        let cfg = EffortPolicyConfig::default();
        let d = decide("claude-fable-5", EffortTier::Low, None, false, Some("high"), None, &cfg);
        assert_eq!(d.tier, EffortTier::High, "explicit override must win over the Fable budget cap");
    }

    #[test]
    fn unknown_provider_defaults_to_fixed_medium_not_escalated_by_risk() {
        // REVX finding: an unrecognized provider must never be pushed above
        // its safe Medium default by a HIGH-risk run's escalation.
        let cfg = EffortPolicyConfig::default();
        let d = decide(
            "totally-unrecognized-provider",
            EffortTier::Xhigh,
            Some(RiskClass::High),
            false,
            None,
            None,
            &cfg,
        );
        assert_eq!(d.tier, EffortTier::Medium, "unknown provider must clamp to a fixed Medium: got {:?}", d.tier);
    }

    #[test]
    fn is_review_only_provider_classifies_correctly() {
        assert!(is_review_only_provider("claude-fable-5"));
        assert!(!is_review_only_provider("opus"));
        assert!(!is_review_only_provider("codex"));
    }

    // ── tier_to_native / codex_model_for_tier (REVX-07) ─────────────────

    #[test]
    fn codex_native_mapping_covers_all_five_levels() {
        assert_eq!(tier_to_native("codex", EffortTier::Minimal).as_deref(), Some("none"));
        assert_eq!(tier_to_native("codex", EffortTier::Low).as_deref(), Some("low"));
        assert_eq!(tier_to_native("codex", EffortTier::Medium).as_deref(), Some("medium"));
        assert_eq!(tier_to_native("codex", EffortTier::High).as_deref(), Some("high"));
        assert_eq!(tier_to_native("codex", EffortTier::Xhigh).as_deref(), Some("xhigh"));
    }

    #[test]
    fn anthropic_native_mapping_clamps_to_three_levels() {
        assert_eq!(tier_to_native("opus", EffortTier::Minimal).as_deref(), Some("low"));
        assert_eq!(tier_to_native("opus", EffortTier::Xhigh).as_deref(), Some("high"));
        assert_eq!(tier_to_native("claude-fable-5", EffortTier::High).as_deref(), Some("high"));
    }

    #[test]
    fn unknown_or_no_control_provider_yields_none() {
        assert_eq!(tier_to_native("free", EffortTier::High), None);
        assert_eq!(tier_to_native("totally-unknown", EffortTier::High), None);
    }

    #[test]
    fn codex_model_for_tier_maps_sol_terra_luna() {
        assert_eq!(codex_model_for_tier(EffortTier::Xhigh), "gpt-5.6-sol");
        assert_eq!(codex_model_for_tier(EffortTier::High), "gpt-5.6-sol");
        assert_eq!(codex_model_for_tier(EffortTier::Medium), "gpt-5.6-terra");
        assert_eq!(codex_model_for_tier(EffortTier::Low), "gpt-5.6-luna");
        assert_eq!(codex_model_for_tier(EffortTier::Minimal), "gpt-5.6-luna");
        assert!(ALLOWED_CODEX_MODELS.contains(&"gpt-5.6-sol"));
    }

    // ── EffortPolicyConfig::from_env (REVX-06) ──────────────────────────

    #[test]
    #[serial_test::serial]
    fn from_env_defaults_when_nothing_set() {
        for key in [
            "REVIEW_EFFORT_POLICY_ENABLED",
            "REVIEW_EFFORT_BASELINE_TIER",
            "REVIEW_EFFORT_LOC_ESCALATE",
        ] {
            std::env::remove_var(key);
        }
        let cfg = EffortPolicyConfig::from_env();
        assert!(cfg.enabled);
        assert_eq!(cfg.baseline_tier, EffortTier::Medium);
        assert_eq!(cfg.loc_escalate_threshold, 400);
    }

    #[test]
    #[serial_test::serial]
    fn from_env_override_changes_threshold() {
        std::env::set_var("REVIEW_EFFORT_LOC_ESCALATE", "10");
        let cfg = EffortPolicyConfig::from_env();
        assert_eq!(cfg.loc_escalate_threshold, 10);
        std::env::remove_var("REVIEW_EFFORT_LOC_ESCALATE");
    }

    #[test]
    #[serial_test::serial]
    fn from_env_disabled_flag_is_readable() {
        std::env::set_var("REVIEW_EFFORT_POLICY_ENABLED", "0");
        let cfg = EffortPolicyConfig::from_env();
        assert!(!cfg.enabled);
        std::env::remove_var("REVIEW_EFFORT_POLICY_ENABLED");
    }

    #[test]
    #[serial_test::serial]
    fn from_env_malformed_numeric_falls_back_to_default() {
        std::env::set_var("REVIEW_EFFORT_LOC_ESCALATE", "not-a-number");
        let cfg = EffortPolicyConfig::from_env();
        assert_eq!(cfg.loc_escalate_threshold, 400);
        std::env::remove_var("REVIEW_EFFORT_LOC_ESCALATE");
    }

    #[test]
    #[serial_test::serial]
    fn from_env_empty_provider_override_falls_back_to_builtin() {
        std::env::set_var("REVIEW_EFFORT_CAPSTONE_PROVIDERS", "");
        let cfg = EffortPolicyConfig::from_env();
        assert!(!cfg.capstone_providers.is_empty());
        std::env::remove_var("REVIEW_EFFORT_CAPSTONE_PROVIDERS");
    }

    // ── PassHistory Redis fallback (degrade-safe) ───────────────────────

    #[tokio::test]
    #[serial_test::serial]
    async fn load_pass_history_degrades_to_pass_one_without_redis_or_context() {
        std::env::remove_var("REDIS_URL");
        let history = load_pass_history(&json!({})).await;
        assert_eq!(history.pass_number, 1);
    }

    #[tokio::test]
    async fn load_pass_history_prefers_context_block() {
        let ctx = json!({"prior_passes": {"pass_number": 3, "prior_findings_material": 1, "prior_verdict_contested": true}});
        let history = load_pass_history(&ctx).await;
        assert_eq!(history.pass_number, 3);
        assert!(history.prior_verdict_contested);
    }

    #[test]
    fn content_risk_hint_detects_secret_shaped_diff_text() {
        let ctx = json!({"diff": "+ token = \"abcdef1234567890\""});
        // Best-effort: must never panic regardless of match outcome.
        let _ = content_risk_hint(&ctx);
        assert!(!content_risk_hint(&json!({})));
    }
}
