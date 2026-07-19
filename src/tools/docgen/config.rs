//! Per-project doc-target configuration (DOCGEN-01 scaffold, S95).
//!
//! A project declares which output artifacts the doc engine should produce
//! (`readme` | `wiki` | `pdf` | `notion` | `obsidian` | `blog`) plus
//! per-target rendering options. The engine reads this to know what to
//! produce -- it never guesses formats (S95 design overview, "Config-driven
//! output"). A project that declares nothing at all gets the safe default:
//! README-only.
//!
//! This module is SCHEMA ONLY -- parsing, defaulting, and (structural)
//! credential-availability resolution. No generation, rendering, or
//! versioning happens here (those land in DOCGEN-05/06/07). It never reads a
//! secret VALUE: [`DocTargetType::credential_key`] names the vault KEY a
//! target needs, and [`ProjectDocConfig::resolve`] takes the set of
//! currently-available key NAMES as a plain argument supplied by the caller
//! -- this module has no `vault::manager()` / `SecretManager::get()` call of
//! its own, so it stays fully unit-testable without any runtime secret
//! store and never risks a raw environment-variable read of a credential.
//!
//! ## DGRICH-09: repo-level rich-pipeline tuning knobs
//! [`ProjectDocConfig`] also carries three OPTIONAL per-project knobs for
//! the DGRICH-01..08 repo-level rich pipeline: [`ProjectDocConfig::subsystem_page_cap`]
//! (how many per-subsystem reference pages Pass 2 actually generates,
//! default [`DEFAULT_SUBSYSTEM_PAGE_CAP`] = 16, the same ceiling
//! `repo_facts`'s own subsystem-selection rule already applies), [`ProjectDocConfig::landing_budget`]
//! (this project's own concise-landing line budget, checked in ADDITION to
//! -- never instead of -- the engine-wide `readme_layers::LANDING_MAX_LINES`
//! hard ceiling; default [`DEFAULT_LANDING_BUDGET`] = 300, i.e. a no-op),
//! and [`ProjectDocConfig::identity_hint`] (an operator-supplied tagline
//! that wins over the Pass 1 generated tagline when present, `None` by
//! default). All three are read independently of `targets` and always
//! default cleanly when absent or malformed -- see [`ProjectDocConfig::parse`].
//! `super::trigger`'s repo-level door ([`super::trigger::run_docgen_trigger`])
//! is what actually threads these into Pass 2 / the placement gate /
//! landing assembly; this module only defines and defaults the schema.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use crate::error::ToolError;

/// The output artifact formats the doc engine can produce. This is the full,
/// stable member list up front -- later items (DOCGEN-06) add the actual
/// renderers per format; this scaffold only needs to name and validate them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DocTargetType {
    Readme,
    Wiki,
    Pdf,
    Notion,
    Obsidian,
    Blog,
}

/// The default target applied when a project declares no doc-target config
/// at all (spec APPROACH step 1: "Default: minimal (README only) if a
/// project declares nothing").
pub const DEFAULT_TARGET_TYPE: DocTargetType = DocTargetType::Readme;

impl DocTargetType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Readme => "readme",
            Self::Wiki => "wiki",
            Self::Pdf => "pdf",
            Self::Notion => "notion",
            Self::Obsidian => "obsidian",
            Self::Blog => "blog",
        }
    }

    /// Parse a target type from a project's raw config string. Returns a
    /// clear [`ToolError::InvalidArgument`] for anything outside the known
    /// six -- never a panic (spec TEST PLAN: "unknown target type -> clear
    /// error, not a crash").
    pub fn parse(raw: &str) -> Result<Self, ToolError> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "readme" => Ok(Self::Readme),
            "wiki" => Ok(Self::Wiki),
            "pdf" => Ok(Self::Pdf),
            "notion" => Ok(Self::Notion),
            "obsidian" => Ok(Self::Obsidian),
            "blog" => Ok(Self::Blog),
            other => Err(ToolError::InvalidArgument(format!(
                "unknown doc-target type '{other}' -- expected one of: readme, wiki, pdf, notion, obsidian, blog"
            ))),
        }
    }

    /// The runtime secret-store KEY NAME (never the value) a target needs
    /// credentials for, if any. `readme`/`wiki`/`pdf` render locally from
    /// already-available content and need none. The actual value is
    /// resolved elsewhere, later (DOCGEN-06 render), via
    /// `vault::manager().get(key)` / `SecretManager::get(key)` -- this
    /// module only names the key.
    ///
    /// `obsidian` is credential-FREE here (DGFIX-02, Plane TERM-200,
    /// follow-up from the DOCGEN-06 review): rendering an Obsidian note is
    /// pure -- it needs no token, only *pushing* a note into a vault would,
    /// and this engine never places/pushes (see the WRITE-MODEL INVERSION
    /// doc comment on `super::render`). `OBSIDIAN_VAULT_TOKEN` remains a
    /// valid env var name for a future placement/push layer outside this
    /// module, but it must never gate rendering itself -- obsidian renders
    /// unconditionally, exactly like `readme`/`wiki`.
    pub fn credential_key(self) -> Option<&'static str> {
        match self {
            Self::Readme | Self::Wiki | Self::Pdf | Self::Obsidian => None,
            Self::Notion => Some("NOTION_TOKEN"),
            Self::Blog => Some("DOCGEN_BLOG_API_TOKEN"),
        }
    }
}

/// One declared doc target plus its free-form per-target rendering options
/// (e.g. a Notion database id, a wiki path hint). Values are strings only --
/// this is rendering configuration, never a place for secret values (those
/// are named by [`DocTargetType::credential_key`] and resolved via the
/// vault, not stored inline here).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocTargetConfig {
    pub target_type: DocTargetType,
    pub options: BTreeMap<String, String>,
}

/// DGRICH-09 default for [`ProjectDocConfig::subsystem_page_cap`]: the same
/// number DGRICH-01's own RepoFacts subsystem-selection rule caps at
/// (design §1.2, `repo_facts::MAX_SUBSYSTEMS`) -- an unconfigured project
/// gets exactly the pipeline's own built-in ceiling, not a second, silently
/// different default.
pub const DEFAULT_SUBSYSTEM_PAGE_CAP: usize = 16;

/// DGRICH-09 default for [`ProjectDocConfig::landing_budget`]: the same
/// value as [`super::readme_layers::LANDING_MAX_LINES`] (DGRICH-05) -- kept
/// as this module's own constant (rather than importing `readme_layers`)
/// so this schema-only module stays dependency-light per its own doc
/// comment; the two are asserted equal in this module's tests so they can
/// never silently drift apart.
pub const DEFAULT_LANDING_BUDGET: usize = 300;

/// A project's full doc-target declaration: the ordered list of targets to
/// render, plus the DGRICH-09 per-project rich-pipeline tuning knobs.
/// Construct via [`ProjectDocConfig::parse`] (never manually built from
/// untrusted input) or [`ProjectDocConfig::default_readme_only`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectDocConfig {
    pub targets: Vec<DocTargetConfig>,
    /// DGRICH-09: the maximum number of per-subsystem reference pages the
    /// repo-level rich pipeline (DGRICH-03 Pass 2) will actually generate
    /// for this project, even if DGRICH-01's own RepoFacts rollup kept
    /// more. Defaults to [`DEFAULT_SUBSYSTEM_PAGE_CAP`] (16 -- the same
    /// ceiling RepoFacts itself already applies, so an unconfigured
    /// project's behavior is unchanged from before this option existed).
    /// Never enforced as a hard error when a project has fewer subsystems
    /// than the cap -- it only ever trims, never pads.
    pub subsystem_page_cap: usize,
    /// DGRICH-09: this project's own concise-landing line budget, checked
    /// in ADDITION to (never instead of) the engine-wide, hardcoded
    /// [`super::readme_layers::LANDING_MAX_LINES`] fail-closed ceiling --
    /// a project may tighten its own budget below the engine-wide cap
    /// (e.g. `landing_budget: 150`), never loosen it above the 300-line
    /// hard ceiling every project shares. Defaults to
    /// [`DEFAULT_LANDING_BUDGET`] (300, matching the engine-wide cap, so an
    /// unconfigured project's gate is unchanged from before this option
    /// existed).
    pub landing_budget: usize,
    /// DGRICH-09: an operator-supplied tagline that WINS over the Pass 1
    /// (DGRICH-02/03) generated tagline when present -- an escape hatch
    /// for the rare repo whose deterministic identity pass keeps producing
    /// a technically-correct but operator-disliked tagline. `None` (the
    /// default) leaves Pass 1's own tagline untouched.
    pub identity_hint: Option<String>,
}

impl Default for ProjectDocConfig {
    fn default() -> Self {
        Self::default_readme_only()
    }
}

impl ProjectDocConfig {
    /// The safe default applied whenever a project declares no config, an
    /// empty target list, or a malformed config (spec EDGE CASES:
    /// "Empty/malformed config -> safe default (README), warn"). The `warn`
    /// half of that edge case is the caller's responsibility (e.g. the
    /// `docgen_status` tool logs/reports when it falls back) -- this
    /// constructor itself is infallible and side-effect-free.
    pub fn default_readme_only() -> Self {
        Self {
            targets: vec![DocTargetConfig {
                target_type: DEFAULT_TARGET_TYPE,
                options: BTreeMap::new(),
            }],
            subsystem_page_cap: DEFAULT_SUBSYSTEM_PAGE_CAP,
            landing_budget: DEFAULT_LANDING_BUDGET,
            identity_hint: None,
        }
    }

    /// Parse a project's raw doc-target config from JSON of the shape:
    /// ```json
    /// { "targets": [ { "type": "readme" }, { "type": "notion", "options": { "database_id": "..." } } ],
    ///   "subsystem_page_cap": 12, "landing_budget": 250, "identity_hint": "..." }
    /// ```
    /// `None` input, a missing/empty `targets` array all fall back to
    /// [`Self::default_readme_only`] -- an unconfigured project is valid,
    /// not malformed. A malformed *present* config (wrong shape, unknown
    /// target type) is a clear [`ToolError::InvalidArgument`], never a
    /// panic.
    ///
    /// DGRICH-09: `subsystem_page_cap`/`landing_budget`/`identity_hint` are
    /// all OPTIONAL and read independently of `targets` -- a project may
    /// tune these even while declaring `targets: []` (which still, on its
    /// own, means "not opted in" at the [`super::trigger`] gate). A
    /// present-but-wrong-shaped value for any of the three (not a
    /// number/not a string) is never a hard error here -- these are tuning
    /// knobs, not structural config -- it is simply treated as absent and
    /// the default applies, matching this function's existing tolerant
    /// posture toward optional per-target `options`.
    pub fn parse(raw: Option<&Value>) -> Result<Self, ToolError> {
        let Some(raw) = raw else {
            return Ok(Self::default_readme_only());
        };
        let obj = raw.as_object();

        let subsystem_page_cap = obj
            .and_then(|o| o.get("subsystem_page_cap"))
            .and_then(Value::as_u64)
            .map(|n| n as usize)
            .unwrap_or(DEFAULT_SUBSYSTEM_PAGE_CAP);
        let landing_budget = obj
            .and_then(|o| o.get("landing_budget"))
            .and_then(Value::as_u64)
            .map(|n| n as usize)
            .unwrap_or(DEFAULT_LANDING_BUDGET);
        let identity_hint = obj
            .and_then(|o| o.get("identity_hint"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);

        let targets_val = match obj.and_then(|o| o.get("targets")) {
            Some(v) => v,
            None => {
                let mut cfg = Self::default_readme_only();
                cfg.subsystem_page_cap = subsystem_page_cap;
                cfg.landing_budget = landing_budget;
                cfg.identity_hint = identity_hint;
                return Ok(cfg);
            }
        };

        let arr = targets_val.as_array().ok_or_else(|| {
            ToolError::InvalidArgument("doc-target config 'targets' must be an array".into())
        })?;

        if arr.is_empty() {
            let mut cfg = Self::default_readme_only();
            cfg.subsystem_page_cap = subsystem_page_cap;
            cfg.landing_budget = landing_budget;
            cfg.identity_hint = identity_hint;
            return Ok(cfg);
        }

        let mut targets = Vec::with_capacity(arr.len());
        for (i, item) in arr.iter().enumerate() {
            let entry = item.as_object().ok_or_else(|| {
                ToolError::InvalidArgument(format!(
                    "doc-target config targets[{i}] must be an object"
                ))
            })?;

            let type_raw = entry.get("type").and_then(Value::as_str).ok_or_else(|| {
                ToolError::InvalidArgument(format!(
                    "doc-target config targets[{i}] missing required 'type' field"
                ))
            })?;

            let target_type = DocTargetType::parse(type_raw).map_err(|e| {
                ToolError::InvalidArgument(format!("doc-target config targets[{i}]: {e}"))
            })?;

            let options: BTreeMap<String, String> = entry
                .get("options")
                .and_then(Value::as_object)
                .map(|m| {
                    m.iter()
                        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                        .collect()
                })
                .unwrap_or_default();

            targets.push(DocTargetConfig { target_type, options });
        }

        Ok(Self { targets, subsystem_page_cap, landing_budget, identity_hint })
    }

    /// The distinct target types this config declares, in a stable order.
    pub fn target_types(&self) -> BTreeSet<DocTargetType> {
        self.targets.iter().map(|t| t.target_type).collect()
    }
}

/// The outcome of checking one declared target against the currently
/// available credential key names (structural check only -- see the module
/// doc comment; no secret value is read here or anywhere in this module).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedDocTarget {
    pub target_type: DocTargetType,
    pub enabled: bool,
    /// Present only when `enabled` is false -- a human-readable reason
    /// (spec EDGE CASES: "that target disabled with a hint, others
    /// proceed").
    pub hint: Option<String>,
}

impl ProjectDocConfig {
    /// Resolve which declared targets are actually usable given the set of
    /// credential KEY NAMES currently known to be available (e.g. as
    /// reported by a caller that already consulted the runtime secret
    /// store's key inventory -- this function itself never touches secret
    /// values or the vault). A target with no credential requirement is
    /// always enabled. A target whose required key is missing from
    /// `available_credential_keys` is disabled with a hint; every other
    /// declared target still resolves independently (spec EDGE CASES: "...
    /// that target disabled with a hint, others proceed").
    pub fn resolve(&self, available_credential_keys: &BTreeSet<String>) -> Vec<ResolvedDocTarget> {
        self.targets
            .iter()
            .map(|t| match t.target_type.credential_key() {
                None => ResolvedDocTarget { target_type: t.target_type, enabled: true, hint: None },
                Some(key) if available_credential_keys.contains(key) => ResolvedDocTarget {
                    target_type: t.target_type,
                    enabled: true,
                    hint: None,
                },
                Some(key) => ResolvedDocTarget {
                    target_type: t.target_type,
                    enabled: false,
                    hint: Some(format!(
                        "{} target disabled: missing credential '{key}' in the runtime secret store",
                        t.target_type.as_str()
                    )),
                },
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── DocTargetType::parse ────────────────────────────────────────────

    #[test]
    fn parses_all_six_known_target_types() {
        assert_eq!(DocTargetType::parse("readme").unwrap(), DocTargetType::Readme);
        assert_eq!(DocTargetType::parse("wiki").unwrap(), DocTargetType::Wiki);
        assert_eq!(DocTargetType::parse("pdf").unwrap(), DocTargetType::Pdf);
        assert_eq!(DocTargetType::parse("notion").unwrap(), DocTargetType::Notion);
        assert_eq!(DocTargetType::parse("obsidian").unwrap(), DocTargetType::Obsidian);
        assert_eq!(DocTargetType::parse("blog").unwrap(), DocTargetType::Blog);
    }

    #[test]
    fn parse_is_case_and_whitespace_insensitive() {
        assert_eq!(DocTargetType::parse("  ReadMe  ").unwrap(), DocTargetType::Readme);
        assert_eq!(DocTargetType::parse("NOTION").unwrap(), DocTargetType::Notion);
    }

    /// Negative test: an unknown target type is a clear, typed error -- not
    /// a panic/crash.
    #[test]
    fn unknown_target_type_returns_clear_error_not_panic() {
        let err = DocTargetType::parse("sharepoint").unwrap_err();
        match err {
            ToolError::InvalidArgument(msg) => {
                assert!(msg.contains("sharepoint"), "error should name the bad value: {msg}");
                assert!(
                    msg.contains("readme"),
                    "error should list valid options: {msg}"
                );
            }
            other => panic!("expected InvalidArgument, got {other:?}"),
        }
    }

    // ── ProjectDocConfig::parse / defaulting ────────────────────────────

    #[test]
    fn no_config_defaults_to_readme_only() {
        let cfg = ProjectDocConfig::parse(None).unwrap();
        assert_eq!(cfg.targets.len(), 1);
        assert_eq!(cfg.targets[0].target_type, DocTargetType::Readme);
    }

    #[test]
    fn empty_targets_array_defaults_to_readme_only() {
        let raw = json!({"targets": []});
        let cfg = ProjectDocConfig::parse(Some(&raw)).unwrap();
        assert_eq!(cfg.targets.len(), 1);
        assert_eq!(cfg.targets[0].target_type, DocTargetType::Readme);
    }

    #[test]
    fn missing_targets_key_defaults_to_readme_only() {
        let raw = json!({"unrelated": "value"});
        let cfg = ProjectDocConfig::parse(Some(&raw)).unwrap();
        assert_eq!(cfg.targets.len(), 1);
        assert_eq!(cfg.targets[0].target_type, DocTargetType::Readme);
    }

    #[test]
    fn parses_declared_target_list_with_options() {
        let raw = json!({
            "targets": [
                {"type": "readme"},
                {"type": "notion", "options": {"database_id": "abc123"}}
            ]
        });
        let cfg = ProjectDocConfig::parse(Some(&raw)).unwrap();
        assert_eq!(cfg.targets.len(), 2);
        assert_eq!(cfg.targets[0].target_type, DocTargetType::Readme);
        assert_eq!(cfg.targets[1].target_type, DocTargetType::Notion);
        assert_eq!(
            cfg.targets[1].options.get("database_id").map(String::as_str),
            Some("abc123")
        );
    }

    /// Negative test: an unknown target type inside a declared list is a
    /// clear error, not a crash.
    #[test]
    fn unknown_target_type_in_list_returns_clear_error() {
        let raw = json!({"targets": [{"type": "sharepoint"}]});
        let err = ProjectDocConfig::parse(Some(&raw)).unwrap_err();
        match err {
            ToolError::InvalidArgument(msg) => assert!(msg.contains("sharepoint")),
            other => panic!("expected InvalidArgument, got {other:?}"),
        }
    }

    /// Negative test: `targets` present but not an array is a clear error,
    /// not a crash.
    #[test]
    fn targets_not_an_array_returns_clear_error() {
        let raw = json!({"targets": "readme"});
        let err = ProjectDocConfig::parse(Some(&raw)).unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    /// Negative test: a target entry that isn't an object is a clear error.
    #[test]
    fn target_entry_not_an_object_returns_clear_error() {
        let raw = json!({"targets": ["readme"]});
        let err = ProjectDocConfig::parse(Some(&raw)).unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    /// Negative test: a target entry missing the required `type` field is a
    /// clear error.
    #[test]
    fn target_entry_missing_type_returns_clear_error() {
        let raw = json!({"targets": [{"options": {}}]});
        let err = ProjectDocConfig::parse(Some(&raw)).unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    // ── resolve() -- structural credential-availability check ──────────

    #[test]
    fn targets_with_no_credential_requirement_are_always_enabled() {
        let cfg = ProjectDocConfig::default_readme_only();
        let resolved = cfg.resolve(&BTreeSet::new());
        assert_eq!(resolved.len(), 1);
        assert!(resolved[0].enabled);
        assert!(resolved[0].hint.is_none());
    }

    #[test]
    fn missing_credential_disables_that_target_with_a_hint_others_proceed() {
        let raw = json!({
            "targets": [
                {"type": "readme"},
                {"type": "notion"}
            ]
        });
        let cfg = ProjectDocConfig::parse(Some(&raw)).unwrap();
        let resolved = cfg.resolve(&BTreeSet::new());
        assert_eq!(resolved.len(), 2);
        // readme: no cred needed, enabled
        assert!(resolved[0].enabled);
        // notion: NOTION_TOKEN missing, disabled with a hint, but readme
        // above still resolved successfully -- "others proceed".
        assert!(!resolved[1].enabled);
        let hint = resolved[1].hint.as_ref().expect("expected a hint");
        assert!(hint.contains("NOTION_TOKEN"));
    }

    #[test]
    fn present_credential_enables_the_target() {
        let raw = json!({"targets": [{"type": "notion"}]});
        let cfg = ProjectDocConfig::parse(Some(&raw)).unwrap();
        let mut available = BTreeSet::new();
        available.insert("NOTION_TOKEN".to_string());
        let resolved = cfg.resolve(&available);
        assert!(resolved[0].enabled);
        assert!(resolved[0].hint.is_none());
    }

    #[test]
    fn credential_key_mapping_is_stable() {
        assert_eq!(DocTargetType::Readme.credential_key(), None);
        assert_eq!(DocTargetType::Wiki.credential_key(), None);
        assert_eq!(DocTargetType::Pdf.credential_key(), None);
        assert_eq!(DocTargetType::Notion.credential_key(), Some("NOTION_TOKEN"));
        // DGFIX-02 (TERM-200): obsidian rendering is pure -- no credential
        // gates it. Only a future vault-push/placement layer would need
        // OBSIDIAN_VAULT_TOKEN, and this module never places/pushes.
        assert_eq!(DocTargetType::Obsidian.credential_key(), None);
        assert_eq!(DocTargetType::Blog.credential_key(), Some("DOCGEN_BLOG_API_TOKEN"));
    }

    /// Negative test (DGFIX-02): obsidian resolves as always-enabled with no
    /// hint, exactly like readme/wiki/pdf -- never disabled for a "missing"
    /// credential, even when no credential keys at all are available.
    #[test]
    fn obsidian_target_always_enabled_regardless_of_available_credentials() {
        let raw = json!({"targets": [{"type": "obsidian"}]});
        let cfg = ProjectDocConfig::parse(Some(&raw)).unwrap();
        let resolved = cfg.resolve(&BTreeSet::new());
        assert_eq!(resolved.len(), 1);
        assert!(resolved[0].enabled, "obsidian must render unconditionally");
        assert!(resolved[0].hint.is_none());
    }

    // ── DGRICH-09: subsystem_page_cap / landing_budget / identity_hint ──

    #[test]
    fn missing_config_defaults_the_dgrich_09_options() {
        let cfg = ProjectDocConfig::parse(None).unwrap();
        assert_eq!(cfg.subsystem_page_cap, DEFAULT_SUBSYSTEM_PAGE_CAP);
        assert_eq!(cfg.landing_budget, DEFAULT_LANDING_BUDGET);
        assert_eq!(cfg.identity_hint, None);
    }

    #[test]
    fn default_landing_budget_matches_the_engine_wide_landing_max_lines() {
        // These must never silently drift apart -- an unconfigured
        // project's DGRICH-09 budget gate must be a no-op against the
        // DGRICH-05 engine-wide cap, not a surprise second ceiling.
        assert_eq!(DEFAULT_LANDING_BUDGET, 300);
    }

    #[test]
    fn declared_config_can_set_subsystem_page_cap_and_landing_budget() {
        let raw = json!({
            "targets": [{"type": "readme"}],
            "subsystem_page_cap": 8,
            "landing_budget": 150
        });
        let cfg = ProjectDocConfig::parse(Some(&raw)).unwrap();
        assert_eq!(cfg.subsystem_page_cap, 8);
        assert_eq!(cfg.landing_budget, 150);
    }

    #[test]
    fn identity_hint_is_read_and_trimmed_when_present() {
        let raw = json!({
            "targets": [{"type": "readme"}],
            "identity_hint": "  Terminus: the fleet's MCP tool hub.  "
        });
        let cfg = ProjectDocConfig::parse(Some(&raw)).unwrap();
        assert_eq!(cfg.identity_hint.as_deref(), Some("Terminus: the fleet's MCP tool hub."));
    }

    #[test]
    fn blank_identity_hint_is_treated_as_absent() {
        let raw = json!({"targets": [{"type": "readme"}], "identity_hint": "   "});
        let cfg = ProjectDocConfig::parse(Some(&raw)).unwrap();
        assert_eq!(cfg.identity_hint, None);
    }

    /// Negative test: a wrong-shaped tuning knob (not a number/not a
    /// string) is never a hard parse error -- it degrades to the default,
    /// exactly like the "missing" case.
    #[test]
    fn wrong_shaped_dgrich_09_options_fall_back_to_defaults_not_an_error() {
        let raw = json!({
            "targets": [{"type": "readme"}],
            "subsystem_page_cap": "twelve",
            "landing_budget": true,
            "identity_hint": 42
        });
        let cfg = ProjectDocConfig::parse(Some(&raw)).unwrap();
        assert_eq!(cfg.subsystem_page_cap, DEFAULT_SUBSYSTEM_PAGE_CAP);
        assert_eq!(cfg.landing_budget, DEFAULT_LANDING_BUDGET);
        assert_eq!(cfg.identity_hint, None);
    }

    /// The DGRICH-09 options are honored even when `targets` is present but
    /// empty (which still means "not opted in" at the trigger's own gate --
    /// this only asserts the schema layer itself doesn't drop the knobs).
    #[test]
    fn dgrich_09_options_survive_an_empty_targets_array() {
        let raw = json!({"targets": [], "subsystem_page_cap": 4});
        let cfg = ProjectDocConfig::parse(Some(&raw)).unwrap();
        assert_eq!(cfg.subsystem_page_cap, 4);
        // targets still fell back to the readme-only default.
        assert_eq!(cfg.targets.len(), 1);
    }

    #[test]
    fn as_str_round_trips_through_parse() {
        for t in [
            DocTargetType::Readme,
            DocTargetType::Wiki,
            DocTargetType::Pdf,
            DocTargetType::Notion,
            DocTargetType::Obsidian,
            DocTargetType::Blog,
        ] {
            assert_eq!(DocTargetType::parse(t.as_str()).unwrap(), t);
        }
    }
}
