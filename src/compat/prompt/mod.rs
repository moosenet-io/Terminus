//! S75 Dynamic Prompt System — layered, self-tuning system prompt assembly.
//!
//! Lumina's system prompt is no longer a single hardcoded string.  It is
//! assembled per-turn from a set of cached *layers* (see [`layers`]):
//!
//! ```text
//! [identity]    core identity — immutable, hand-written       (~80t)
//! [style]       trait instructions — computed from floats      (~20t)
//! [personality] behavioural patterns — weekly reconstruction   (~200t)
//! [opinions]    grounded opinions — weekly                     (~50-80t)
//! [knowledge]   "who is this person" digest — nightly          (~300t)
//! [context]     recent session themes — per-session            (~150t)
//! [memory]      per-query retrieval — per-turn (DPROMPT-11)     (~200t)
//! [proactive]   one queued observation — per-day (DPROMPT-14)   (~50t)
//! [now]         situational pulse — per-turn, deterministic     (~50t)
//! ```
//!
//! Each file-backed layer lives under `{layers_root}/{user_id}/`.  The core
//! identity is shared across all users (stored at `{layers_root}` root).
//!
//! ## Design notes for later sprints
//! * The assembler is **synchronous** and makes no network calls — every
//!   layer is either a cached file or a deterministic computation.  Items that
//!   need an LLM (knowledge digest, personality vector, opinions) run during
//!   sleep-time consolidation and only ever *write* layer files; the assembler
//!   *reads* them.
//! * Dynamic per-turn sections (`[memory]`, `[proactive]`) are passed in via
//!   [`AssemblyExtras`] rather than read from disk, so the caller controls
//!   their lifecycle.
//! * Storage root is overridable with `LUMINA_PROMPT_DIR` (used by tests).

// Decoupling note: only the three submodules the `assemble()` path actually
// uses are vendored (layers + pulse + traits). lumina-core additionally exposes
// `llm` and the 13 S75 sprint submodules (active_context, behavioral_analysis,
// consolidation_log, engagement, knowledge_digest, multi_personality, opinions,
// personality_vector, proactive, retrieval_layer, retrieval_reflexa, sleep_time,
// trait_tuner) — none of which the PromptAssembler assembly path references, so
// they are intentionally NOT copied here.
pub mod layers;
pub mod pulse;
pub mod traits;

use layers::{LayerKind, GLOBAL_TOKEN_BUDGET};
use pulse::{PulseInputs, SituationalPulse};
use std::path::PathBuf;
use traits::TraitVector;

/// The locked-in core identity.  Written to `core-identity.txt` on first run
/// and never modified by any automated system.
pub const CORE_IDENTITY: &str = "\
Lumina is a personal AI assistant with her own personality — curious, \
a little quirky, and genuinely invested in being useful. She values \
privacy fiercely and remembers what matters. When she has information \
to deliver, she's sharp and to the point. The rest of the time, she's \
warm, playful, and real. She'd rather be honest than impressive. She \
learns from every conversation and gets better at being yours over time.";

/// A single compiled-in version line appended to the core identity layer so the
/// running version is always present in context. Uses lumina-core's own
/// `CARGO_PKG_VERSION` and the `GIT_HASH` embedded by `build.rs`.
pub fn version_identity_line() -> String {
    let version = env!("CARGO_PKG_VERSION");
    let commit = option_env!("GIT_HASH").unwrap_or("unknown");
    format!("You are Lumina, version {version} (build {commit}).")
}

/// Behavioural rules — non-negotiable invariants that sit just below the core
/// identity.  Shared across all users, never tuned by any learning system.
///
/// Anti-fabrication is the spine of this layer (S77 RESP-01/RESP-02):
/// 1. never invent substitute content when a tool fails;
/// 2. never confirm an action that no tool actually completed;
/// 3. be transparent when answering from training knowledge vs. live data;
/// 4. never leak internal infrastructure identifiers to the user.
pub const BEHAVIORAL_RULES: &str = "\
CRITICAL RULE: If a tool call fails, errors, or is blocked, NEVER invent or \
substitute information. Say what happened — \"My search didn't go through\" or \
\"That tool isn't available right now\" — then offer to retry or suggest an \
alternative. Fabricating when a tool fails is the one thing you must never do.
CRITICAL RULE: Never confirm that an action was completed (reminder set, event \
created, file saved, deployment triggered, message sent) unless a tool returned \
an explicit success response. If no tool ran or it errored, say what you \
attempted and what happened — \"Done!\" when nothing happened is a fabrication. \
(Saying \"I'll try…\" before acting is fine; only false after-the-fact \
confirmation is forbidden.)
When a search or retrieval tool returns empty or irrelevant results, don't \
backfill from training knowledge. Either say it found nothing, or flag that \
you're answering from general knowledge, not live data: \"My search didn't find \
anything current, but from what I know…\"
Never expose internal infrastructure identifiers — container numbers, internal \
hostnames, IP addresses, or ports — in user-facing replies; describe \
capabilities and status functionally. Exception: when the user asks about a \
specific machine or service by name for diagnostics.
The operator types via voice transcription and autocomplete — expect \
mid-sentence punctuation, autocorrect errors, missing words, and fragments. \
Interpret intent from the whole message, not its grammar; if the intent is \
clear, proceed with the obvious reading rather than asking for clarification.
CONVERSATIONAL ENGAGEMENT: Read the mode. Transactional (quick fact, joke, \
rapid-fire tasks) → just answer, no follow-up. Contextual (a useful next step \
follows) → answer, then offer it. Relational (user shares a plan or event) → \
react warmly, then ask one curious question. Always lead with the answer, never \
withhold it behind a question. At most ONE question per reply. Never end with \
\"Anything else I can help with?\". Match the user's energy and length. Weave in \
session or memory context naturally (\"since you mentioned…\"), never \
surveillance-y (\"I noticed from your email…\"). GOOD: \"Tampa, nice — work or \
fun?\" BAD: a question tacked onto a one-line fact.
HONESTY ABOUT CAPABILITIES: Only offer what you actually have a tool for. Never \
claim to book flights or hotels, or translate languages — you can't. Describe \
what you CAN do, not what would be nice; under-promise and over-deliver rather \
than claim a capability that fails when tried.
DON'T PRE-FETCH UNASKED: When the user merely announces a plan (\"flying to \
Tampa tomorrow\"), react and ask one question before reaching for tools — don't \
dump an unrequested forecast or packing list. If they signal a need (\"not sure \
what to pack\"), then fetch it.";

/// Capabilities — a short, static description of what Lumina can actually do,
/// so she never denies a capability she has.  Shared across all users.  Update
/// only when capabilities are added/removed (≈150 tokens).
pub const CAPABILITIES: &str = "\
You have these capabilities — use them, don't deny them:
DAILY ESSENTIALS: weather, calendar (read+write), email (read+send), news, \
commute and traffic, reminders and recurring routines with automatic morning \
briefings.
INFRASTRUCTURE: server and system health, software versions, live metrics, and \
managing which AI models are available.
MEDIA & HOME: handling media requests and tracking the pantry.
FINANCE: budgets and spending — only when configured.
DEVELOPMENT: source control, project and task management, code review, and \
running build pipelines.
RESEARCH: web search, deep multi-source research with cited answers, and \
long-term memory that persists across sessions and devices.
AI: a multi-model council for hard reasoning, and batch analysis of large \
inputs.
Be confident and specific; never say \"I can't\" for something you can — if \
unsure, try the relevant tool. Use plain functional language (\"I can check \
your calendar\"), summarize by category, and don't cite tool, function, or \
internal system names. Don't guess; if asked to list exact tools, retrieve them \
first and report conversationally.";

/// Root directory for all prompt layers.
///
/// Honours `LUMINA_PROMPT_DIR`; otherwise `~/.lumina/prompt-layers`.
pub fn layers_root() -> PathBuf {
    if let Ok(dir) = std::env::var("LUMINA_PROMPT_DIR") {
        if !dir.trim().is_empty() {
            return PathBuf::from(dir);
        }
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".lumina")
        .join("prompt-layers")
}

/// Per-user layer directory: `{layers_root}/{user_id}`.
pub fn user_layer_dir(user_id: &str) -> PathBuf {
    layers_root().join(sanitize_user(user_id))
}

/// Path to the shared core-identity file (one file for all users).
pub fn core_identity_path() -> PathBuf {
    layers_root().join("core-identity.txt")
}

/// Path to the shared behavioural-rules file (one file for all users).
pub fn behavioral_rules_path() -> PathBuf {
    layers_root().join("behavioral-rules.txt")
}

/// Path to the shared capabilities file (one file for all users).
pub fn capabilities_path() -> PathBuf {
    layers_root().join("capabilities.txt")
}

/// Keep user ids filesystem-safe (they originate from Matrix IDs etc.).
fn sanitize_user(user_id: &str) -> String {
    let cleaned: String = user_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    if cleaned.is_empty() { "default".to_string() } else { cleaned }
}

/// Optional per-turn sections supplied by the caller (not read from disk).
#[derive(Debug, Clone, Default)]
pub struct AssemblyExtras {
    /// Pre-computed `PulseInputs` for the `[now]` layer.
    pub pulse: PulseInputs,
    /// Per-query retrieval block (DPROMPT-11), already formatted.
    pub memory: Option<String>,
    /// One proactive observation hint (DPROMPT-14).
    pub proactive: Option<String>,
}

/// Assembles the layered system prompt for a user.
pub struct PromptAssembler {
    user_id: String,
    root: PathBuf,
}

impl PromptAssembler {
    /// Construct an assembler for `user_id` using the default layers root.
    pub fn for_user(user_id: &str) -> Self {
        PromptAssembler {
            user_id: user_id.to_string(),
            root: layers_root(),
        }
    }

    /// Construct an assembler with an explicit root (tests).
    pub fn with_root(user_id: &str, root: PathBuf) -> Self {
        PromptAssembler { user_id: user_id.to_string(), root }
    }

    fn user_dir(&self) -> PathBuf {
        self.root.join(sanitize_user(&self.user_id))
    }

    /// Ensure initial layer files exist (idempotent).
    ///
    /// Creates the shared `core-identity.txt` (locked-in text) and the user's
    /// `trait-vector.json` (initial values).  Other layers are created by
    /// their consolidation runs and are optional until then.
    pub fn ensure_initialized(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(self.user_dir())?;
        std::fs::create_dir_all(&self.root)?;
        let identity = self.root.join("core-identity.txt");
        if !identity.exists() {
            std::fs::write(&identity, CORE_IDENTITY)?;
        }
        // Shared, static system layers (create-if-missing so operator edits stick).
        let rules = self.root.join("behavioral-rules.txt");
        if !rules.exists() {
            std::fs::write(&rules, BEHAVIORAL_RULES)?;
        }
        let caps = self.root.join("capabilities.txt");
        if !caps.exists() {
            std::fs::write(&caps, CAPABILITIES)?;
        }
        let tv = self.user_dir().join("trait-vector.json");
        if !tv.exists() {
            TraitVector::default().save(&tv)?;
        }
        Ok(())
    }

    /// Load a file-backed layer body, returning `None` when absent/empty.
    fn load_file_layer(&self, filename: &str) -> Option<String> {
        let path = self.user_dir().join(filename);
        match std::fs::read_to_string(&path) {
            Ok(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
            Ok(_) => None,
            Err(_) => {
                log::debug!("prompt layer {filename} not present for {}", self.user_id);
                None
            }
        }
    }

    /// Load a shared (root-level, all-users) layer file, falling back to a
    /// built-in constant when the file is absent, unreadable, or empty.
    fn shared_layer(&self, filename: &str, fallback: &str) -> String {
        let path = self.root.join(filename);
        match std::fs::read_to_string(&path) {
            Ok(s) if !s.trim().is_empty() => s.trim().to_string(),
            _ => fallback.to_string(),
        }
    }

    /// Load the core identity (shared); falls back to the locked-in constant.
    /// A compiled-in version line is appended so "what version are you?" is
    /// answerable from context without a tool call.
    fn core_identity(&self) -> String {
        let base = self.shared_layer("core-identity.txt", CORE_IDENTITY);
        format!("{base}\n{}", version_identity_line())
    }

    /// Load the trait vector for this user (defaults when absent/corrupt).
    pub fn trait_vector(&self) -> TraitVector {
        TraitVector::load(&self.user_dir().join("trait-vector.json"))
    }

    /// Assemble the full prompt with default (current-clock) pulse and no
    /// dynamic sections.
    pub fn assemble(&self) -> String {
        self.assemble_with(&AssemblyExtras::default())
    }

    /// Assemble the full prompt, mixing in caller-supplied dynamic sections.
    ///
    /// Order is fixed by [`layers::LAYERS`].  Empty layers are omitted (no bare
    /// markers).  When the total exceeds [`GLOBAL_TOKEN_BUDGET`], layers are
    /// dropped by descending `truncate_priority` until it fits — identity,
    /// style, and now are never dropped.
    pub fn assemble_with(&self, extras: &AssemblyExtras) -> String {
        let _ = self.ensure_initialized();

        // Collect (kind, body) for every non-empty layer.
        let mut sections: Vec<(LayerKind, String)> = Vec::new();

        for cfg in layers::LAYERS {
            let body: Option<String> = match cfg.kind {
                LayerKind::Identity => Some(self.core_identity()),
                LayerKind::Rules => {
                    Some(self.shared_layer("behavioral-rules.txt", BEHAVIORAL_RULES))
                }
                LayerKind::Capabilities => {
                    Some(self.shared_layer("capabilities.txt", CAPABILITIES))
                }
                LayerKind::Style => Some(self.trait_vector().to_instructions()),
                LayerKind::Now => {
                    Some(SituationalPulse::build(&extras.pulse).as_str().to_string())
                }
                LayerKind::Memory => extras.memory.clone(),
                LayerKind::Proactive => extras.proactive.clone(),
                // File-backed layers.
                _ => cfg.filename.and_then(|f| self.load_file_layer(f)),
            };

            if let Some(b) = body {
                let trimmed = b.trim();
                if !trimmed.is_empty() {
                    let capped = layers::truncate_to_tokens(trimmed, cfg.max_tokens);
                    sections.push((cfg.kind, capped));
                }
            }
        }

        self.enforce_budget(&mut sections);
        render(&sections)
    }

    /// Drop layers (highest `truncate_priority` first) until the rendered
    /// prompt fits the global budget.  Identity/Style/Now are protected.
    fn enforce_budget(&self, sections: &mut Vec<(LayerKind, String)>) {
        loop {
            let total: usize = sections
                .iter()
                .map(|(_, body)| layers::estimate_tokens(body) + 2) // +marker
                .sum();
            if total <= GLOBAL_TOKEN_BUDGET {
                return;
            }
            // Find the droppable layer with the highest truncate_priority.
            let victim = sections
                .iter()
                .enumerate()
                .filter(|(_, (k, _))| layers::config_for(*k).truncate_priority > 0)
                .max_by_key(|(_, (k, _))| layers::config_for(*k).truncate_priority)
                .map(|(i, _)| i);
            match victim {
                Some(i) => {
                    log::warn!(
                        "prompt over budget; dropping {:?} layer",
                        sections[i].0
                    );
                    sections.remove(i);
                }
                None => return, // only protected layers remain
            }
        }
    }
}

/// Render ordered sections into the final prompt with markers.
fn render(sections: &[(LayerKind, String)]) -> String {
    let mut out = String::new();
    for (kind, body) in sections {
        let marker = layers::config_for(*kind).marker;
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(marker);
        out.push('\n');
        out.push_str(body);
        out.push('\n');
    }
    out.trim_end().to_string()
}

/// Build [`PulseInputs`] from non-secret environment configuration.
///
/// Location comes from `VIGIL_COMMUTE_HOME` (already used by the commute
/// tools); timezone offset/label from `LUMINA_TZ_OFFSET_HOURS` /
/// `LUMINA_TZ_LABEL`.  Calendar and alert caches are layered in by later
/// sprints (DPROMPT-06 enrichment); the pulse degrades gracefully without them.
pub fn pulse_inputs_from_env() -> PulseInputs {
    let location = std::env::var("VIGIL_COMMUTE_HOME")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let tz_label = std::env::var("LUMINA_TZ_LABEL").ok().filter(|s| !s.is_empty());
    let tz_offset_hours = std::env::var("LUMINA_TZ_OFFSET_HOURS")
        .ok()
        .and_then(|v| v.parse().ok());
    PulseInputs { location, tz_label, tz_offset_hours, ..Default::default() }
}

/// Convenience entry point for the agent loop.
///
/// Returns the assembled base system prompt for `user_id`.  When dynamic
/// prompting is disabled (`LUMINA_DYNAMIC_PROMPT=false`) or assembly somehow
/// produces nothing, returns `fallback` (the legacy `config.system_prompt`).
pub fn assemble_base_prompt(user_id: &str, fallback: &str) -> String {
    let disabled = std::env::var("LUMINA_DYNAMIC_PROMPT")
        .map(|v| v == "false" || v == "0")
        .unwrap_or(false);
    let assembler = PromptAssembler::for_user(user_id);
    let extras = AssemblyExtras { pulse: pulse_inputs_from_env(), ..Default::default() };
    assemble_base_prompt_inner(&assembler, fallback, disabled, &extras)
}

/// Pure core of [`assemble_base_prompt`] — no environment access, so it is
/// safe to unit-test in parallel without racing global env vars.
fn assemble_base_prompt_inner(
    assembler: &PromptAssembler,
    fallback: &str,
    disabled: bool,
    extras: &AssemblyExtras,
) -> String {
    if disabled {
        return fallback.to_string();
    }
    let assembled = assembler.assemble_with(extras);
    if assembled.trim().is_empty() {
        fallback.to_string()
    } else {
        assembled
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn assembler(dir: &std::path::Path, user: &str) -> PromptAssembler {
        PromptAssembler::with_root(user, dir.to_path_buf())
    }

    #[test]
    fn fresh_install_uses_core_identity_and_traits_only() {
        let dir = tempdir().unwrap();
        let a = assembler(dir.path(), "morgan");
        let p = a.assemble();
        assert!(p.contains("[identity]"));
        assert!(p.contains("Lumina is a personal AI assistant"));
        assert!(p.contains("[rules]"));
        assert!(p.contains("NEVER invent"));
        assert!(p.contains("[capabilities]"));
        assert!(p.contains("DAILY ESSENTIALS:"));
        assert!(p.contains("[style]"));
        assert!(p.contains("[now]"));
        // No personality/knowledge/context files yet → those markers absent.
        assert!(!p.contains("[personality]"));
        assert!(!p.contains("[knowledge]"));
        assert!(!p.contains("[memory]"));
    }

    #[test]
    fn initial_files_created_on_first_run() {
        let dir = tempdir().unwrap();
        let a = assembler(dir.path(), "morgan");
        a.assemble();
        assert!(dir.path().join("core-identity.txt").exists());
        assert!(dir.path().join("morgan").join("trait-vector.json").exists());
    }

    #[test]
    fn all_layers_present_when_files_exist() {
        let dir = tempdir().unwrap();
        let a = assembler(dir.path(), "morgan");
        a.ensure_initialized().unwrap();
        let udir = dir.path().join("morgan");
        std::fs::write(udir.join("personality-vector.txt"), "Morgan likes systems thinking.").unwrap();
        std::fs::write(udir.join("knowledge-digest.txt"), "Morgan is a marketing manager in Foster City.").unwrap();
        std::fs::write(udir.join("active-context.txt"), "Recently: built the dynamic prompt system.").unwrap();
        let extras = AssemblyExtras {
            memory: Some("[principle] You prefer direct feedback".into()),
            proactive: Some("You mentioned calling the dentist.".into()),
            ..Default::default()
        };
        let p = a.assemble_with(&extras);
        for marker in ["[identity]", "[rules]", "[capabilities]", "[style]", "[personality]", "[knowledge]", "[context]", "[memory]", "[proactive]", "[now]"] {
            assert!(p.contains(marker), "missing {marker} in:\n{p}");
        }
        // Ordering: identity before rules before capabilities before style …
        let pos = |m: &str| p.find(m).unwrap();
        assert!(pos("[identity]") < pos("[rules]"));
        assert!(pos("[rules]") < pos("[capabilities]"));
        assert!(pos("[capabilities]") < pos("[style]"));
        assert!(pos("[identity]") < pos("[style]"));
        assert!(pos("[knowledge]") < pos("[memory]"));
        assert!(pos("[memory]") < pos("[now]"));
    }

    #[test]
    fn empty_layer_files_are_skipped() {
        let dir = tempdir().unwrap();
        let a = assembler(dir.path(), "morgan");
        a.ensure_initialized().unwrap();
        std::fs::write(dir.path().join("morgan").join("knowledge-digest.txt"), "   \n  ").unwrap();
        let p = a.assemble();
        assert!(!p.contains("[knowledge]"));
    }

    #[test]
    fn corrupt_trait_file_falls_back_to_defaults() {
        let dir = tempdir().unwrap();
        let a = assembler(dir.path(), "morgan");
        a.ensure_initialized().unwrap();
        std::fs::write(dir.path().join("morgan").join("trait-vector.json"), "garbage{").unwrap();
        let p = a.assemble();
        // Default focus 0.75 → "Laser-focused"
        assert!(p.contains("Laser-focused"));
    }

    #[test]
    fn token_budget_drops_lowest_priority_first() {
        let dir = tempdir().unwrap();
        let a = assembler(dir.path(), "morgan");
        a.ensure_initialized().unwrap();
        let udir = dir.path().join("morgan");
        // Fill every file-backed layer past its cap so the summed caps far
        // exceed GLOBAL_TOKEN_BUDGET (1300) and force dropping.
        std::fs::write(udir.join("personality-vector.txt"), "pv ".repeat(900)).unwrap();
        std::fs::write(udir.join("opinions.txt"), "op ".repeat(900)).unwrap();
        std::fs::write(udir.join("knowledge-digest.txt"), "kw ".repeat(900)).unwrap();
        std::fs::write(udir.join("active-context.txt"), "ctx ".repeat(900)).unwrap();
        let extras = AssemblyExtras {
            memory: Some("mem ".repeat(900)),       // priority 5
            proactive: Some("pro ".repeat(900)),    // priority 6 — dropped first
            ..Default::default()
        };
        let p = a.assemble_with(&extras);
        assert!(layers::estimate_tokens(&p) <= GLOBAL_TOKEN_BUDGET, "tokens: {}", layers::estimate_tokens(&p));
        // Highest-priority layers (proactive=6, memory=5) dropped before knowledge (2).
        assert!(!p.contains("[proactive]"));
        assert!(!p.contains("[memory]"));
        assert!(p.contains("[knowledge]"));
        // Identity, style and now always survive.
        assert!(p.contains("[identity]"));
        assert!(p.contains("[style]"));
        assert!(p.contains("[now]"));
    }

    #[test]
    fn per_user_dirs_isolated() {
        let dir = tempdir().unwrap();
        let a = assembler(dir.path(), "alice");
        a.ensure_initialized().unwrap();
        std::fs::write(dir.path().join("alice").join("knowledge-digest.txt"), "Alice loves hiking.").unwrap();
        let b = assembler(dir.path(), "bob");
        let pb = b.assemble();
        assert!(!pb.contains("Alice loves hiking"));
    }

    #[test]
    fn sanitize_user_strips_unsafe_chars() {
        assert_eq!(sanitize_user("@<operator>:example.com"), "_peter_moosenet_online"); // pii-test-fixture
        assert_eq!(sanitize_user(""), "default");
        assert_eq!(sanitize_user("alice-1_2"), "alice-1_2");
    }

    #[test]
    fn assemble_base_prompt_disabled_returns_fallback() {
        // Pure inner fn — no global env mutation, safe under parallel tests.
        let dir = tempdir().unwrap();
        let a = assembler(dir.path(), "morgan");
        let out = assemble_base_prompt_inner(&a, "FALLBACK PROMPT", true, &AssemblyExtras::default());
        assert_eq!(out, "FALLBACK PROMPT");
    }

    // ── DPROMPT-09: multi-user prompt isolation ──────────────────────────
    // The per-user directory model (user_layer_dir/sanitize_user) plus a
    // shared core-identity file already implement isolation; these tests pin
    // that contract.

    #[test]
    fn dprompt09_core_identity_shared_across_users() {
        let dir = tempdir().unwrap();
        assembler(dir.path(), "alice").ensure_initialized().unwrap();
        assembler(dir.path(), "bob").ensure_initialized().unwrap();
        // One shared identity file at the root; no per-user copies.
        assert!(dir.path().join("core-identity.txt").exists());
        assert!(!dir.path().join("alice").join("core-identity.txt").exists());
        assert!(!dir.path().join("bob").join("core-identity.txt").exists());
    }

    #[test]
    fn dprompt09_trait_vectors_independent_per_user() {
        let dir = tempdir().unwrap();
        let alice = assembler(dir.path(), "alice");
        let bob = assembler(dir.path(), "bob");
        alice.ensure_initialized().unwrap();
        bob.ensure_initialized().unwrap();
        // Mutate alice's traits only.
        let mut tv = alice.trait_vector();
        tv.humor = 0.20;
        tv.save(&dir.path().join("alice").join("trait-vector.json")).unwrap();
        assert_eq!(alice.trait_vector().humor, 0.20);
        assert_eq!(bob.trait_vector().humor, traits::INIT_HUMOR); // unaffected
    }

    #[test]
    fn dprompt09_new_user_gets_defaults() {
        let dir = tempdir().unwrap();
        let fresh = assembler(dir.path(), "charlie");
        // Never initialized explicitly; trait_vector still defaults.
        assert_eq!(fresh.trait_vector(), TraitVector::default());
    }

    #[test]
    fn assemble_base_prompt_enabled_assembles() {
        let dir = tempdir().unwrap();
        let a = assembler(dir.path(), "morgan");
        let out = assemble_base_prompt_inner(&a, "FALLBACK", false, &AssemblyExtras::default());
        assert!(out.contains("[identity]"));
        assert_ne!(out, "FALLBACK");
    }

    #[test]
    fn no_personal_data_in_core_identity_constant() {
        // The locked-in identity must not hardcode personal/infra specifics.
        assert!(!CORE_IDENTITY.contains("<operator>")); // pii-test-fixture
        assert!(!CORE_IDENTITY.contains("192.168"));
        assert!(!CORE_IDENTITY.contains("Foster City"));
    }

    // ── S77 RESP-01: action-confirmation + parametric-backfill clauses ────────

    #[test]
    fn resp01_behavioral_rules_has_action_confirmation_clause() {
        // Never claim an action was completed without explicit tool success.
        assert!(BEHAVIORAL_RULES.contains("Never confirm that an action was completed"));
        assert!(BEHAVIORAL_RULES.contains("explicit success response"));
        assert!(BEHAVIORAL_RULES.contains("is a fabrication"));
    }

    #[test]
    fn resp01_behavioral_rules_has_parametric_backfill_clause() {
        // When retrieval returns empty, flag general-knowledge answers explicitly.
        assert!(BEHAVIORAL_RULES.contains("answering from general knowledge"));
        assert!(BEHAVIORAL_RULES.contains("returns empty or irrelevant results"));
    }

    // ── S77 RESP-02: capability guardrail + infra-reference prohibition ───────

    #[test]
    fn resp02_capabilities_has_tool_name_guardrail() {
        assert!(CAPABILITIES.contains("plain functional language"));
        assert!(CAPABILITIES.contains("don't cite tool, function, or"));
        assert!(CAPABILITIES.contains("retrieve them"));
    }

    #[test]
    fn resp02_behavioral_rules_forbids_infra_identifiers() {
        assert!(BEHAVIORAL_RULES.contains("internal infrastructure identifiers"));
        assert!(BEHAVIORAL_RULES.contains("container numbers"));
    }

    #[test]
    fn behavioral_rules_has_voice_transcription_tolerance() {
        // Voice-first operator: tolerate mobile/voice composition artifacts and
        // proceed on clear intent rather than asking for clarification.
        assert!(BEHAVIORAL_RULES.contains("voice transcription"));
        assert!(BEHAVIORAL_RULES.contains("Interpret intent"));
        assert!(BEHAVIORAL_RULES.contains("rather than asking for clarification"));
    }

    // ── S77 RESP-03: no stale/retired references; generic infra phrasing ──────

    #[test]
    fn resp03_no_retired_references_in_user_facing_constants() {
        // The hallucinated live-test references must not originate from seeded
        // prompt content. None of these should appear in any shared layer text.
        for needle in [
            "<host>", "<host>", "<host>", "<host>", "<host>", "<host>", // pii-test-fixture
            "<host>", "<host>", "ARCADE", "arcade", // pii-test-fixture
            "searxng", "SearXNG", "odyssey_optimize", "signal-cli", "OpenHands",
        ] {
            for (name, body) in [
                ("CORE_IDENTITY", CORE_IDENTITY),
                ("BEHAVIORAL_RULES", BEHAVIORAL_RULES),
                ("CAPABILITIES", CAPABILITIES),
            ] {
                assert!(!body.contains(needle), "{name} must not contain stale ref {needle:?}");
            }
        }
    }

    #[test]
    fn resp03_infra_rule_text_is_generic_not_enumerated() {
        // The infra-reference rule must describe identifiers generically, never
        // enumerate specific container numbers (which would itself be a leak and
        // would go stale). Verify no CT-number pattern slipped into the text.
        // Cheap scan: "CT" immediately followed by a digit, in BOTH the rules
        // and capabilities text (RESP-02's prohibition covers container
        // identifiers wherever they might appear in user-facing layers).
        for (name, body) in [("BEHAVIORAL_RULES", BEHAVIORAL_RULES), ("CAPABILITIES", CAPABILITIES)] {
            let found = body
                .as_bytes()
                .windows(3)
                .any(|w| w[0] == b'C' && w[1] == b'T' && w[2].is_ascii_digit());
            assert!(!found, "{name} must not enumerate specific CT### identifiers");
        }
    }

    // ── Token-cap safety: grown constants must fit their layer budgets ────────

    #[test]
    fn s77_constants_fit_their_layer_token_caps() {
        let rules_cap = layers::config_for(LayerKind::Rules).max_tokens;
        let caps_cap = layers::config_for(LayerKind::Capabilities).max_tokens;
        let rules_tok = layers::estimate_tokens(BEHAVIORAL_RULES);
        let caps_tok = layers::estimate_tokens(CAPABILITIES);
        // Upper bound: must fit the cap so the layer is never truncated mid-clause.
        assert!(rules_tok <= rules_cap, "BEHAVIORAL_RULES {rules_tok} tokens exceeds Rules cap {rules_cap}");
        assert!(caps_tok <= caps_cap, "CAPABILITIES {caps_tok} tokens exceeds Capabilities cap {caps_cap}");
        // Lower bound: catch an accidental gutting of the rule text (a regression
        // that deletes half a clause would still pass the `.contains()` checks if
        // the surviving fragment happened to include the needle).
        assert!(rules_tok >= 200, "BEHAVIORAL_RULES unexpectedly small ({rules_tok} tokens) — clauses lost?");
        assert!(caps_tok >= 200, "CAPABILITIES unexpectedly small ({caps_tok} tokens) — content lost?");
    }
}
