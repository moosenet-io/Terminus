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
    pub fn credential_key(self) -> Option<&'static str> {
        match self {
            Self::Readme | Self::Wiki | Self::Pdf => None,
            Self::Notion => Some("NOTION_TOKEN"),
            Self::Obsidian => Some("OBSIDIAN_VAULT_TOKEN"),
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

/// A project's full doc-target declaration: the ordered list of targets to
/// render. Construct via [`ProjectDocConfig::parse`] (never manually built
/// from untrusted input) or [`ProjectDocConfig::default_readme_only`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProjectDocConfig {
    pub targets: Vec<DocTargetConfig>,
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
        }
    }

    /// Parse a project's raw doc-target config from JSON of the shape:
    /// ```json
    /// { "targets": [ { "type": "readme" }, { "type": "notion", "options": { "database_id": "..." } } ] }
    /// ```
    /// `None` input, a missing/empty `targets` array all fall back to
    /// [`Self::default_readme_only`] -- an unconfigured project is valid,
    /// not malformed. A malformed *present* config (wrong shape, unknown
    /// target type) is a clear [`ToolError::InvalidArgument`], never a
    /// panic.
    pub fn parse(raw: Option<&Value>) -> Result<Self, ToolError> {
        let Some(raw) = raw else {
            return Ok(Self::default_readme_only());
        };

        let targets_val = match raw.as_object().and_then(|o| o.get("targets")) {
            Some(v) => v,
            None => return Ok(Self::default_readme_only()),
        };

        let arr = targets_val.as_array().ok_or_else(|| {
            ToolError::InvalidArgument("doc-target config 'targets' must be an array".into())
        })?;

        if arr.is_empty() {
            return Ok(Self::default_readme_only());
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

        Ok(Self { targets })
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
        assert_eq!(DocTargetType::Obsidian.credential_key(), Some("OBSIDIAN_VAULT_TOKEN"));
        assert_eq!(DocTargetType::Blog.credential_key(), Some("DOCGEN_BLOG_API_TOKEN"));
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
