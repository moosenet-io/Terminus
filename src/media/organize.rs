//! Media domain organize + destructive-op tools (MEDIA-04).
//!
//! Three tools:
//! - [`media_organize`] — non-destructive library organization (tag,
//!   monitor toggle, collection membership). Tiered with the **exact same**
//!   pure [`crate::media::request::classify_request`]/[`MutationTier`]
//!   model MEDIA-03 uses for requests -- a specific/unambiguous/single/
//!   season-scoped change executes immediately, anything ambiguous, bulk,
//!   or whole-series-scoped returns a confirmation payload and requires a
//!   follow-up `confirm: true`. This is the *light* tiering model; it is
//!   never used to gate the destructive tools below.
//! - [`media_delete`] — a single destructive deletion (movie or series),
//!   gated by a **HARD TYPED confirmation** that is strictly stronger than
//!   MEDIA-03's boolean `confirm: true`: the caller must echo back
//!   `confirm_delete` equal, verbatim, to the exact title of the thing being
//!   deleted. A bare `confirm: true`/light ack never triggers it.
//! - [`media_cleanup`] — bulk destructive cleanup (e.g. "clean up watched").
//!   The first call (no `confirm_delete`) only **enumerates** the exact
//!   targets that would be removed; nothing is deleted until a follow-up
//!   call echoes back that exact enumerated set via `confirm_delete`. Items
//!   not watched by *every* Plex user are never silently removed -- they are
//!   flagged and excluded from the eligible set (multi-user Plex EDGE CASE).
//!
//! ## Why typed-confirm is stronger than MEDIA-03's `confirm: true`
//! A boolean `confirm: true` gates *acquisition* (get a thing), where the
//! worst case of a false-positive confirm is an unwanted download. A
//! destructive op's worst case is unrecoverable data loss, so the bar is
//! higher: the caller (Lumina, ultimately relaying a human's words) must
//! reproduce the *exact* name of what will be destroyed. This makes an
//! LLM-side "sure, go ahead" or a stale/replayed `confirm: true` structurally
//! incapable of triggering a delete -- the confirmation payload is the only
//! source of the string that must be echoed back.
//!
//! ## Audit
//! Every *executed* destructive action (single delete, or each item removed
//! by a confirmed bulk cleanup) is recorded via
//! [`crate::gateway_framework::audit::AuditEntry`] (S6-sanitized) naming the
//! exact target. Confirmation-only responses (nothing executed) are not
//! audited as mutations -- no state changed, mirroring MEDIA-03.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use tracing::instrument;

use crate::error::ToolError;
use crate::gateway_framework::audit::{AuditEntry, AuditResult};
use crate::gateway_framework::ActionKind;
use crate::registry::ToolRegistry;
use crate::tool::RustTool;

use super::clients::radarr::RadarrClient;
use super::clients::sonarr::SonarrClient;
use super::request::{classify_request, MutationTier, RequestKind};

// ── shared library-lookup helper (pure over already-fetched JSON) ──────────

/// Find a library item by numeric `id` (Radarr/Sonarr both use an integer
/// `id` field), returning its `title` if present. Pure -- no I/O; the caller
/// fetches the library array first.
fn find_by_id(items: &[Value], id: i64) -> Option<Value> {
    items.iter().find(|item| item.get("id").and_then(|v| v.as_i64()) == Some(id)).cloned()
}

fn title_of(item: &Value) -> String {
    item.get("title").and_then(|v| v.as_str()).unwrap_or_default().to_string()
}

// ── media_organize (non-destructive, MEDIA-03 tiering reused verbatim) ─────

/// Which library-metadata field a `media_organize` call is changing. Kept
/// deliberately small -- MEDIA-04 scope is tag/monitor/collection, not a
/// full Radarr/Sonarr resource editor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OrganizeAction {
    /// Set the `monitored` flag.
    Monitor,
    /// Replace the `tags` array (caller supplies already-resolved tag ids).
    Tag,
    /// Set a movie's TMDb collection id (movies only).
    AddToCollection,
}

impl OrganizeAction {
    fn parse(s: &str) -> Result<Self, ToolError> {
        match s {
            "monitor" => Ok(Self::Monitor),
            "tag" => Ok(Self::Tag),
            "add_to_collection" => Ok(Self::AddToCollection),
            other => Err(ToolError::InvalidArgument(format!(
                "action must be one of \"monitor\", \"tag\", \"add_to_collection\" (got \"{other}\")"
            ))),
        }
    }
}

/// Derive the same [`RequestKind`] shape MEDIA-03 uses, from `media_organize`
/// args -- a specific season is `Season`, no season on a series is `Series`
/// (always high-impact/Confirm, matching "whole series" in `media_request`).
fn organize_kind(media_type: &str, season: Option<i64>) -> RequestKind {
    match (media_type, season) {
        ("movie", _) => RequestKind::Movie,
        ("series", Some(_)) => RequestKind::Season,
        _ => RequestKind::Series,
    }
}

pub struct MediaOrganize {
    radarr: Option<RadarrClient>,
    sonarr: Option<SonarrClient>,
}

impl MediaOrganize {
    fn build_body(existing: &Value, action: OrganizeAction, args: &Value) -> Result<Value, ToolError> {
        let mut body = existing.clone();
        match action {
            OrganizeAction::Monitor => {
                let monitored = args
                    .get("monitored")
                    .and_then(|v| v.as_bool())
                    .ok_or_else(|| ToolError::InvalidArgument("monitored (bool) is required for action=monitor".into()))?;
                body["monitored"] = json!(monitored);
            }
            OrganizeAction::Tag => {
                let tag_ids = args
                    .get("tag_ids")
                    .and_then(|v| v.as_array())
                    .ok_or_else(|| ToolError::InvalidArgument("tag_ids (array of int) is required for action=tag".into()))?
                    .clone();
                body["tags"] = json!(tag_ids);
            }
            OrganizeAction::AddToCollection => {
                let collection_tmdb_id = args.get("collection_tmdb_id").and_then(|v| v.as_i64()).ok_or_else(|| {
                    ToolError::InvalidArgument("collection_tmdb_id (int) is required for action=add_to_collection".into())
                })?;
                body["collection"] = json!({ "tmdbId": collection_tmdb_id });
            }
        }
        Ok(body)
    }
}

#[async_trait]
impl RustTool for MediaOrganize {
    fn name(&self) -> &str {
        "media_organize"
    }

    fn description(&self) -> &str {
        "Non-destructive library organization: tag a movie/series, toggle its monitored state, or set a movie's collection. Uses the same tiered mutation safety as media_request -- a specific, unambiguous, single-item or single-season change executes immediately; anything ambiguous, bulk, or whole-series-scoped returns a confirmation payload and requires confirm: true. Never used for deletion/removal -- see media_delete and media_cleanup for those." // pii-test-fixture
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "integer", "description": "Radarr/Sonarr resource id of the item to organize." },
                "title": { "type": "string", "description": "Title, for narration/audit; not used for lookup (id is authoritative)." },
                "media_type": { "type": "string", "enum": ["movie", "series"] },
                "season": { "type": "integer", "description": "A specific season number, if this change is season-scoped. Omit for a whole-series change (always Confirm-tier)." },
                "action": { "type": "string", "enum": ["monitor", "tag", "add_to_collection"] },
                "monitored": { "type": "boolean", "description": "Required for action=monitor." },
                "tag_ids": { "type": "array", "items": { "type": "integer" }, "description": "Required for action=tag -- already-resolved Radarr/Sonarr tag ids." },
                "collection_tmdb_id": { "type": "integer", "description": "Required for action=add_to_collection (movies only)." },
                "item_count": { "type": "integer", "description": "How many discrete items this call would change at once. Defaults to 1; >1 is always Confirm-tier." },
                "is_ambiguous": { "type": "boolean", "description": "True if the target itself is not definitively resolved. Defaults to false." },
                "confirm": { "type": "boolean", "description": "Must be true to execute a Confirm-tier organize change." }
            },
            "required": ["id", "media_type", "action"]
        })
    }

    #[instrument(skip(self, args), fields(tool = "media_organize"))]
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let id = args
            .get("id")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| ToolError::InvalidArgument("id is required".into()))?;
        let media_type = args.get("media_type").and_then(|v| v.as_str()).unwrap_or("");
        if media_type != "movie" && media_type != "series" {
            return Err(ToolError::InvalidArgument("media_type must be \"movie\" or \"series\"".into()));
        }
        let action_str = args.get("action").and_then(|v| v.as_str()).unwrap_or("");
        let action = OrganizeAction::parse(action_str)?;
        if action == OrganizeAction::AddToCollection && media_type != "movie" {
            return Err(ToolError::InvalidArgument("add_to_collection only applies to media_type=movie".into()));
        }

        let season = args.get("season").and_then(|v| v.as_i64());
        let item_count = args.get("item_count").and_then(|v| v.as_u64()).unwrap_or(1).max(1) as u32;
        let is_ambiguous = args.get("is_ambiguous").and_then(|v| v.as_bool()).unwrap_or(false);
        let confirm = args.get("confirm").and_then(|v| v.as_bool()).unwrap_or(false);

        let kind = organize_kind(media_type, season);
        // Non-destructive metadata changes have no download size, so the
        // oversized-single-item leg of classify_request never fires here --
        // ambiguity/bulk/whole-series are the only levers, which is exactly
        // what "organize" needs.
        let tier = classify_request(kind, is_ambiguous, item_count, 0);

        if tier == MutationTier::Confirm && !confirm {
            return Ok(json!({
                "summary": "This organize change is ambiguous, bulk, or whole-series-scoped -- reply with confirm: true to proceed.",
                "structured": { "id": id, "media_type": media_type, "action": action_str, "tier": "confirm", "executed": false }
            })
            .to_string());
        }

        let (title, updated) = match media_type {
            "movie" => {
                let radarr = self
                    .radarr
                    .as_ref()
                    .ok_or_else(|| ToolError::NotConfigured("RADARR_URL/RADARR_API_KEY not set".into()))?;
                let library = radarr.library().await.unwrap_or(json!([]));
                let items = library.as_array().cloned().unwrap_or_default();
                let existing = find_by_id(&items, id)
                    .ok_or_else(|| ToolError::NotFound(format!("movie id {id} not found in Radarr library")))?;
                let title = title_of(&existing);
                let body = Self::build_body(&existing, action, &args)?;
                let updated = radarr.update_movie(id, body).await?;
                (title, updated)
            }
            _ => {
                let sonarr = self
                    .sonarr
                    .as_ref()
                    .ok_or_else(|| ToolError::NotConfigured("SONARR_URL/SONARR_API_KEY not set".into()))?;
                let library = sonarr.library().await.unwrap_or(json!([]));
                let items = library.as_array().cloned().unwrap_or_default();
                let existing = find_by_id(&items, id)
                    .ok_or_else(|| ToolError::NotFound(format!("series id {id} not found in Sonarr library")))?;
                let title = title_of(&existing);
                let body = Self::build_body(&existing, action, &args)?;
                let updated = sonarr.update_series(id, body).await?;
                (title, updated)
            }
        };

        let detail = format!("media_organize executed: id={id} media_type={media_type} action={action_str} title={title}");
        AuditEntry::new("media", "media_organize", ActionKind::Tool, AuditResult::Success, Some(&detail)).log();

        Ok(json!({
            "summary": format!("Updated \"{title}\" ({action_str})."),
            "structured": { "id": id, "title": title, "media_type": media_type, "action": action_str, "tier": match tier { MutationTier::Light => "light", MutationTier::Confirm => "confirm" }, "executed": true, "updated": updated }
        })
        .to_string())
    }
}

// ── media_delete (single destructive op, hard typed confirm) ───────────────

pub struct MediaDelete {
    radarr: Option<RadarrClient>,
    sonarr: Option<SonarrClient>,
}

/// Whether a typed delete confirmation matches the exact target title.
/// **This is the entire hard-gate**: a bare `confirm: true` is not even
/// accepted as an argument by this tool -- only an exact (trimmed,
/// case-sensitive) string match against the real title unlocks the delete.
/// Pure, unit-tested directly.
fn delete_confirmed(target_title: &str, confirm_delete: Option<&str>) -> bool {
    match confirm_delete {
        Some(c) => c.trim() == target_title.trim() && !target_title.trim().is_empty(),
        None => false,
    }
}

#[async_trait]
impl RustTool for MediaDelete {
    fn name(&self) -> &str {
        "media_delete"
    }

    fn description(&self) -> &str {
        "Permanently delete a single movie or series (Radarr/Sonarr) and its files. DESTRUCTIVE -- hard-gated: the first call (no confirm_delete) returns the exact target title and does NOT delete; a follow-up call must set confirm_delete to that exact title (not a boolean) to actually remove it. A bare confirm: true never triggers a deletion. Deleting something already absent from the library is a no-op, not an error." // pii-test-fixture
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "integer", "description": "Radarr/Sonarr resource id of the item to delete." },
                "media_type": { "type": "string", "enum": ["movie", "series"] },
                "confirm_delete": { "type": "string", "description": "Must exactly equal the target's title (as returned by the first, unconfirmed call) to actually delete. Omit to get the confirmation payload naming the exact target." }
            },
            "required": ["id", "media_type"]
        })
    }

    #[instrument(skip(self, args), fields(tool = "media_delete"))]
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let id = args
            .get("id")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| ToolError::InvalidArgument("id is required".into()))?;
        let media_type = args.get("media_type").and_then(|v| v.as_str()).unwrap_or("");
        if media_type != "movie" && media_type != "series" {
            return Err(ToolError::InvalidArgument("media_type must be \"movie\" or \"series\"".into()));
        }
        let confirm_delete = args.get("confirm_delete").and_then(|v| v.as_str());

        // Locate the item first -- deleting something not present is a
        // clean no-op, never an error, and never requires (or performs) any
        // confirmation dance (EDGE CASE).
        let existing = match media_type {
            "movie" => {
                let radarr = self
                    .radarr
                    .as_ref()
                    .ok_or_else(|| ToolError::NotConfigured("RADARR_URL/RADARR_API_KEY not set".into()))?;
                let library = radarr.library().await.unwrap_or(json!([]));
                find_by_id(&library.as_array().cloned().unwrap_or_default(), id)
            }
            _ => {
                let sonarr = self
                    .sonarr
                    .as_ref()
                    .ok_or_else(|| ToolError::NotConfigured("SONARR_URL/SONARR_API_KEY not set".into()))?;
                let library = sonarr.library().await.unwrap_or(json!([]));
                find_by_id(&library.as_array().cloned().unwrap_or_default(), id)
            }
        };

        let Some(existing) = existing else {
            return Ok(json!({
                "summary": format!("Nothing to delete -- {media_type} id {id} is not in the library."),
                "structured": { "id": id, "media_type": media_type, "executed": false, "already_absent": true }
            })
            .to_string());
        };
        let target_title = title_of(&existing);

        if !delete_confirmed(&target_title, confirm_delete) {
            return Ok(json!({
                "summary": format!(
                    "This will permanently delete \"{target_title}\" and its files. To confirm, call again with confirm_delete: \"{target_title}\" exactly."
                ),
                "structured": { "id": id, "media_type": media_type, "title": target_title, "executed": false, "requires_confirmation": true }
            })
            .to_string());
        }

        let deleted = match media_type {
            "movie" => {
                let radarr = self
                    .radarr
                    .as_ref()
                    .ok_or_else(|| ToolError::NotConfigured("RADARR_URL/RADARR_API_KEY not set".into()))?;
                radarr.delete_movie(id).await
            }
            _ => {
                let sonarr = self
                    .sonarr
                    .as_ref()
                    .ok_or_else(|| ToolError::NotConfigured("SONARR_URL/SONARR_API_KEY not set".into()))?;
                sonarr.delete_series(id).await
            }
        };

        match deleted {
            Ok(true) => {
                let detail = format!("media_delete executed: id={id} media_type={media_type} title={target_title}");
                AuditEntry::new("media", "media_delete", ActionKind::Tool, AuditResult::Success, Some(&detail)).log();
                Ok(json!({
                    "summary": format!("Deleted \"{target_title}\" and its files."),
                    "structured": { "id": id, "media_type": media_type, "title": target_title, "executed": true }
                })
                .to_string())
            }
            Ok(false) => Ok(json!({
                "summary": format!("Nothing to delete -- {media_type} id {id} is not in the library."),
                "structured": { "id": id, "media_type": media_type, "title": target_title, "executed": false, "already_absent": true }
            })
            .to_string()),
            Err(e) => {
                let detail = format!("media_delete failed: id={id} media_type={media_type} title={target_title} error={e}");
                AuditEntry::new("media", "media_delete", ActionKind::Tool, AuditResult::Failure, Some(&detail)).log();
                Err(e)
            }
        }
    }
}

// ── media_cleanup (bulk destructive op: enumerate, then hard-confirm) ──────

/// A single bulk-cleanup candidate. `watched_by_all_users` is supplied by
/// the caller (Lumina, having already cross-referenced Plex per-user watch
/// history -- this domain's `PlexClient` (MEDIA-01) exposes only
/// account-level history, not a per-user breakdown, so per-user aggregation
/// happens upstream of this tool; wire shape not verified against a live
/// multi-user Plex deployment). Defaults to `false` (NOT eligible) when
/// omitted -- the safe default for a destructive op is to flag, not assume
/// consensus (EDGE CASE: multi-user Plex).
#[derive(Debug, Clone, Deserialize)]
struct CleanupCandidate {
    id: i64,
    title: String,
    #[serde(default)]
    watched_by_all_users: bool,
}

/// Partition candidates into (eligible-for-removal, flagged/not-eligible).
/// Pure, unit-tested directly.
fn cleanup_partition(candidates: &[CleanupCandidate]) -> (Vec<&CleanupCandidate>, Vec<&CleanupCandidate>) {
    candidates.iter().partition(|c| c.watched_by_all_users)
}

/// Whether a typed bulk-delete confirmation matches the exact enumerated
/// eligible set -- order-independent, but the *set* must match exactly (no
/// partial confirm, no confirming a superset/subset). Pure, unit-tested.
fn cleanup_confirmed(eligible_titles: &[String], confirm_delete: Option<&[String]>) -> bool {
    let Some(confirm) = confirm_delete else { return false };
    if confirm.is_empty() || eligible_titles.is_empty() {
        return false;
    }
    let mut a: Vec<String> = eligible_titles.iter().map(|s| s.trim().to_string()).collect();
    let mut b: Vec<String> = confirm.iter().map(|s| s.trim().to_string()).collect();
    a.sort();
    b.sort();
    a == b
}

pub struct MediaCleanup {
    radarr: Option<RadarrClient>,
    sonarr: Option<SonarrClient>,
}

#[async_trait]
impl RustTool for MediaCleanup {
    fn name(&self) -> &str {
        "media_cleanup"
    }

    fn description(&self) -> &str {
        "Bulk-remove watched media (e.g. \"clean up what I've watched\") from Radarr/Sonarr. DESTRUCTIVE -- hard-gated: the first call enumerates the EXACT titles that would be removed and deletes nothing; a follow-up call must set confirm_delete to that exact list of titles to actually remove them. Items not watched by every Plex user on a shared server are flagged and never silently removed, even when confirmed." // pii-test-fixture
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "media_type": { "type": "string", "enum": ["movie", "series"] },
                "candidates": {
                    "type": "array",
                    "description": "Items to consider for cleanup, pre-resolved by the caller (typically from Plex watch history cross-referenced with the Radarr/Sonarr library).",
                    "items": {
                        "type": "object",
                        "properties": {
                            "id": { "type": "integer" },
                            "title": { "type": "string" },
                            "watched_by_all_users": { "type": "boolean", "description": "False/omitted -> flagged, never removed even if confirmed (multi-user Plex safety)." }
                        },
                        "required": ["id", "title"]
                    }
                },
                "confirm_delete": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Must exactly equal the enumerated list of eligible titles (as returned by the first, unconfirmed call) to actually delete them."
                }
            },
            "required": ["media_type", "candidates"]
        })
    }

    #[instrument(skip(self, args), fields(tool = "media_cleanup"))]
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let media_type = args.get("media_type").and_then(|v| v.as_str()).unwrap_or("");
        if media_type != "movie" && media_type != "series" {
            return Err(ToolError::InvalidArgument("media_type must be \"movie\" or \"series\"".into()));
        }
        let candidates: Vec<CleanupCandidate> = serde_json::from_value(
            args.get("candidates").cloned().ok_or_else(|| ToolError::InvalidArgument("candidates is required".into()))?,
        )
        .map_err(|e| ToolError::InvalidArgument(format!("invalid candidates: {e}")))?;
        if candidates.is_empty() {
            return Ok(json!({
                "summary": "Nothing to clean up -- no candidates supplied.",
                "structured": { "media_type": media_type, "executed": false, "eligible": [], "flagged": [] }
            })
            .to_string());
        }

        let confirm_delete: Option<Vec<String>> = args
            .get("confirm_delete")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect());

        let (eligible, flagged) = cleanup_partition(&candidates);
        let eligible_titles: Vec<String> = eligible.iter().map(|c| c.title.clone()).collect();
        let flagged_titles: Vec<String> = flagged.iter().map(|c| c.title.clone()).collect();

        if eligible.is_empty() {
            return Ok(json!({
                "summary": "Nothing eligible to clean up -- all candidates are flagged (not watched by every user).",
                "structured": { "media_type": media_type, "executed": false, "eligible": [], "flagged": flagged_titles }
            })
            .to_string());
        }

        if !cleanup_confirmed(&eligible_titles, confirm_delete.as_deref()) {
            // ENUMERATE the exact targets before ever acting -- no blind
            // purge. Nothing has been deleted at this point.
            return Ok(json!({
                "summary": format!(
                    "This will permanently delete {} item(s): {}. {}To confirm, call again with confirm_delete set to exactly this list of titles.",
                    eligible_titles.len(),
                    eligible_titles.join(", "),
                    if flagged_titles.is_empty() { String::new() } else { format!("({} flagged as not watched by every user, will NOT be removed: {}) ", flagged_titles.len(), flagged_titles.join(", ")) }
                ),
                "structured": { "media_type": media_type, "executed": false, "eligible": eligible_titles, "flagged": flagged_titles, "requires_confirmation": true }
            })
            .to_string());
        }

        let mut deleted = Vec::new();
        let mut already_absent = Vec::new();
        let mut failed = Vec::new();

        for c in &eligible {
            let result = match media_type {
                "movie" => match &self.radarr {
                    Some(r) => r.delete_movie(c.id).await,
                    None => Err(ToolError::NotConfigured("RADARR_URL/RADARR_API_KEY not set".into())),
                },
                _ => match &self.sonarr {
                    Some(s) => s.delete_series(c.id).await,
                    None => Err(ToolError::NotConfigured("SONARR_URL/SONARR_API_KEY not set".into())),
                },
            };
            match result {
                Ok(true) => {
                    let detail = format!("media_cleanup executed: id={} media_type={media_type} title={}", c.id, c.title);
                    AuditEntry::new("media", "media_cleanup", ActionKind::Tool, AuditResult::Success, Some(&detail)).log();
                    deleted.push(c.title.clone());
                }
                Ok(false) => already_absent.push(c.title.clone()),
                Err(e) => {
                    let detail =
                        format!("media_cleanup failed: id={} media_type={media_type} title={} error={e}", c.id, c.title);
                    AuditEntry::new("media", "media_cleanup", ActionKind::Tool, AuditResult::Failure, Some(&detail)).log();
                    failed.push(c.title.clone());
                }
            }
        }

        Ok(json!({
            "summary": format!("Cleaned up {} item(s).", deleted.len()),
            "structured": {
                "media_type": media_type,
                "executed": true,
                "deleted": deleted,
                "already_absent": already_absent,
                "failed": failed,
                "flagged": flagged_titles,
            }
        })
        .to_string())
    }
}

// ── registration ─────────────────────────────────────────────────────────────

/// Register the MEDIA-04 organize + destructive-op tools. Degrades
/// independently per service, same as MEDIA-03.
pub fn register(registry: &mut ToolRegistry) {
    registry.register_or_replace(Box::new(MediaOrganize {
        radarr: RadarrClient::from_env().ok(),
        sonarr: SonarrClient::from_env().ok(),
    }));
    registry.register_or_replace(Box::new(MediaDelete {
        radarr: RadarrClient::from_env().ok(),
        sonarr: SonarrClient::from_env().ok(),
    }));
    registry.register_or_replace(Box::new(MediaCleanup {
        radarr: RadarrClient::from_env().ok(),
        sonarr: SonarrClient::from_env().ok(),
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;

    fn organize_tool(radarr: Option<&str>, sonarr: Option<&str>) -> MediaOrganize {
        MediaOrganize {
            radarr: radarr.map(|u| RadarrClient::new(u, "k", reqwest::Client::new())),
            sonarr: sonarr.map(|u| SonarrClient::new(u, "k", reqwest::Client::new())),
        }
    }
    fn delete_tool(radarr: Option<&str>, sonarr: Option<&str>) -> MediaDelete {
        MediaDelete {
            radarr: radarr.map(|u| RadarrClient::new(u, "k", reqwest::Client::new())),
            sonarr: sonarr.map(|u| SonarrClient::new(u, "k", reqwest::Client::new())),
        }
    }
    fn cleanup_tool(radarr: Option<&str>, sonarr: Option<&str>) -> MediaCleanup {
        MediaCleanup {
            radarr: radarr.map(|u| RadarrClient::new(u, "k", reqwest::Client::new())),
            sonarr: sonarr.map(|u| SonarrClient::new(u, "k", reqwest::Client::new())),
        }
    }

    // ── delete_confirmed (pure) ─────────────────────────────────────────────

    #[test]
    fn delete_confirmed_requires_exact_title_match() {
        assert!(delete_confirmed("Dune", Some("Dune")));
        assert!(delete_confirmed("Dune", Some("  Dune  ")));
        assert!(!delete_confirmed("Dune", Some("dune")), "must be case-sensitive");
        assert!(!delete_confirmed("Dune", Some("Dune Part Two")));
        assert!(!delete_confirmed("Dune", None));
        assert!(!delete_confirmed("Dune", Some("")));
    }

    // NEGATIVE: a bare boolean-shaped ack must never satisfy this -- there is
    // no boolean path into this function at all, only exact-string.
    #[test]
    fn delete_confirmed_bare_true_string_does_not_match_a_real_title() {
        assert!(!delete_confirmed("Dune", Some("true")));
    }

    // ── cleanup_partition / cleanup_confirmed (pure) ────────────────────────

    #[test]
    fn cleanup_partition_splits_on_watched_by_all_users() {
        let candidates = vec![
            CleanupCandidate { id: 1, title: "A".into(), watched_by_all_users: true },
            CleanupCandidate { id: 2, title: "B".into(), watched_by_all_users: false },
        ];
        let (eligible, flagged) = cleanup_partition(&candidates);
        assert_eq!(eligible.len(), 1);
        assert_eq!(eligible[0].title, "A");
        assert_eq!(flagged.len(), 1);
        assert_eq!(flagged[0].title, "B");
    }

    #[test]
    fn cleanup_confirmed_requires_exact_set_match() {
        let eligible = vec!["A".to_string(), "B".to_string()];
        assert!(cleanup_confirmed(&eligible, Some(&["B".to_string(), "A".to_string()])), "order-independent");
        assert!(!cleanup_confirmed(&eligible, Some(&["A".to_string()])), "partial confirm must not match");
        assert!(!cleanup_confirmed(&eligible, Some(&["A".to_string(), "B".to_string(), "C".to_string()])), "superset must not match");
        assert!(!cleanup_confirmed(&eligible, None));
        assert!(!cleanup_confirmed(&eligible, Some(&[])));
    }

    #[test]
    fn cleanup_confirmed_empty_eligible_never_confirms() {
        assert!(!cleanup_confirmed(&[], Some(&["A".to_string()])));
    }

    // ── media_organize: tiering + execution (mocked) ────────────────────────

    #[tokio::test]
    async fn organize_light_tier_movie_monitor_executes() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/api/v3/movie");
            then.status(200).json_body(json!([{ "id": 7, "title": "Dune", "monitored": true }]));
        });
        let put_mock = server.mock(|when, then| {
            when.method(PUT).path("/api/v3/movie/7");
            then.status(200).json_body(json!({ "id": 7, "title": "Dune", "monitored": false }));
        });

        let tool = organize_tool(Some(&server.base_url()), None);
        let result = tool
            .execute(json!({ "id": 7, "media_type": "movie", "action": "monitor", "monitored": false }))
            .await
            .unwrap();

        put_mock.assert();
        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["structured"]["executed"], true);
        assert_eq!(parsed["structured"]["tier"], "light");
    }

    #[tokio::test]
    async fn organize_whole_series_is_confirm_tier_and_does_not_execute() {
        let server = MockServer::start();
        let put_mock = server.mock(|when, then| {
            when.method(PUT).path("/api/v3/series/9");
            then.status(200).json_body(json!({}));
        });

        let tool = organize_tool(None, Some(&server.base_url()));
        // No `season` -> whole-series -> RequestKind::Series -> Confirm.
        let result = tool
            .execute(json!({ "id": 9, "media_type": "series", "action": "monitor", "monitored": false }))
            .await
            .unwrap();

        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["structured"]["executed"], false);
        assert_eq!(parsed["structured"]["tier"], "confirm");
        assert_eq!(put_mock.hits(), 0);
    }

    #[tokio::test]
    async fn organize_confirmed_series_executes() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/api/v3/series");
            then.status(200).json_body(json!([{ "id": 9, "title": "Foundation", "monitored": true }]));
        });
        let put_mock = server.mock(|when, then| {
            when.method(PUT).path("/api/v3/series/9");
            then.status(200).json_body(json!({ "id": 9 }));
        });

        let tool = organize_tool(None, Some(&server.base_url()));
        let result = tool
            .execute(json!({ "id": 9, "media_type": "series", "action": "monitor", "monitored": false, "confirm": true }))
            .await
            .unwrap();

        put_mock.assert();
        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["structured"]["executed"], true);
    }

    // ── media_delete: hard typed confirm gate (mocked) ──────────────────────

    #[tokio::test]
    async fn delete_bare_confirm_true_does_not_delete() {
        // media_delete's schema doesn't even accept a "confirm" boolean, but
        // prove the negative explicitly: passing one (as if a caller tried
        // to reuse the media_request/media_organize shape) has zero effect.
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/api/v3/movie");
            then.status(200).json_body(json!([{ "id": 7, "title": "Dune" }]));
        });
        let delete_mock = server.mock(|when, then| {
            when.method(DELETE).path("/api/v3/movie/7");
            then.status(200);
        });

        let tool = delete_tool(Some(&server.base_url()), None);
        let result = tool.execute(json!({ "id": 7, "media_type": "movie", "confirm": true })).await.unwrap();

        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["structured"]["executed"], false);
        assert_eq!(parsed["structured"]["requires_confirmation"], true);
        assert_eq!(delete_mock.hits(), 0, "a bare confirm:true must never trigger a delete");
    }

    #[tokio::test]
    async fn delete_light_ack_does_not_delete() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/api/v3/movie");
            then.status(200).json_body(json!([{ "id": 7, "title": "Dune" }]));
        });
        let delete_mock = server.mock(|when, then| {
            when.method(DELETE).path("/api/v3/movie/7");
            then.status(200);
        });

        let tool = delete_tool(Some(&server.base_url()), None);
        // "yes" / "confirmed" / any non-exact-title string must not work.
        let result = tool
            .execute(json!({ "id": 7, "media_type": "movie", "confirm_delete": "yes" }))
            .await
            .unwrap();

        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["structured"]["executed"], false);
        assert_eq!(delete_mock.hits(), 0);
    }

    #[tokio::test]
    async fn delete_exact_typed_confirm_deletes() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/api/v3/movie");
            then.status(200).json_body(json!([{ "id": 7, "title": "Dune" }]));
        });
        let delete_mock = server.mock(|when, then| {
            when.method(DELETE).path("/api/v3/movie/7");
            then.status(200);
        });

        let tool = delete_tool(Some(&server.base_url()), None);
        let result = tool
            .execute(json!({ "id": 7, "media_type": "movie", "confirm_delete": "Dune" }))
            .await
            .unwrap();

        delete_mock.assert();
        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["structured"]["executed"], true);
    }

    #[tokio::test]
    async fn delete_not_present_is_a_clean_no_op() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/api/v3/movie");
            then.status(200).json_body(json!([]));
        });
        let delete_mock = server.mock(|when, then| {
            when.method(DELETE).path("/api/v3/movie/999");
            then.status(200);
        });

        let tool = delete_tool(Some(&server.base_url()), None);
        let result = tool.execute(json!({ "id": 999, "media_type": "movie" })).await.unwrap();

        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["structured"]["already_absent"], true);
        assert_eq!(parsed["structured"]["executed"], false);
        assert_eq!(delete_mock.hits(), 0);
        assert!(parsed["summary"].as_str().unwrap().to_lowercase().contains("not in the library"));
    }

    #[tokio::test]
    async fn delete_series_exact_typed_confirm_deletes() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/api/v3/series");
            then.status(200).json_body(json!([{ "id": 9, "title": "Foundation" }]));
        });
        let delete_mock = server.mock(|when, then| {
            when.method(DELETE).path("/api/v3/series/9");
            then.status(200);
        });

        let tool = delete_tool(None, Some(&server.base_url()));
        let result = tool
            .execute(json!({ "id": 9, "media_type": "series", "confirm_delete": "Foundation" }))
            .await
            .unwrap();

        delete_mock.assert();
        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["structured"]["executed"], true);
    }

    // ── media_cleanup: enumerate-then-confirm bulk destructive (mocked) ─────

    #[tokio::test]
    async fn cleanup_unconfirmed_enumerates_and_deletes_nothing() {
        let server = MockServer::start();
        let delete_arrival = server.mock(|when, then| {
            when.method(DELETE).path("/api/v3/movie/1");
            then.status(200);
        });
        let delete_dune = server.mock(|when, then| {
            when.method(DELETE).path("/api/v3/movie/2");
            then.status(200);
        });

        let tool = cleanup_tool(Some(&server.base_url()), None);
        let result = tool
            .execute(json!({
                "media_type": "movie",
                "candidates": [
                    { "id": 1, "title": "Arrival", "watched_by_all_users": true },
                    { "id": 2, "title": "Dune", "watched_by_all_users": true },
                ]
            }))
            .await
            .unwrap();

        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["structured"]["executed"], false);
        let eligible: Vec<String> =
            parsed["structured"]["eligible"].as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect();
        assert_eq!(eligible.len(), 2);
        assert!(eligible.contains(&"Arrival".to_string()));
        assert!(eligible.contains(&"Dune".to_string()));
        assert_eq!(delete_arrival.hits(), 0, "unconfirmed bulk cleanup must not delete anything");
        assert_eq!(delete_dune.hits(), 0, "unconfirmed bulk cleanup must not delete anything");
    }

    #[tokio::test]
    async fn cleanup_confirmed_deletes_exactly_the_enumerated_set() {
        let server = MockServer::start();
        let delete_arrival = server.mock(|when, then| {
            when.method(DELETE).path("/api/v3/movie/1");
            then.status(200);
        });
        let delete_dune = server.mock(|when, then| {
            when.method(DELETE).path("/api/v3/movie/2");
            then.status(200);
        });

        let tool = cleanup_tool(Some(&server.base_url()), None);
        let result = tool
            .execute(json!({
                "media_type": "movie",
                "candidates": [
                    { "id": 1, "title": "Arrival", "watched_by_all_users": true },
                    { "id": 2, "title": "Dune", "watched_by_all_users": true },
                ],
                "confirm_delete": ["Dune", "Arrival"]
            }))
            .await
            .unwrap();

        delete_arrival.assert();
        delete_dune.assert();
        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["structured"]["executed"], true);
        assert_eq!(parsed["structured"]["deleted"].as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn cleanup_flags_items_not_watched_by_all_users_and_never_removes_them() {
        let server = MockServer::start();
        let delete_shared = server.mock(|when, then| {
            when.method(DELETE).path("/api/v3/movie/1");
            then.status(200);
        });
        let delete_personal = server.mock(|when, then| {
            when.method(DELETE).path("/api/v3/movie/2");
            then.status(200);
        });

        let tool = cleanup_tool(Some(&server.base_url()), None);
        // First call: enumerate. Only the all-users-watched item should be
        // in "eligible"; the other must be "flagged".
        let result = tool
            .execute(json!({
                "media_type": "movie",
                "candidates": [
                    { "id": 1, "title": "Shared Watch", "watched_by_all_users": true },
                    { "id": 2, "title": "Solo Watch", "watched_by_all_users": false },
                ]
            }))
            .await
            .unwrap();
        let parsed: Value = serde_json::from_str(&result).unwrap();
        let eligible: Vec<String> =
            parsed["structured"]["eligible"].as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect();
        let flagged: Vec<String> =
            parsed["structured"]["flagged"].as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect();
        assert_eq!(eligible, vec!["Shared Watch".to_string()]);
        assert_eq!(flagged, vec!["Solo Watch".to_string()]);

        // Even if the caller tries to confirm-delete BOTH (e.g. confused
        // ack), only the eligible one can ever be removed: the confirm set
        // won't match the eligible set (which excludes the flagged item),
        // so this must fall back to re-enumeration, not a partial delete.
        let result2 = tool
            .execute(json!({
                "media_type": "movie",
                "candidates": [
                    { "id": 1, "title": "Shared Watch", "watched_by_all_users": true },
                    { "id": 2, "title": "Solo Watch", "watched_by_all_users": false },
                ],
                "confirm_delete": ["Shared Watch", "Solo Watch"]
            }))
            .await
            .unwrap();
        let parsed2: Value = serde_json::from_str(&result2).unwrap();
        assert_eq!(parsed2["structured"]["executed"], false, "confirm set must match eligible exactly, not a superset");
        assert_eq!(delete_shared.hits(), 0);
        assert_eq!(delete_personal.hits(), 0);

        // Confirming exactly the eligible set works and never touches the
        // flagged item.
        let result3 = tool
            .execute(json!({
                "media_type": "movie",
                "candidates": [
                    { "id": 1, "title": "Shared Watch", "watched_by_all_users": true },
                    { "id": 2, "title": "Solo Watch", "watched_by_all_users": false },
                ],
                "confirm_delete": ["Shared Watch"]
            }))
            .await
            .unwrap();
        let parsed3: Value = serde_json::from_str(&result3).unwrap();
        assert_eq!(parsed3["structured"]["executed"], true);
        delete_shared.assert();
        assert_eq!(delete_personal.hits(), 0, "flagged item must never be removed, even on a confirmed bulk cleanup");
    }

    #[tokio::test]
    async fn cleanup_defaults_watched_by_all_users_to_false_when_omitted() {
        let tool = cleanup_tool(None, None);
        let result = tool
            .execute(json!({
                "media_type": "movie",
                "candidates": [ { "id": 1, "title": "Unknown Status" } ]
            }))
            .await
            .unwrap();
        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["structured"]["eligible"].as_array().unwrap().len(), 0);
        assert_eq!(parsed["structured"]["flagged"].as_array().unwrap().len(), 1);
    }

    // ── config-missing / invalid-arg guards ─────────────────────────────────

    #[tokio::test]
    async fn organize_missing_radarr_client_is_not_configured() {
        let tool = organize_tool(None, None);
        let result = tool.execute(json!({ "id": 1, "media_type": "movie", "action": "monitor", "monitored": true })).await;
        assert!(matches!(result, Err(ToolError::NotConfigured(_))));
    }

    #[tokio::test]
    async fn delete_missing_sonarr_client_is_not_configured() {
        let tool = delete_tool(None, None);
        let result = tool.execute(json!({ "id": 1, "media_type": "series" })).await;
        assert!(matches!(result, Err(ToolError::NotConfigured(_))));
    }

    #[tokio::test]
    async fn organize_invalid_action_is_invalid_argument() {
        let tool = organize_tool(None, None);
        let result = tool.execute(json!({ "id": 1, "media_type": "movie", "action": "delete" })).await;
        assert!(matches!(result, Err(ToolError::InvalidArgument(_))));
    }

    #[tokio::test]
    async fn cleanup_invalid_media_type_is_invalid_argument() {
        let tool = cleanup_tool(None, None);
        let result = tool.execute(json!({ "media_type": "album", "candidates": [] })).await;
        assert!(matches!(result, Err(ToolError::InvalidArgument(_))));
    }

    #[test]
    fn tool_metadata_is_valid() {
        let o = organize_tool(None, None);
        assert_eq!(o.name(), "media_organize");
        assert!(!o.description().is_empty());
        let d = delete_tool(None, None);
        assert_eq!(d.name(), "media_delete");
        assert!(!d.description().is_empty());
        let c = cleanup_tool(None, None);
        assert_eq!(c.name(), "media_cleanup");
        assert!(!c.description().is_empty());
    }

    // media_organize on an id that isn't in the library → NotFound, not a panic.
    #[tokio::test]
    async fn organize_item_not_found_returns_error() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/api/v3/movie");
            then.status(200).json_body(json!([]));
        });

        let tool = organize_tool(Some(&server.base_url()), None);
        let result = tool
            .execute(json!({ "id": 999, "media_type": "movie", "action": "monitor", "monitored": false }))
            .await;

        assert!(matches!(result, Err(ToolError::NotFound(_))));
    }

    // media_cleanup with no candidates → clean no-op response (nothing eligible,
    // executed:false), never an error and never a delete.
    #[tokio::test]
    async fn cleanup_empty_candidates_returns_clean_response() {
        let server = MockServer::start();
        let any_delete = server.mock(|when, then| {
            when.method(DELETE);
            then.status(200);
        });

        let tool = cleanup_tool(Some(&server.base_url()), None);
        let result = tool
            .execute(json!({ "media_type": "movie", "candidates": [] }))
            .await
            .unwrap();

        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["structured"]["executed"], false);
        assert_eq!(
            parsed["structured"]["eligible"].as_array().map(|a| a.len()).unwrap_or(0),
            0
        );
        assert_eq!(any_delete.hits(), 0, "empty cleanup must never delete anything");
    }
}
