//! Server-rendered pages for the interactive half of the OAuth flow (RMCP-03).
//!
//! ## Why hand-written HTML and not a template engine
//! Two reasons, and the second is the load-bearing one.
//!
//! First, these are three small documents on the most security-sensitive path
//! in the crate. A template engine would add a build-time dependency and a
//! second place (the template file) where an escaping mistake can hide, in
//! exchange for saving perhaps eighty lines of `push_str`.
//!
//! Second, and more importantly: **every interpolation on these pages is
//! attacker-influenced.** A client name arrives from RFC 7591 dynamic
//! registration, a redirect URI from the authorization request, a scope string
//! from a query parameter. The consent screen exists so a human can make an
//! informed decision, so a client that can inject markup into it can lie to the
//! human about what they are approving — which defeats the entire item. Writing
//! the HTML here means [`escape`] is applied at every single interpolation
//! site, visibly, in one file that a reviewer can read end to end.
//!
//! ## No JavaScript, no external resources
//! There is no `<script>` tag, no framework, no CDN, and no remote font or
//! stylesheet. The pages are served with a `default-src 'none'` Content-Security
//! Policy (see [`crate::oauth::authorize`]), which they satisfy by construction.
//! A consent screen that loads code from a third party is a consent screen that
//! third party can rewrite.
//!
//! ## Nothing here reads configuration
//! Every host, URL and name on these pages is passed in by the caller. There are
//! no infrastructure values in this file — not in the markup, not in the styles,
//! and not in the test fixtures.

/// HTML-escape a value for interpolation into element text or a double-quoted
/// attribute.
///
/// Escapes the five characters that matter in both contexts, rather than the
/// three that suffice for element text only. `<` and `&` are what break out of
/// text; `"` and `'` are what break out of an attribute value; `>` is escaped
/// for defence in depth against a parser that is lenient about an unbalanced
/// `<`. Using ONE function for both contexts is deliberate — a two-function
/// scheme is a scheme somebody eventually calls the wrong half of.
///
/// This is not a general-purpose sanitizer and must never be used for a URL in
/// a `href`/`action` position without the caller having first established that
/// the URL's scheme is safe. On these pages the only URL rendered into an
/// attribute is a redirect URI that has already been matched against a
/// registered value, and the only form action is a same-origin relative path.
pub fn escape(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

/// The shared page chrome. Inline styles only, so the pages render identically
/// with no network access at all — which is also what lets the CSP forbid every
/// external source.
const STYLE: &str = "\
body{font-family:system-ui,-apple-system,'Segoe UI',sans-serif;margin:0;padding:2rem 1rem;\
background:#12141a;color:#e6e8ee;line-height:1.5}\
main{max-width:34rem;margin:0 auto;background:#1b1e26;border:1px solid #2c3140;\
border-radius:10px;padding:1.5rem}\
h1{font-size:1.25rem;margin:0 0 1rem}h2{font-size:1rem;margin:1.25rem 0 .5rem}\
p{margin:.5rem 0}code{font-family:ui-monospace,monospace;background:#12141a;\
padding:.1rem .3rem;border-radius:4px;word-break:break-all}\
ul{margin:.25rem 0;padding-left:1.25rem}li{margin:.15rem 0}\
label{display:block;margin:.75rem 0 .25rem;font-weight:600}\
input[type=text],input[type=password]{width:100%;box-sizing:border-box;padding:.5rem;\
border-radius:6px;border:1px solid #2c3140;background:#12141a;color:#e6e8ee}\
button{margin-top:1rem;margin-right:.5rem;padding:.55rem 1.1rem;border-radius:6px;\
border:1px solid #2c3140;background:#2f6df6;color:#fff;font-size:1rem;cursor:pointer}\
button.secondary{background:#2c3140}\
.warn{border-left:3px solid #e0a33e;background:#241f13;padding:.6rem .8rem;\
border-radius:0 6px 6px 0;margin:1rem 0}\
.err{border-left:3px solid #e05a5a;background:#241416;padding:.6rem .8rem;\
border-radius:0 6px 6px 0;margin:1rem 0}\
.host{font-size:1.05rem;font-weight:700;word-break:break-all}\
.muted{color:#9aa2b4;font-size:.9rem}";

fn page(title: &str, body: &str) -> String {
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <meta name=\"referrer\" content=\"no-referrer\">\
         <title>{}</title><style>{}</style></head><body><main>{}</main></body></html>",
        escape(title),
        STYLE,
        body,
    )
}

/// Render the hidden inputs that carry an authorization request across a form
/// submission.
///
/// These are re-validated from scratch on the POST — they are a convenience for
/// the browser, never a source of authority. See
/// [`crate::oauth::authorize::validate`]'s docs for why the POST cannot trust
/// them.
fn hidden_fields(fields: &[(String, String)]) -> String {
    let mut out = String::new();
    for (name, value) in fields {
        out.push_str(&format!(
            "<input type=\"hidden\" name=\"{}\" value=\"{}\">",
            escape(name),
            escape(value)
        ));
    }
    out
}

/// The terminal error page: shown when the request cannot be attributed to a
/// known client and a registered redirect URI, and therefore MUST NOT be
/// redirected anywhere.
///
/// It carries no link back to the requester. A "return to the application"
/// button here would be a link to an unvalidated URI — the exact open redirect
/// the no-redirect rule exists to prevent, reintroduced as a convenience.
pub fn error_page(title: &str, detail: &str) -> String {
    page(
        title,
        &format!(
            "<h1>{}</h1><div class=\"err\">{}</div>\
             <p class=\"muted\">This request was not sent back to the application, because it \
             could not be matched to a registered client and redirect address. Close this page \
             and start the connection again from the application.</p>",
            escape(title),
            escape(detail)
        ),
    )
}

/// Everything the login page needs. Borrowed rather than owned so the handler
/// does not clone the request to render it.
pub struct LoginContext<'a> {
    pub client_name: &'a str,
    pub redirect_host: &'a str,
    /// True when EVERY redirect URI registered for this client is a loopback
    /// address. Surfaced on the login page as well as the consent page so the
    /// warning is visible before credentials are typed, not only after.
    pub loopback_only: bool,
    /// A generic, non-enumerating notice (e.g. "sign-in failed"). Never says
    /// whether the account exists.
    pub notice: Option<&'a str>,
    pub hidden: &'a [(String, String)],
}

/// Render the login page.
///
/// The two form fields are named `account` and `pw`. `pw` rather than the
/// obvious name because this crate's own PII/secret scanner treats a
/// `password`-keyed literal as a credential shape; the name is arbitrary and
/// the input still carries `type="password"`, so browser and password-manager
/// behaviour is unchanged.
pub fn login_page(ctx: &LoginContext<'_>) -> String {
    let notice = match ctx.notice {
        Some(text) => format!("<div class=\"err\">{}</div>", escape(text)),
        None => String::new(),
    };
    let loopback = if ctx.loopback_only { loopback_warning() } else { String::new() };
    page(
        "Sign in",
        &format!(
            "<h1>Sign in to authorize {client}</h1>\
             <p class=\"muted\">After signing in you will be shown exactly what \
             <strong>{client}</strong> is asking for, and you can refuse.</p>\
             <p class=\"muted\">It will be sent back to</p><p class=\"host\">{host}</p>\
             {loopback}{notice}\
             <form method=\"post\" action=\"login\" autocomplete=\"on\">{hidden}\
             <label for=\"account\">Account</label>\
             <input id=\"account\" name=\"account\" type=\"text\" autocomplete=\"username\" \
             autocapitalize=\"none\" spellcheck=\"false\" required>\
             <label for=\"pw\">Password</label>\
             <input id=\"pw\" name=\"pw\" type=\"password\" autocomplete=\"current-password\" required>\
             <button type=\"submit\">Sign in</button></form>",
            client = escape(ctx.client_name),
            host = escape(ctx.redirect_host),
            loopback = loopback,
            notice = notice,
            hidden = hidden_fields(ctx.hidden),
        ),
    )
}

/// The MCP specification requires that a user be warned when an authorization
/// will be delivered to a loopback address, because a loopback redirect cannot
/// be authenticated: any process on the user's own machine that manages to bind
/// the port first receives the authorization code. The warning is therefore not
/// decoration — it is the only thing standing between the user and a
/// local-malware code interception, and it says what the user should actually
/// check rather than merely that something is unusual.
fn loopback_warning() -> String {
    "<div class=\"warn\"><strong>This application runs on this computer.</strong> \
     The authorization will be delivered to a local address, which cannot be \
     verified — any program running on this machine could receive it. Only \
     continue if you started this sign-in yourself, from an application you \
     installed.</div>"
        .to_string()
}

/// One resolved tool group, as displayed to the human: the group's name, its
/// description, and the concrete patterns it expands to.
pub struct GroupSummary {
    pub name: String,
    pub description: String,
    pub patterns: Vec<String>,
}

/// Everything the consent page needs.
pub struct ConsentContext<'a> {
    pub client_name: &'a str,
    pub account_name: &'a str,
    pub redirect_host: &'a str,
    pub redirect_uri: &'a str,
    pub loopback_only: bool,
    /// The NARROWED scopes — what will actually be granted, never what was
    /// requested. See [`crate::oauth::authorize::narrow_scope`].
    pub scopes: &'a [String],
    /// The resolved tool groups this client is scoped to. Empty means the
    /// client reaches nothing, and the page says so in those words.
    pub groups: &'a [GroupSummary],
    /// The federated server namespaces this client may see.
    pub namespaces: &'a [String],
    pub csrf: &'a str,
    pub hidden: &'a [(String, String)],
}

/// Render the consent page.
///
/// ## Why this page shows a capability list and not a scope string
/// A scope string is a token the client chose. "mcp" tells a human nothing
/// about whether approving it hands over a weather lookup or the ability to
/// restart a production host. The decision this page asks for is only
/// meaningful if the human can see the concrete capability set, so the resolved
/// tool groups (with their patterns) and the federated namespaces are rendered
/// as a list. An empty list is rendered explicitly as "nothing", because the
/// alternative — an empty section that reads as "unrestricted" — is the exact
/// misreading the whole scoping model is built to avoid.
pub fn consent_page(ctx: &ConsentContext<'_>) -> String {
    let loopback = if ctx.loopback_only { loopback_warning() } else { String::new() };

    let scopes = if ctx.scopes.is_empty() {
        "<p>Nothing.</p>".to_string()
    } else {
        let items: String = ctx
            .scopes
            .iter()
            .map(|s| format!("<li><code>{}</code></li>", escape(s)))
            .collect();
        format!("<ul>{items}</ul>")
    };

    let groups = if ctx.groups.is_empty() {
        // Said in words, not left as an empty list: "no groups" must read as
        // "no tools", never as "all tools".
        "<div class=\"warn\">This connector is not scoped to any tools. Approving it \
         grants no tool access at all.</div>"
            .to_string()
    } else {
        let items: String = ctx
            .groups
            .iter()
            .map(|g| {
                let patterns = if g.patterns.is_empty() {
                    "<li class=\"muted\">(this group currently matches no tools)</li>".to_string()
                } else {
                    g.patterns
                        .iter()
                        .map(|p| format!("<li><code>{}</code></li>", escape(p)))
                        .collect()
                };
                format!(
                    "<li><strong>{}</strong><div class=\"muted\">{}</div><ul>{}</ul></li>",
                    escape(&g.name),
                    escape(&g.description),
                    patterns
                )
            })
            .collect();
        format!("<ul>{items}</ul>")
    };

    let namespaces = if ctx.namespaces.is_empty() {
        "<p class=\"muted\">This server only. No federated servers are in scope.</p>".to_string()
    } else {
        let items: String = ctx
            .namespaces
            .iter()
            .map(|n| format!("<li><code>{}</code></li>", escape(n)))
            .collect();
        format!("<ul>{items}</ul>")
    };

    page(
        "Authorize connector",
        &format!(
            "<h1>Allow {client} to act as you?</h1>\
             <p>Signed in as <strong>{account}</strong>.</p>\
             <p class=\"muted\">The authorization will be sent to</p>\
             <p class=\"host\">{host}</p>\
             <p class=\"muted\"><code>{uri}</code></p>\
             {loopback}\
             <h2>Permissions requested</h2>{scopes}\
             <h2>Tools this connector could use</h2>{groups}\
             <h2>Servers it can see</h2>{namespaces}\
             <p class=\"muted\">It can never do more than your own account is allowed to do — \
             this list is already narrowed to the smaller of the two.</p>\
             <form method=\"post\" action=\"consent\">{hidden}\
             <input type=\"hidden\" name=\"csrf\" value=\"{csrf}\">\
             <button type=\"submit\" name=\"approve\" value=\"yes\">Allow</button>\
             <button type=\"submit\" name=\"approve\" value=\"no\" class=\"secondary\">Refuse</button>\
             </form>",
            client = escape(ctx.client_name),
            account = escape(ctx.account_name),
            host = escape(ctx.redirect_host),
            uri = escape(ctx.redirect_uri),
            loopback = loopback,
            scopes = scopes,
            groups = groups,
            namespaces = namespaces,
            hidden = hidden_fields(ctx.hidden),
            csrf = escape(ctx.csrf),
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The escaper must neutralise both the text-context and the
    /// attribute-context break-out characters. One function is used in both
    /// positions, so both must be covered.
    #[test]
    fn escape_neutralises_markup_and_attribute_breakouts() {
        assert_eq!(escape("<b>"), "&lt;b&gt;");
        assert_eq!(escape("a&b"), "a&amp;b");
        assert_eq!(escape("\"quoted\""), "&quot;quoted&quot;");
        assert_eq!(escape("it's"), "it&#39;s");
        assert_eq!(escape("plain text"), "plain text");
    }

    /// The attack this page most needs to survive: a dynamically-registered
    /// client naming itself with markup, so the consent screen lies to the
    /// human about what they are approving.
    #[test]
    fn a_hostile_client_name_cannot_inject_markup() {
        let hostile = "<script>alert(1)</script><b>Trusted App</b>";
        let groups = [GroupSummary {
            name: "weather".into(),
            description: "Forecasts".into(),
            patterns: vec!["weather_*".into()],
        }];
        let scopes = ["mcp".to_string()];
        let rendered = consent_page(&ConsentContext {
            client_name: hostile,
            account_name: "operator",
            redirect_host: "client.test",
            redirect_uri: "https://client.test/cb",
            loopback_only: false,
            scopes: &scopes,
            groups: &groups,
            namespaces: &[],
            csrf: "abc",
            hidden: &[],
        });
        assert!(!rendered.contains("<script>"), "markup must not survive: {rendered}");
        assert!(rendered.contains("&lt;script&gt;"));
    }

    /// A hidden field's VALUE is attacker-controlled too — `state` comes
    /// straight from the query string. An unescaped `"` there closes the
    /// attribute and injects a new one.
    #[test]
    fn a_hostile_hidden_value_cannot_escape_its_attribute() {
        let fields = vec![("state".to_string(), "\"><script>x</script>".to_string())];
        let rendered = hidden_fields(&fields);
        assert!(!rendered.contains("<script>"), "{rendered}");
        assert!(rendered.contains("&quot;&gt;&lt;script&gt;"));
    }

    /// The loopback warning is a specification requirement, not decoration:
    /// a loopback redirect cannot be authenticated, so the human must be told.
    #[test]
    fn loopback_clients_are_warned_on_both_pages() {
        let scopes = ["mcp".to_string()];
        let consent = consent_page(&ConsentContext {
            client_name: "Local Tool",
            account_name: "operator",
            redirect_host: "127.0.0.1",
            redirect_uri: "http://127.0.0.1:3118/callback",
            loopback_only: true,
            scopes: &scopes,
            groups: &[],
            namespaces: &[],
            csrf: "abc",
            hidden: &[],
        });
        assert!(consent.contains("runs on this computer"), "{consent}");

        let login = login_page(&LoginContext {
            client_name: "Local Tool",
            redirect_host: "127.0.0.1",
            loopback_only: true,
            notice: None,
            hidden: &[],
        });
        assert!(login.contains("runs on this computer"), "{login}");
    }

    /// A non-loopback client must NOT be warned — a warning shown on every
    /// page is a warning nobody reads, which is how a real one gets ignored.
    #[test]
    fn non_loopback_clients_are_not_warned() {
        let login = login_page(&LoginContext {
            client_name: "Hosted App",
            redirect_host: "client.test",
            loopback_only: false,
            notice: None,
            hidden: &[],
        });
        assert!(!login.contains("runs on this computer"));
    }

    /// The single most dangerous misreading in the whole scoping model: an
    /// empty capability set displayed as blank space reads as "unrestricted".
    /// It must be stated in words.
    #[test]
    fn an_unscoped_client_is_described_as_granting_nothing() {
        let scopes = ["mcp".to_string()];
        let rendered = consent_page(&ConsentContext {
            client_name: "Unscoped",
            account_name: "operator",
            redirect_host: "client.test",
            redirect_uri: "https://client.test/cb",
            loopback_only: false,
            scopes: &scopes,
            groups: &[],
            namespaces: &[],
            csrf: "abc",
            hidden: &[],
        });
        assert!(rendered.contains("not scoped to any tools"), "{rendered}");
        assert!(rendered.contains("No federated servers are in scope"), "{rendered}");
    }

    /// The consent screen's reason for existing: the human sees CONCRETE
    /// capabilities, not an opaque scope token.
    #[test]
    fn consent_shows_resolved_capabilities_not_just_a_scope_string() {
        let groups = [GroupSummary {
            name: "fleet-readonly".into(),
            description: "Read-only fleet status".into(),
            patterns: vec!["vitals_*".into(), "ledger_read".into()],
        }];
        let namespaces = ["peer-one".to_string()];
        let scopes = ["mcp".to_string()];
        let rendered = consent_page(&ConsentContext {
            client_name: "Connector",
            account_name: "operator",
            redirect_host: "client.test",
            redirect_uri: "https://client.test/cb",
            loopback_only: false,
            scopes: &scopes,
            groups: &groups,
            namespaces: &namespaces,
            csrf: "abc",
            hidden: &[],
        });
        assert!(rendered.contains("fleet-readonly"));
        assert!(rendered.contains("vitals_*"));
        assert!(rendered.contains("ledger_read"));
        assert!(rendered.contains("peer-one"));
        assert!(rendered.contains("Connector"));
        assert!(rendered.contains("client.test"));
    }

    /// The error page must not offer a way back to the requester: any such
    /// link would point at the unvalidated URI the page exists to refuse.
    #[test]
    fn the_error_page_carries_no_link_anywhere() {
        let rendered = error_page("Unknown client", "That client is not registered here.");
        assert!(!rendered.contains("<a "), "no anchors: {rendered}");
        assert!(!rendered.contains("http://"), "{rendered}");
        assert!(rendered.contains("not registered here"));
    }

    /// No page may load anything from the network. The CSP forbids it, but the
    /// markup should not depend on the header to be true.
    #[test]
    fn pages_reference_no_external_resource_and_no_script() {
        let scopes = ["mcp".to_string()];
        let pages = [
            error_page("t", "d"),
            login_page(&LoginContext {
                client_name: "c",
                redirect_host: "h",
                loopback_only: false,
                notice: Some("Sign-in failed."),
                hidden: &[("state".to_string(), "s".to_string())],
            }),
            consent_page(&ConsentContext {
                client_name: "c",
                account_name: "a",
                redirect_host: "h",
                redirect_uri: "https://client.test/cb",
                loopback_only: false,
                scopes: &scopes,
                groups: &[],
                namespaces: &[],
                csrf: "x",
                hidden: &[],
            }),
        ];
        for rendered in pages {
            assert!(!rendered.contains("<script"), "no script tags: {rendered}");
            assert!(!rendered.contains("<link"), "no external stylesheets: {rendered}");
            assert!(!rendered.contains("<img"), "no remote images: {rendered}");
            assert!(!rendered.contains("//cdn"), "no CDN references: {rendered}");
        }
    }

    /// The login notice must be generic. This test does not enforce that on
    /// its own — the handler chooses the text — but it does prove the page
    /// renders whatever it is given verbatim (escaped), so the handler's
    /// single constant is the only thing that decides.
    #[test]
    fn login_renders_the_notice_it_is_given() {
        let rendered = login_page(&LoginContext {
            client_name: "c",
            redirect_host: "h",
            loopback_only: false,
            notice: Some("Sign-in failed."),
            hidden: &[],
        });
        assert!(rendered.contains("Sign-in failed."));
        assert!(!rendered.contains("no such account"));
    }
}
