//! RMCP-11 — `rmcp_session_list` and `rmcp_session_revoke`.
//!
//! ## Why these are tools and not an admin HTTP route
//!
//! The sanctioned surface rule: the GUI (RMCP-13) and an operator at a CLI must
//! reach the same implementation. A second admin endpoint alongside these tools
//! would be a second authorization decision, a second audit path, and a second
//! chance for "revoked in the UI" to mean something different from "revoked".
//! So the tool is the surface, [`RevocationService`] is the implementation, and
//! the GUI is a caller of the tool like anything else.
//!
//! ## Why revocation is NOT approval-gated
//!
//! Terminus gates a handful of tools behind human approval
//! ([`crate::approval`]), all of them things that ADD reach or destroy data
//! irreversibly. Revocation is the opposite on both counts: it can only ever
//! narrow access, and it is undone by re-authorizing. Gating it would put a
//! confirmation step in front of the one control an operator reaches for when
//! something is actively going wrong — the exact moment latency is least
//! affordable. The asymmetry is deliberate and is the same one
//! [`crate::approval`]'s own list encodes by leaving the read-only `pg_*` tools
//! ungated.
//!
//! ## No token ever crosses this boundary
//!
//! Neither tool accepts or returns token material. Selection is by account
//! name, `client_id`, or family id; a listing carries family ids and timestamps.
//! Revoking BY a raw token is the RFC 7009 endpoint's job
//! ([`RevocationService::revoke_presented_token`]) and stays there, because a
//! token in a tool argument would travel through dispatch, argument summaries,
//! and every trace layer in between.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::OnceCell;
use uuid::Uuid;

use crate::error::ToolError;
use crate::oauth::audit::SelectorKind;
use crate::oauth::revoke::{RevocationService, SessionSelector, SessionStore};
use crate::oauth::store::OauthStore;
use crate::oauth::OauthConfig;
use crate::registry::ToolRegistry;
use crate::tool::{RustTool, ToolOutput};

/// How a tool obtains a [`RevocationService`].
///
/// A seam rather than a direct `OauthStore` so the tools' own argument handling
/// — which selector a set of arguments means, what an ambiguous or empty
/// selector does — is testable without a database. That matters more here than
/// usual: the failure this indirection lets us test is "an under-specified
/// selector silently means EVERYTHING", which is unrecoverable in production and
/// invisible in a code read.
#[async_trait]
trait ServiceSource: Send + Sync {
    async fn service(&self) -> Result<RevocationService, ToolError>;
}

/// The production source: connect lazily, once, from the runtime environment.
///
/// Lazy because the OAuth door is optional — a Terminus deployment with no
/// connector configured must not fail to start, and must not open a Postgres
/// pool it will never use, merely because these two tools are registered.
/// Cached because the alternative is a fresh pool per tool call.
struct EnvServiceSource {
    cell: OnceCell<RevocationService>,
}

impl EnvServiceSource {
    fn new() -> Self {
        Self { cell: OnceCell::new() }
    }
}

#[async_trait]
impl ServiceSource for EnvServiceSource {
    async fn service(&self) -> Result<RevocationService, ToolError> {
        self.cell
            .get_or_try_init(|| async {
                // `OauthConfig::from_env` reads the connection URL; the runtime
                // secret store is materialized into the process environment at
                // startup, so that read IS the vault read (see `crate::oauth`'s
                // module docs and `crate::pg::conn`'s precedent). The URL is
                // never logged or echoed, here or there.
                let config = OauthConfig::from_env()?;
                let store = OauthStore::connect(&config).await?;
                if !store.schema_ready().await {
                    return Err(ToolError::NotConfigured(
                        "the RMCP OAuth schema is not present — apply the S132 migration before \
                         managing connector sessions"
                            .into(),
                    ));
                }
                let store: Arc<dyn SessionStore> = Arc::new(store);
                Ok(RevocationService::new(store))
            })
            .await
            .cloned()
    }
}

/// Read `account` / `client` / `family_id` out of tool arguments into a
/// selector.
///
/// Returns `Ok(None)` when NO selector field was supplied, which the listing
/// tool reads as "everything" and the revoking tool refuses. Splitting those two
/// readings apart at the call site — rather than defaulting here — is
/// deliberate: "no filter" is a reasonable default for a read and a catastrophic
/// one for a write.
fn selector_from(args: &Value) -> Result<Option<SessionSelector>, ToolError> {
    let field = |name: &str| -> Option<String> {
        args.get(name)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
    };
    let account = field("account");
    let client = field("client");
    let family = field("family_id");

    if let Some(raw) = family {
        if account.is_some() || client.is_some() {
            // Refused rather than silently preferring one. A caller that names
            // both means something this API cannot express, and guessing which
            // half to honour is how an operator revokes more (or less) than they
            // intended.
            return Err(ToolError::InvalidArgument(
                "give either family_id or account/client, not both".into(),
            ));
        }
        let id = Uuid::parse_str(&raw).map_err(|_| {
            ToolError::InvalidArgument("family_id must be a UUID, as reported by rmcp_session_list".into())
        })?;
        return Ok(Some(SessionSelector::Family(id)));
    }

    Ok(match (account, client) {
        (Some(account), Some(client)) => Some(SessionSelector::AccountAndClient { account, client }),
        (Some(account), None) => Some(SessionSelector::Account(account)),
        (None, Some(client)) => Some(SessionSelector::Client(client)),
        (None, None) => None,
    })
}

/// `rmcp_session_list`.
struct RmcpSessionList {
    source: Arc<dyn ServiceSource>,
}

#[async_trait]
impl RustTool for RmcpSessionList {
    fn name(&self) -> &str {
        "rmcp_session_list"
    }

    fn description(&self) -> &str {
        "List remote-MCP connector sessions (OAuth refresh-token families), optionally filtered by \
         account, client_id, or family_id. Reports the family id, binding, scope, timestamps and \
         whether the session is still live. Never returns token material."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "account": { "type": "string", "description": "Account name to filter by." },
                "client": { "type": "string", "description": "Public client_id to filter by." },
                "family_id": { "type": "string", "description": "A single session's family id (UUID). Mutually exclusive with account/client." },
                "include_revoked": { "type": "boolean", "description": "Include sessions that are already dead. Defaults to false." }
            },
            "additionalProperties": false
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        Ok(self.execute_structured(args).await?.text)
    }

    async fn execute_structured(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let selector = selector_from(&args)?;
        let include_revoked = args
            .get("include_revoked")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let service = self.source.service().await?;
        let sessions = service.list(selector.as_ref(), include_revoked).await?;

        let text = if sessions.is_empty() {
            // Says "no sessions", never "no such account" — the listing must not
            // become an existence oracle for account names, and the operational
            // answer is the same either way.
            "no matching connector sessions".to_string()
        } else {
            let mut lines = Vec::with_capacity(sessions.len());
            for s in &sessions {
                lines.push(format!(
                    "{} account={} client={} scope={} last_used={} {}",
                    s.family_id,
                    s.account_id,
                    s.client_id,
                    s.scope,
                    s.last_used_at,
                    if s.live { "live" } else { "revoked" }
                ));
            }
            lines.join("\n")
        };
        Ok(ToolOutput::with_structured(text, json!({ "sessions": sessions })))
    }
}

/// `rmcp_session_revoke`.
struct RmcpSessionRevoke {
    source: Arc<dyn ServiceSource>,
}

#[async_trait]
impl RustTool for RmcpSessionRevoke {
    fn name(&self) -> &str {
        "rmcp_session_revoke"
    }

    fn description(&self) -> &str {
        "Revoke remote-MCP connector access by account, client_id, or session family_id. \
         Revoking an account+client pair also revokes the consent behind it. Verified against \
         the store before it reports success; revoking something already revoked succeeds and \
         reports that it changed nothing. \
         TIMING: revoking an ACCOUNT, a CLIENT, or an account+client pair cuts the caller off at \
         its next request. Revoking a single FAMILY does not, if another session for the same \
         account and client is still live — that caller's existing access token keeps working \
         until it expires, because a token carries no session claim for the server to match \
         (TERM #635). Its refresh token is dead either way, so the session cannot be extended. \
         See the Terminus README, 'Exactly what is wired today', for the single account of this."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "account": { "type": "string", "description": "Revoke every session this account holds. Combined with 'client', revokes that pair's consent too." },
                "client": { "type": "string", "description": "Revoke every session issued to this public client_id." },
                "family_id": { "type": "string", "description": "Revoke one session, by the family id reported by rmcp_session_list. Mutually exclusive with account/client." }
            },
            "additionalProperties": false
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        Ok(self.execute_structured(args).await?.text)
    }

    async fn execute_structured(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let selector = selector_from(&args)?.ok_or_else(|| {
            // The refusal that matters. An empty selector on the LISTING tool
            // means "show me everything"; the same shape here would mean "revoke
            // every connector session in the fleet" for anyone who called the
            // tool with no arguments. There is deliberately no
            // `revoke_everything: true` escape hatch either — an operator who
            // genuinely wants that can name each account, and having to do so is
            // the point.
            ToolError::InvalidArgument(
                "name what to revoke: account, client, or family_id. Refusing to revoke every \
                 session in the fleet by default"
                    .into(),
            )
        })?;

        let service = self.source.service().await?;
        let report = service.revoke(&selector).await?;

        let text = if report.families_matched == 0 {
            "nothing matched; no sessions were revoked".to_string()
        } else if report.families_newly_revoked == 0 {
            format!(
                "already revoked: {} session(s) matched and all were already dead",
                report.families_matched
            )
        } else {
            let mut text = format!(
                "revoked {} of {} session(s) ({} token(s), {} consent(s)); verified against the store",
                report.families_newly_revoked,
                report.families_matched,
                report.tokens_revoked,
                report.consents_revoked
            );
            // The moment an operator forms their mental model is the moment they
            // read this line, which is why the caveat is HERE and not only in
            // the tool description they may never have read.
            //
            // Only a single-family revoke can leave a caller connected: the
            // dispatch check asks whether ANY session is live for the
            // (account, client) pair, so revoking an account, a client, or a
            // pair genuinely cuts off at the next request, while revoking one
            // family among several does not.
            if report.selector == SelectorKind::Family {
                text.push_str(
                    ". NOTE: if another session for the same account and client is still live, \
                     that caller's existing access token keeps working until it expires — a \
                     token carries no session claim to match (TERM #635). Its refresh token is \
                     dead, so the session cannot be extended. To cut the caller off now, revoke \
                     the account+client pair.",
                );
            }
            text
        };
        Ok(ToolOutput::with_structured(text, serde_json::to_value(&report).unwrap_or(Value::Null)))
    }
}

pub fn register(registry: &mut ToolRegistry) {
    let source: Arc<dyn ServiceSource> = Arc::new(EnvServiceSource::new());
    registry.register_or_replace(Box::new(RmcpSessionList { source: source.clone() }));
    registry.register_or_replace(Box::new(RmcpSessionRevoke { source }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oauth::revoke::fake::FakeSessionStore;

    fn ids() -> (Uuid, Uuid, Uuid) {
        (
            Uuid::parse_str("11111111-1111-4111-8111-111111111111").unwrap(),
            Uuid::parse_str("22222222-2222-4222-8222-222222222222").unwrap(),
            Uuid::parse_str("33333333-3333-4333-8333-333333333333").unwrap(),
        )
    }

    struct FixedSource {
        store: Arc<FakeSessionStore>,
    }

    #[async_trait]
    impl ServiceSource for FixedSource {
        async fn service(&self) -> Result<RevocationService, ToolError> {
            Ok(RevocationService::new(self.store.clone()))
        }
    }

    /// A source that always fails, standing in for "the OAuth door is not
    /// configured on this deployment".
    struct UnconfiguredSource;

    #[async_trait]
    impl ServiceSource for UnconfiguredSource {
        async fn service(&self) -> Result<RevocationService, ToolError> {
            Err(ToolError::NotConfigured("no RMCP database configured".into()))
        }
    }

    fn tools() -> (RmcpSessionList, RmcpSessionRevoke, Arc<FakeSessionStore>, (Uuid, Uuid, Uuid)) {
        let (account, client, family) = ids();
        let store = Arc::new(
            FakeSessionStore::new()
                .with_account("operator", account)
                .with_client("a-connector", client)
                .with_session(family, account, client),
        );
        let source: Arc<dyn ServiceSource> = Arc::new(FixedSource { store: store.clone() });
        (
            RmcpSessionList { source: source.clone() },
            RmcpSessionRevoke { source },
            store,
            (account, client, family),
        )
    }

    /// The refusal this tool most needs: no arguments must NOT mean "revoke
    /// everything". A listing with no arguments legitimately means everything,
    /// which is exactly why the two tools cannot share one default.
    #[tokio::test]
    async fn revoking_with_no_selector_is_refused_and_changes_nothing() {
        let (_list, revoke, store, (_a, _c, family)) = tools();
        let err = revoke.execute(json!({})).await.expect_err("must refuse");
        assert!(matches!(err, ToolError::InvalidArgument(_)), "{err:?}");
        assert!(store.is_live(family), "an empty selector revoked a live session");

        // …while the listing tool answers the same empty arguments with the
        // whole list.
        let listed = _list.execute_structured(json!({})).await.expect("listing");
        assert!(listed.text.contains(&family.to_string()), "{}", listed.text);
    }

    /// An ambiguous selector is refused rather than half-honoured — guessing
    /// which half an operator meant is how the wrong thing gets revoked.
    #[tokio::test]
    async fn an_ambiguous_selector_is_refused() {
        let (_list, revoke, store, (_a, _c, family)) = tools();
        let err = revoke
            .execute(json!({ "family_id": family.to_string(), "account": "operator" }))
            .await
            .expect_err("must refuse");
        assert!(matches!(err, ToolError::InvalidArgument(_)), "{err:?}");
        assert!(store.is_live(family));
    }

    /// A malformed family id is an argument error, not a silent no-op that
    /// reports success.
    #[tokio::test]
    async fn a_malformed_family_id_is_refused() {
        let (_list, revoke, _store, _) = tools();
        let err = revoke
            .execute(json!({ "family_id": "not-a-uuid" }))
            .await
            .expect_err("must refuse");
        assert!(matches!(err, ToolError::InvalidArgument(_)), "{err:?}");
    }

    /// The ordinary path, end to end through the tool: revoke by account and
    /// client, and the session is dead.
    #[tokio::test]
    async fn revoking_by_account_and_client_kills_the_session() {
        let (_list, revoke, store, (_a, _c, family)) = tools();
        let out = revoke
            .execute(json!({ "account": "operator", "client": "a-connector" }))
            .await
            .expect("revocation succeeds");
        assert!(out.contains("revoked 1 of 1"), "{out}");
        assert!(!store.is_live(family));
    }

    /// Idempotence surfaces as a distinct, honest message rather than as a
    /// second "revoked 1" that would suggest something was still live.
    #[tokio::test]
    async fn a_second_revocation_reports_that_it_changed_nothing() {
        let (_list, revoke, _store, (_a, _c, family)) = tools();
        let args = json!({ "family_id": family.to_string() });
        revoke.execute(args.clone()).await.expect("first");
        let out = revoke.execute(args).await.expect("second must succeed");
        assert!(out.contains("already revoked"), "{out}");
    }

    /// The disclosure an operator actually reads: revoking ONE family warns
    /// that the caller may not be cut off; revoking a PAIR does not, because
    /// that one genuinely is.
    ///
    /// This is the fourth place this item has had to correct the same
    /// incomplete account of revocation timing (README, `oauth::mod`, the tool
    /// description, and this runtime message). It is the one that matters most:
    /// a description may never be read, but this line is what appears the
    /// instant the operator acts.
    #[tokio::test]
    async fn a_single_family_revoke_warns_that_the_caller_may_not_be_cut_off() {
        let (_list, revoke, _store, (_a, _c, family)) = tools();
        let out = revoke
            .execute(json!({ "family_id": family.to_string() }))
            .await
            .expect("revocation succeeds");
        assert!(out.contains("revoked 1 of 1"), "{out}");
        assert!(out.contains("TERM #635"), "the per-session caveat is missing: {out}");
        assert!(
            out.contains("keeps working until it expires"),
            "the caveat must say what actually happens, not just cite an issue: {out}"
        );

        // Revoking the PAIR is a real cut-off, so the caveat must NOT appear —
        // a warning attached to every outcome is a warning nobody reads.
        let (_list, revoke, _store, _) = tools();
        let out = revoke
            .execute(json!({ "account": "operator", "client": "a-connector" }))
            .await
            .expect("revocation succeeds");
        assert!(out.contains("revoked 1 of 1"), "{out}");
        assert!(
            !out.contains("TERM #635"),
            "an account+client revoke IS an immediate cut-off and must not be hedged: {out}"
        );
    }

    /// The tool DESCRIPTION carries the same distinction, since that is where an
    /// operator forms the model before they ever run it.
    #[test]
    fn the_revoke_description_discloses_the_per_session_limit() {
        let (_list, revoke, _store, _) = tools();
        let d = revoke.description();
        assert!(d.contains("TERM #635"), "{d}");
        assert!(d.contains("Exactly what is wired today"), "it must point at the one account: {d}");
        // And it must not repeat the bare claim that started this.
        assert!(
            !d.contains("Takes effect at the next request, not at the next token expiry"),
            "the unqualified timing claim is back: {d}"
        );
    }

    /// A selector that resolves to nothing is reported as such, and — the part
    /// that matters — revokes nothing rather than degrading to "no filters".
    #[tokio::test]
    async fn an_unknown_account_revokes_nothing() {
        let (_list, revoke, store, (_a, _c, family)) = tools();
        let out = revoke
            .execute(json!({ "account": "no-such-account" }))
            .await
            .expect("not an error");
        assert!(out.contains("nothing matched"), "{out}");
        assert!(store.is_live(family));
    }

    /// A listing must carry no token material — it goes into a GUI and gets
    /// pasted into chats. Asserted on the structured payload, which is what a
    /// caller actually consumes.
    #[tokio::test]
    async fn a_listing_reports_family_ids_and_no_credentials() {
        let (list, _revoke, _store, (_a, _c, family)) = tools();
        let out = list.execute_structured(json!({})).await.expect("listing");
        let json = serde_json::to_string(&out.structured).expect("serializable");
        assert!(json.contains(&family.to_string()));
        for suspicious in ["token_hash", "code_hash", "password", "secret", "refresh_token"] {
            assert!(!json.contains(suspicious), "{suspicious} appeared in a listing: {json}");
        }
    }

    /// On a deployment with no OAuth door, these tools must fail with a clear
    /// "not configured" rather than a confusing database error — and must not
    /// have opened a pool merely by being registered.
    #[tokio::test]
    async fn an_unconfigured_deployment_reports_not_configured() {
        let source: Arc<dyn ServiceSource> = Arc::new(UnconfiguredSource);
        let list = RmcpSessionList { source: source.clone() };
        let revoke = RmcpSessionRevoke { source };
        assert!(matches!(
            list.execute(json!({})).await.expect_err("must fail"),
            ToolError::NotConfigured(_)
        ));
        assert!(matches!(
            revoke.execute(json!({ "account": "operator" })).await.expect_err("must fail"),
            ToolError::NotConfigured(_)
        ));
    }

    /// Argument parsing, exhaustively — this is the function that decides what
    /// gets revoked, so every shape it accepts is pinned.
    #[test]
    fn selector_parsing_covers_every_shape() {
        assert_eq!(selector_from(&json!({})).unwrap(), None);
        // Blank strings are absence, not an empty-named account.
        assert_eq!(selector_from(&json!({ "account": "   " })).unwrap(), None);
        assert_eq!(
            selector_from(&json!({ "account": "operator" })).unwrap(),
            Some(SessionSelector::Account("operator".into()))
        );
        assert_eq!(
            selector_from(&json!({ "client": "a-connector" })).unwrap(),
            Some(SessionSelector::Client("a-connector".into()))
        );
        assert_eq!(
            selector_from(&json!({ "account": " operator ", "client": "a-connector" })).unwrap(),
            Some(SessionSelector::AccountAndClient {
                account: "operator".into(),
                client: "a-connector".into()
            })
        );
        assert!(selector_from(&json!({ "family_id": "nope" })).is_err());
        assert!(selector_from(&json!({ "family_id": Uuid::nil().to_string(), "client": "c" })).is_err());
    }

    /// Tool names are dispatch keys and must be stable; the schemas must be the
    /// object shape the registry expects.
    #[test]
    fn tool_identities_and_schemas_are_well_formed() {
        let (list, revoke, _store, _) = tools();
        assert_eq!(list.name(), "rmcp_session_list");
        assert_eq!(revoke.name(), "rmcp_session_revoke");
        for tool in [&list as &dyn RustTool, &revoke as &dyn RustTool] {
            let schema = tool.parameters();
            assert_eq!(schema["type"], "object");
            assert!(schema["properties"].is_object());
            assert!(!tool.description().is_empty());
        }
    }
}
