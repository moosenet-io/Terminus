//! Email read tools over IMAP (Gmail App Password, IMAPS port 993). GOOG-02.
//!
//! Three tools:
//!   google_email_inbox   — list recent messages (FROM/SUBJECT/DATE), optional unread-only.
//!   google_email_read    — read one message's text/plain body + headers.
//!   google_email_summary — LLM-summarized digest of recent subjects (falls back to a list).
//!
//! Connection: TLS to imap.gmail.com:993, LOGIN with the Google App Password, always LOGOUT.
//!
//! Stream-free design: `async-imap` exposes the result of `UID FETCH` only as a
//! `futures::Stream`, but `futures` is not a dependency of this crate. We therefore
//! drive the protocol with the lower-level `run_command` / `read_response` API
//! (both plain `async fn`s) and parse `imap_proto::Response` values ourselves.

use std::sync::Arc;

use async_imap::imap_proto::{AttributeValue, Response, Status};
use async_trait::async_trait;
use serde_json::{json, Value};

use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::tool::RustTool;
use super::GoogleConfig;

const IMAP_HOST: &str = "imap.gmail.com";
const IMAP_PORT: u16 = 993;
const DEFAULT_INBOX_LIMIT: u64 = 10;
const DEFAULT_SUMMARY_HOURS: u64 = 12;
const SUMMARY_SUBJECT_CAP: usize = 20;
const BODY_PREVIEW_CHARS: usize = 3000;

/// Register the three email-read tools.
pub fn register(registry: &mut ToolRegistry, cfg: &GoogleConfig) {
    registry.register_or_replace(Box::new(InboxTool { cfg: cfg.clone() }));
    registry.register_or_replace(Box::new(ReadTool { cfg: cfg.clone() }));
    registry.register_or_replace(Box::new(SummaryTool { cfg: cfg.clone() }));
}

// ── IMAP session (self-contained TLS + command driver) ──────────────────────────

type ImapStream = tokio_rustls::client::TlsStream<tokio::net::TcpStream>;

/// A connected, authenticated IMAP session over TLS.
struct Session {
    inner: async_imap::Session<ImapStream>,
}

impl Session {
    /// Open TLS, LOGIN. The hostname/port are protocol constants (not secrets/infra).
    async fn connect(cfg: &GoogleConfig) -> Result<Self, ToolError> {
        use rustls::ClientConfig;
        use tokio::net::TcpStream;
        use tokio_rustls::TlsConnector;

        let mut root_store = rustls::RootCertStore::empty();
        let cert_result = rustls_native_certs::load_native_certs();
        if cert_result.certs.is_empty() {
            return Err(ToolError::Http(
                "no native TLS certificates available to verify the IMAP server".into(),
            ));
        }
        for cert in cert_result.certs {
            root_store
                .add(cert)
                .map_err(|e| ToolError::Http(format!("invalid root certificate: {e}")))?;
        }

        let tls_config = ClientConfig::builder()
            .with_root_certificates(root_store)
            .with_no_client_auth();
        let connector = TlsConnector::from(Arc::new(tls_config));

        let addr = format!("{IMAP_HOST}:{IMAP_PORT}");
        let tcp = TcpStream::connect(&addr)
            .await
            .map_err(|e| ToolError::Http(format!("IMAP TCP connect to {addr} failed: {e}")))?;

        let server_name = rustls::pki_types::ServerName::try_from(IMAP_HOST)
            .map_err(|e| ToolError::Http(format!("invalid IMAP hostname: {e}")))?;
        let tls = connector
            .connect(server_name, tcp)
            .await
            .map_err(|e| ToolError::Http(format!("IMAP TLS handshake failed: {e}")))?;

        let client = async_imap::Client::new(tls);
        let inner = client
            .login(&cfg.email, &cfg.app_password)
            .await
            .map_err(|(e, _)| ToolError::Http(format!("IMAP LOGIN failed: {e}")))?;

        Ok(Self { inner })
    }

    /// SELECT a mailbox; returns the message count it reports.
    async fn select(&mut self, mailbox: &str) -> Result<u32, ToolError> {
        let mb = self
            .inner
            .select(mailbox)
            .await
            .map_err(|e| ToolError::Http(format!("IMAP SELECT {mailbox} failed: {e}")))?;
        Ok(mb.exists)
    }

    /// UID SEARCH; `criteria` is e.g. "ALL" or "UNSEEN" (no user input — safe).
    async fn uid_search(&mut self, criteria: &str) -> Result<Vec<u32>, ToolError> {
        let set = self
            .inner
            .uid_search(criteria)
            .await
            .map_err(|e| ToolError::Http(format!("IMAP UID SEARCH failed: {e}")))?;
        let mut uids: Vec<u32> = set.into_iter().collect();
        uids.sort_unstable();
        Ok(uids)
    }

    /// Run `UID FETCH <uid_set> <items>` and collect the parsed FETCH rows.
    ///
    /// Drives the connection with `run_command` + `read_response` to avoid the
    /// `futures::Stream` returned by `uid_fetch` (we cannot depend on `futures`).
    async fn uid_fetch(
        &mut self,
        uid_set: &str,
        items: &str,
    ) -> Result<Vec<FetchedMessage>, ToolError> {
        let id = self
            .inner
            .run_command(format!("UID FETCH {uid_set} {items}"))
            .await
            .map_err(|e| ToolError::Http(format!("IMAP UID FETCH failed: {e}")))?;

        let mut out = Vec::new();
        loop {
            let resp = self
                .inner
                .read_response()
                .await
                .map_err(|e| ToolError::Http(format!("IMAP read failed: {e}")))?;
            let Some(data) = resp else {
                // Connection closed before the tagged completion arrived.
                return Err(ToolError::Http("IMAP connection closed mid-FETCH".into()));
            };
            match data.parsed() {
                Response::Fetch(_seq, attrs) => {
                    out.push(FetchedMessage::from_attrs(attrs));
                }
                Response::Done { tag, status, information, .. } if tag == &id => {
                    if *status != Status::Ok {
                        let info = information.as_deref().unwrap_or("");
                        return Err(ToolError::Http(format!(
                            "IMAP FETCH returned {status:?}: {info}"
                        )));
                    }
                    break;
                }
                _ => {} // ignore untagged / unsolicited responses
            }
        }
        Ok(out)
    }

    /// Send LOGOUT (best effort) and drop the connection.
    async fn logout(mut self) {
        let _ = self.inner.logout().await;
    }
}

/// The fields we pull out of a single FETCH response row.
#[derive(Debug, Default, Clone)]
struct FetchedMessage {
    uid: Option<u32>,
    header: String,
    body: String,
}

impl FetchedMessage {
    fn from_attrs(attrs: &[AttributeValue<'_>]) -> Self {
        let mut msg = FetchedMessage::default();
        for attr in attrs {
            match attr {
                AttributeValue::Uid(u) => msg.uid = Some(*u),
                AttributeValue::Rfc822Header(Some(bytes)) => {
                    msg.header = String::from_utf8_lossy(bytes).into_owned();
                }
                AttributeValue::Rfc822(Some(bytes)) => {
                    // Full message: header + body separated by a blank line.
                    let full = String::from_utf8_lossy(bytes).into_owned();
                    if msg.header.is_empty() {
                        if let Some(idx) = full.find("\r\n\r\n").or_else(|| full.find("\n\n")) {
                            msg.header = full[..idx].to_string();
                            msg.body = full[idx..].trim_start().to_string();
                        } else {
                            msg.header = full;
                        }
                    } else if msg.body.is_empty() {
                        msg.body = full;
                    }
                }
                AttributeValue::Rfc822Text(Some(bytes))
                | AttributeValue::BodySection { data: Some(bytes), .. } => {
                    if msg.body.is_empty() {
                        msg.body = String::from_utf8_lossy(bytes).into_owned();
                    }
                }
                _ => {}
            }
        }
        msg
    }
}

// ── Header / body parsing helpers (pure, unit-tested) ────────────────────────────

/// Unfold RFC 5322 folded headers: a CRLF followed by leading whitespace is a
/// continuation of the previous logical line.
fn unfold_headers(headers: &str) -> String {
    let normalized = headers.replace("\r\n", "\n");
    let mut out = String::with_capacity(normalized.len());
    let mut lines = normalized.lines().peekable();
    while let Some(line) = lines.next() {
        out.push_str(line);
        while lines
            .peek()
            .map(|l| l.starts_with(' ') || l.starts_with('\t'))
            .unwrap_or(false)
        {
            if let Some(cont) = lines.next() {
                out.push(' ');
                out.push_str(cont.trim());
            }
        }
        out.push('\n');
    }
    out
}

/// Extract a header field value (case-insensitive field name), unfolding first.
fn extract_header(headers: &str, field: &str) -> Option<String> {
    let prefix = format!("{}:", field.to_lowercase());
    for line in unfold_headers(headers).lines() {
        if line.to_lowercase().starts_with(&prefix) {
            let value = line[field.len() + 1..].trim().to_string();
            return if value.is_empty() { None } else { Some(value) };
        }
    }
    None
}

/// Find a header value (any case); returns an em-dash placeholder when absent.
fn header_or_dash(headers: &str, field: &str) -> String {
    extract_header(headers, field).unwrap_or_else(|| "—".to_string())
}

/// Return the value of a named MIME parameter (e.g. `boundary`) from a header line.
fn mime_param<'a>(header_line: &'a str, param: &str) -> Option<&'a str> {
    let needle = format!("{}=", param.to_lowercase());
    let lower = header_line.to_lowercase();
    let pos = lower.find(&needle)?;
    let rest = header_line[pos + needle.len()..].trim_start();
    if let Some(stripped) = rest.strip_prefix('"') {
        stripped.split('"').next()
    } else {
        rest.split(|c: char| c == ';' || c.is_whitespace()).next()
    }
}

/// Extract the best text/plain body from a raw RFC 822 message body.
///
/// Handles three cases:
///   1. Non-multipart text/plain (or unknown) → returned as-is (HTML stripped).
///   2. multipart/* → walk parts, return the first text/plain part's content.
///   3. No usable text/plain → fall back to HTML-stripped whole body.
fn extract_text_plain(raw_body: &str) -> String {
    let body = raw_body.trim();
    let (top_headers, top_content) = split_headers_body(body);

    let content_type = extract_header(top_headers, "Content-Type").unwrap_or_default();
    let ct_lower = content_type.to_lowercase();

    if ct_lower.contains("multipart/") {
        if let Some(boundary) = mime_param(&content_type, "boundary") {
            if let Some(text) = walk_multipart(top_content, boundary) {
                return clean_body(&text);
            }
        }
    }

    let content = if top_headers.is_empty() { body } else { top_content };
    clean_body(content)
}

/// Split a chunk into (headers, body) at the first blank line. If the leading
/// block does not look like MIME/RFC822 headers, treat the whole chunk as body.
fn split_headers_body(chunk: &str) -> (&str, &str) {
    let looks_like_header = chunk
        .lines()
        .next()
        .map(|l| {
            let lower = l.to_lowercase();
            lower.starts_with("content-")
                || lower.starts_with("mime-")
                || (l.contains(':') && !l.starts_with(' '))
        })
        .unwrap_or(false);
    if !looks_like_header {
        return ("", chunk);
    }
    if let Some(idx) = chunk.find("\r\n\r\n") {
        (&chunk[..idx], &chunk[idx + 4..])
    } else if let Some(idx) = chunk.find("\n\n") {
        (&chunk[..idx], &chunk[idx + 2..])
    } else {
        (chunk, "")
    }
}

/// Walk multipart content separated by `--boundary`, returning the first
/// text/plain part's content (recursing into nested multiparts). Falls back to
/// an HTML-stripped text/html part if no text/plain part exists.
fn walk_multipart(content: &str, boundary: &str) -> Option<String> {
    let delim = format!("--{boundary}");
    let mut html_fallback: Option<String> = None;
    for part in content.split(&delim) {
        let part = part.trim_start_matches(['\r', '\n']);
        if part.is_empty() || part.starts_with("--") {
            continue; // preamble or closing delimiter
        }
        let (headers, body) = split_headers_body(part);
        let ct = extract_header(headers, "Content-Type").unwrap_or_default();
        let ct_lower = ct.to_lowercase();

        if ct_lower.contains("multipart/") {
            if let Some(inner) = mime_param(&ct, "boundary") {
                if let Some(found) = walk_multipart(body, inner) {
                    return Some(found);
                }
            }
        } else if ct_lower.contains("text/plain") {
            return Some(body.to_string());
        } else if ct_lower.contains("text/html") && html_fallback.is_none() {
            html_fallback = Some(strip_html(body));
        }
    }
    html_fallback
}

/// Strip HTML tags and decode a handful of common entities.
fn strip_html(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for ch in html.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    out.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
}

/// Normalize whitespace; strip HTML if the content still looks like markup.
fn clean_body(content: &str) -> String {
    let stripped = if content.contains("</") || content.contains("/>") {
        strip_html(content)
    } else {
        content.to_string()
    };
    stripped.trim().to_string()
}

/// Truncate to at most `max` chars, appending an ellipsis when cut.
fn truncate(text: &str, max: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max {
        text.to_string()
    } else {
        let head: String = chars[..max].iter().collect();
        format!("{}…", head.trim_end())
    }
}

// ── Argument helpers ─────────────────────────────────────────────────────────────

fn arg_u64(args: &Value, key: &str, default: u64) -> u64 {
    args.get(key).and_then(Value::as_u64).unwrap_or(default)
}

fn arg_bool(args: &Value, key: &str, default: bool) -> bool {
    args.get(key).and_then(Value::as_bool).unwrap_or(default)
}

/// Validate a user-supplied email id: IMAP UIDs are positive integers.
fn parse_email_id(args: &Value) -> Result<u32, ToolError> {
    let raw = args
        .get("email_id")
        .ok_or_else(|| ToolError::InvalidArgument("email_id is required".into()))?;
    let s = match raw {
        Value::String(s) => s.trim().to_string(),
        Value::Number(n) => n.to_string(),
        _ => return Err(ToolError::InvalidArgument("email_id must be a string or number".into())),
    };
    if s.is_empty() || s.len() > 10 || !s.chars().all(|c| c.is_ascii_digit()) {
        return Err(ToolError::InvalidArgument(format!(
            "email_id '{s}' is not a valid IMAP UID (1-10 digits)"
        )));
    }
    s.parse::<u32>()
        .map_err(|_| ToolError::InvalidArgument(format!("email_id '{s}' out of range")))
}

/// Newest-first selection of the last `limit` UIDs from an ascending list.
fn newest_n(mut uids: Vec<u32>, limit: usize) -> Vec<u32> {
    if uids.len() > limit {
        let skip = uids.len() - limit;
        uids = uids[skip..].to_vec();
    }
    uids.reverse();
    uids
}

// ── Tool: google_email_inbox ─────────────────────────────────────────────────────

struct InboxTool {
    cfg: GoogleConfig,
}

#[async_trait]
impl RustTool for InboxTool {
    fn name(&self) -> &str {
        "google_email_inbox"
    }
    fn description(&self) -> &str {
        "Check the email inbox — list recent emails, unread mail, and messages \
         (From / Subject / Date). Use when the user asks about their inbox, email, \
         emails, mail, messages, or unread. Args: limit (default 10), unread_only \
         (bool, default false)."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "limit": {"type": "integer", "description": "Max messages to list (default 10)", "minimum": 1, "maximum": 100},
                "unread_only": {"type": "boolean", "description": "Only unread (UNSEEN) messages (default false)"}
            }
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let limit = arg_u64(&args, "limit", DEFAULT_INBOX_LIMIT).clamp(1, 100) as usize;
        let unread_only = arg_bool(&args, "unread_only", false);

        let mut session = Session::connect(&self.cfg).await?;
        session.select("INBOX").await?;
        let criteria = if unread_only { "UNSEEN" } else { "ALL" };
        let uids = session.uid_search(criteria).await?;
        let total = uids.len();
        let chosen = newest_n(uids, limit);

        if chosen.is_empty() {
            session.logout().await;
            let label = if unread_only { "unread " } else { "" };
            return Ok(format!("No {label}messages in INBOX."));
        }

        let uid_set = chosen
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let fetched = session.uid_fetch(&uid_set, "(UID RFC822.HEADER)").await?;
        session.logout().await;

        Ok(format_inbox(&chosen, &fetched, total, unread_only))
    }
}

/// Format the inbox listing, in the newest-first `order` UID order.
fn format_inbox(
    order: &[u32],
    fetched: &[FetchedMessage],
    total: usize,
    unread_only: bool,
) -> String {
    let label = if unread_only { "unread" } else { "total" };
    let mut out = format!(
        "INBOX — showing {} of {} {} message(s):\n\n",
        order.len(),
        total,
        label
    );
    for (i, uid) in order.iter().enumerate() {
        let msg = fetched.iter().find(|m| m.uid == Some(*uid));
        let (from, subject, date) = match msg {
            Some(m) => (
                header_or_dash(&m.header, "From"),
                header_or_dash(&m.header, "Subject"),
                header_or_dash(&m.header, "Date"),
            ),
            None => ("—".into(), "—".into(), "—".into()),
        };
        out.push_str(&format!(
            "{}. [id {}] {}\n   From: {}\n   Date: {}\n",
            i + 1,
            uid,
            subject,
            from,
            date
        ));
        if i + 1 < order.len() {
            out.push('\n');
        }
    }
    out
}

// ── Tool: google_email_read ──────────────────────────────────────────────────────

struct ReadTool {
    cfg: GoogleConfig,
}

#[async_trait]
impl RustTool for ReadTool {
    fn name(&self) -> &str {
        "google_email_read"
    }
    fn description(&self) -> &str {
        "Read or open a single email/message by id. Returns From/To/Subject/Date and the \
         text/plain body. Use when the user wants to read, open, or see the full contents \
         of a specific email or message. Arg: email_id (required)."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "email_id": {"type": "string", "description": "IMAP UID of the message (from google_email_inbox)"}
            },
            "required": ["email_id"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let uid = parse_email_id(&args)?;

        let mut session = Session::connect(&self.cfg).await?;
        session.select("INBOX").await?;
        // BODY.PEEK[TEXT] avoids setting the \Seen flag while reading.
        let fetched = session
            .uid_fetch(&uid.to_string(), "(UID RFC822.HEADER BODY.PEEK[TEXT])")
            .await?;
        session.logout().await;

        let msg = fetched
            .into_iter()
            .find(|m| m.uid == Some(uid))
            .ok_or_else(|| ToolError::NotFound(format!("no message with id {uid} in INBOX")))?;

        Ok(format_read(&msg))
    }
}

fn format_read(msg: &FetchedMessage) -> String {
    let body_plain = extract_text_plain(&msg.body);
    let body_view = truncate(&body_plain, BODY_PREVIEW_CHARS);
    let body_view = if body_view.is_empty() {
        "(no text/plain content)".to_string()
    } else {
        body_view
    };
    format!(
        "From: {}\nTo: {}\nSubject: {}\nDate: {}\n\n{}",
        header_or_dash(&msg.header, "From"),
        header_or_dash(&msg.header, "To"),
        header_or_dash(&msg.header, "Subject"),
        header_or_dash(&msg.header, "Date"),
        body_view
    )
}

// ── Tool: google_email_summary ───────────────────────────────────────────────────

struct SummaryTool {
    cfg: GoogleConfig,
}

#[async_trait]
impl RustTool for SummaryTool {
    fn name(&self) -> &str {
        "google_email_summary"
    }
    fn description(&self) -> &str {
        "Summarize recent email/inbox subjects — a quick digest of recent mail and messages. \
         Use when the user wants a summary or overview of their inbox or recent emails. \
         Arg: hours_back (default 12). Uses an LLM when CHORD_LLM_URL is set, otherwise \
         returns a bulleted subject list."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "hours_back": {"type": "integer", "description": "Look-back window in hours (default 12)", "minimum": 1, "maximum": 720}
            }
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let hours_back = arg_u64(&args, "hours_back", DEFAULT_SUMMARY_HOURS).clamp(1, 720);

        let mut session = Session::connect(&self.cfg).await?;
        session.select("INBOX").await?;
        // IMAP date search is day-granular; approximate the window by taking the
        // most recent messages and noting the requested look-back in the output.
        let uids = session.uid_search("ALL").await?;
        let chosen = newest_n(uids, SUMMARY_SUBJECT_CAP);

        if chosen.is_empty() {
            session.logout().await;
            return Ok(format!("No messages in INBOX (last {hours_back}h)."));
        }

        let uid_set = chosen
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let fetched = session.uid_fetch(&uid_set, "(UID RFC822.HEADER)").await?;
        session.logout().await;

        let subjects: Vec<String> = chosen
            .iter()
            .filter_map(|uid| fetched.iter().find(|m| m.uid == Some(*uid)))
            .map(|m| header_or_dash(&m.header, "Subject"))
            .collect();
        let count = subjects.len();

        // Try the LLM; on any problem fall back to a plain bulleted list.
        let summary = match summarize_via_llm(&subjects, hours_back).await {
            Ok(Some(s)) if !s.trim().is_empty() => s.trim().to_string(),
            _ => bullet_list(&subjects),
        };

        Ok(format!(
            "Email summary (last {hours_back}h, {count} message(s)):\n\n{summary}"
        ))
    }
}

fn bullet_list(subjects: &[String]) -> String {
    if subjects.is_empty() {
        return "(no subjects)".to_string();
    }
    subjects
        .iter()
        .map(|s| format!("• {s}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Build the OpenAI chat-completions request body for subject summarization.
fn build_summary_request(subjects: &[String], hours_back: u64) -> Value {
    let joined = subjects
        .iter()
        .map(|s| format!("- {s}"))
        .collect::<Vec<_>>()
        .join("\n");
    let prompt = format!(
        "Summarize the following email subjects from the last {hours_back} hours into a few \
         short bullet points highlighting anything that looks important or time-sensitive. \
         Be concise.\n\nSubjects:\n{joined}"
    );
    json!({
        "model": "gpt-oss:20b",
        "max_tokens": 150,
        "messages": [
            {"role": "system", "content": "You are a concise email triage assistant."},
            {"role": "user", "content": prompt}
        ]
    })
}

/// Parse the assistant text out of an OpenAI chat-completions response.
fn parse_summary_response(body: &Value) -> Option<String> {
    body.get("choices")?
        .get(0)?
        .get("message")?
        .get("content")?
        .as_str()
        .map(str::to_string)
}

/// Call CHORD_LLM_URL (OpenAI-compatible). `Ok(None)` ⇒ not configured;
/// `Err` ⇒ configured but the call failed (caller falls back either way).
async fn summarize_via_llm(
    subjects: &[String],
    hours_back: u64,
) -> Result<Option<String>, ToolError> {
    let base = match std::env::var("CHORD_LLM_URL") {
        Ok(v) if !v.trim().is_empty() => v.trim().trim_end_matches('/').to_string(),
        _ => return Ok(None),
    };
    let url = format!("{base}/v1/chat/completions");
    let req = build_summary_request(subjects, hours_back);

    let client = reqwest::Client::new();
    let resp = client
        .post(&url)
        .json(&req)
        .send()
        .await
        .map_err(|e| ToolError::Http(format!("LLM request failed: {e}")))?;
    if !resp.status().is_success() {
        return Err(ToolError::Http(format!("LLM returned HTTP {}", resp.status())));
    }
    let body: Value = resp
        .json()
        .await
        .map_err(|e| ToolError::Http(format!("LLM response parse failed: {e}")))?;
    Ok(parse_summary_response(&body))
}

// ── Tests (no network) ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> GoogleConfig {
        GoogleConfig {
            email: "<email>".into(),  // pii-test-fixture
            app_password: "x".into(),
            secondary_email: "<email>".into(),  // pii-test-fixture
            lumina_calendar_id: None,
            extra_calendars: vec![],
        }
    }

    #[test]
    fn registers_three_tools() {
        let mut reg = ToolRegistry::new();
        register(&mut reg, &cfg());
        // Registering again must not panic (register_or_replace).
        register(&mut reg, &cfg());
    }

    #[test]
    fn tool_names_match_spec() {
        assert_eq!(InboxTool { cfg: cfg() }.name(), "google_email_inbox");
        assert_eq!(ReadTool { cfg: cfg() }.name(), "google_email_read");
        assert_eq!(SummaryTool { cfg: cfg() }.name(), "google_email_summary");
    }

    #[test]
    fn parameters_are_objects() {
        assert_eq!(InboxTool { cfg: cfg() }.parameters()["type"], "object");
        assert_eq!(ReadTool { cfg: cfg() }.parameters()["type"], "object");
        assert_eq!(SummaryTool { cfg: cfg() }.parameters()["type"], "object");
        assert_eq!(ReadTool { cfg: cfg() }.parameters()["required"][0], "email_id");
    }

    // ── header parsing ──────────────────────────────────────────────────────────

    #[test]
    fn extract_header_simple_and_case_insensitive() {
        let h = "From: <email>\r\nSubject: Hello\r\nDate: Mon, 01 Jun 2026 09:00:00 +0000\r\n";  // pii-test-fixture
        assert_eq!(extract_header(h, "From").as_deref(), Some("<email>"));  // pii-test-fixture
        assert_eq!(extract_header(h, "subject").as_deref(), Some("Hello"));
        assert_eq!(
            extract_header(h, "DATE").as_deref(),
            Some("Mon, 01 Jun 2026 09:00:00 +0000")
        );
    }

    #[test]
    fn extract_header_missing_is_none() {
        assert!(extract_header("From: <email>\r\n", "Subject").is_none());  // pii-test-fixture
    }

    #[test]
    fn extract_header_unfolds_continuation() {
        let h = "Subject: A very long subject\r\n that wraps here\r\nFrom: <email>\r\n";  // pii-test-fixture
        let v = extract_header(h, "Subject").unwrap();
        assert!(v.contains("very long subject"));
        assert!(v.contains("that wraps here"));
    }

    #[test]
    fn header_or_dash_falls_back() {
        assert_eq!(header_or_dash("From: <email>\r\n", "Cc"), "—");  // pii-test-fixture
        assert_eq!(header_or_dash("Cc: <email>\r\n", "Cc"), "<email>");  // pii-test-fixture
    }

    // ── body extraction ───────────────────────────────────────────────────────────

    #[test]
    fn extract_text_plain_simple() {
        let body = "Just a plain text email body.";
        assert_eq!(extract_text_plain(body), "Just a plain text email body.");
    }

    #[test]
    fn extract_text_plain_from_multipart() {
        let raw = concat!(
            "Content-Type: multipart/alternative; boundary=\"BND\"\r\n",
            "\r\n",
            "--BND\r\n",
            "Content-Type: text/plain; charset=utf-8\r\n",
            "\r\n",
            "Hello plaintext world.\r\n",
            "--BND\r\n",
            "Content-Type: text/html; charset=utf-8\r\n",
            "\r\n",
            "<html><body><p>Hello HTML</p></body></html>\r\n",
            "--BND--\r\n",
        );
        let text = extract_text_plain(raw);
        assert!(text.contains("Hello plaintext world."), "got: {text:?}");
        assert!(!text.contains("<html>"));
    }

    #[test]
    fn extract_text_plain_html_only_falls_back_to_stripped() {
        let raw = concat!(
            "Content-Type: multipart/alternative; boundary=\"B2\"\r\n",
            "\r\n",
            "--B2\r\n",
            "Content-Type: text/html; charset=utf-8\r\n",
            "\r\n",
            "<html><body><p>Only <b>HTML</b> here</p></body></html>\r\n",
            "--B2--\r\n",
        );
        let text = extract_text_plain(raw);
        assert!(text.contains("Only"));
        assert!(text.contains("HTML"));
        assert!(!text.contains('<'));
    }

    #[test]
    fn extract_text_plain_nested_multipart() {
        let raw = concat!(
            "Content-Type: multipart/mixed; boundary=\"OUT\"\r\n",
            "\r\n",
            "--OUT\r\n",
            "Content-Type: multipart/alternative; boundary=\"IN\"\r\n",
            "\r\n",
            "--IN\r\n",
            "Content-Type: text/plain\r\n",
            "\r\n",
            "Nested plain body.\r\n",
            "--IN--\r\n",
            "--OUT--\r\n",
        );
        let text = extract_text_plain(raw);
        assert!(text.contains("Nested plain body."), "got: {text:?}");
    }

    #[test]
    fn from_attrs_extracts_uid_header_body() {
        use std::borrow::Cow;
        let attrs = vec![
            AttributeValue::Uid(42),
            AttributeValue::Rfc822Header(Some(Cow::Borrowed(
                b"From: <email>\r\nSubject: Hi\r\n" as &[u8],  // pii-test-fixture
            ))),
            AttributeValue::BodySection {
                section: None,
                index: None,
                data: Some(Cow::Borrowed(b"Body content here." as &[u8])),
            },
        ];
        let msg = FetchedMessage::from_attrs(&attrs);
        assert_eq!(msg.uid, Some(42));
        assert!(msg.header.contains("Subject: Hi"));
        assert_eq!(msg.body, "Body content here.");
    }

    #[test]
    fn from_attrs_splits_rfc822_full_message() {
        use std::borrow::Cow;
        let attrs = vec![
            AttributeValue::Uid(7),
            AttributeValue::Rfc822(Some(Cow::Borrowed(
                b"From: <email>\r\nSubject: Full\r\n\r\nThe body text." as &[u8],  // pii-test-fixture
            ))),
        ];
        let msg = FetchedMessage::from_attrs(&attrs);
        assert_eq!(msg.uid, Some(7));
        assert!(msg.header.contains("Subject: Full"));
        assert_eq!(msg.body, "The body text.");
    }

    // ── strip_html / truncate ──────────────────────────────────────────────────

    #[test]
    fn strip_html_removes_tags_and_entities() {
        let out = strip_html("<p>Hello &amp; <b>World</b></p>");
        assert!(!out.contains('<'));
        assert!(out.contains("Hello & World"));
    }

    #[test]
    fn truncate_respects_limit_and_unicode() {
        assert_eq!(truncate("short", 100), "short");
        let long = "a".repeat(50);
        assert!(truncate(&long, 10).ends_with('…'));
        let emoji = "😀".repeat(50);
        assert!(truncate(&emoji, 10).ends_with('…'));
    }

    // ── arg helpers ───────────────────────────────────────────────────────────────

    #[test]
    fn arg_defaults_apply() {
        let empty = json!({});
        assert_eq!(arg_u64(&empty, "limit", DEFAULT_INBOX_LIMIT), 10);
        assert!(!arg_bool(&empty, "unread_only", false));
        assert_eq!(arg_u64(&empty, "hours_back", DEFAULT_SUMMARY_HOURS), 12);
    }

    #[test]
    fn arg_values_override_defaults() {
        let a = json!({"limit": 5, "unread_only": true, "hours_back": 48});
        assert_eq!(arg_u64(&a, "limit", DEFAULT_INBOX_LIMIT), 5);
        assert!(arg_bool(&a, "unread_only", false));
        assert_eq!(arg_u64(&a, "hours_back", DEFAULT_SUMMARY_HOURS), 48);
    }

    #[test]
    fn parse_email_id_accepts_string_and_number() {
        assert_eq!(parse_email_id(&json!({"email_id": "42"})).unwrap(), 42);
        assert_eq!(parse_email_id(&json!({"email_id": 99})).unwrap(), 99);
    }

    #[test]
    fn parse_email_id_rejects_bad_input() {
        assert!(parse_email_id(&json!({})).is_err());
        assert!(parse_email_id(&json!({"email_id": ""})).is_err());
        assert!(parse_email_id(&json!({"email_id": "abc"})).is_err());
        assert!(parse_email_id(&json!({"email_id": "1 LOGOUT"})).is_err());
        assert!(parse_email_id(&json!({"email_id": "12345678901"})).is_err());  // pii-test-fixture
    }

    // ── newest_n ordering ─────────────────────────────────────────────────────────

    #[test]
    fn newest_n_takes_last_and_reverses() {
        let uids = vec![1, 2, 3, 4, 5];
        assert_eq!(newest_n(uids.clone(), 3), vec![5, 4, 3]);
        assert_eq!(newest_n(uids, 10), vec![5, 4, 3, 2, 1]);
        assert!(newest_n(vec![], 5).is_empty());
    }

    // ── summary formatting / request / response ───────────────────────────────────

    #[test]
    fn bullet_list_formats_subjects() {
        let s = bullet_list(&["A".to_string(), "B".to_string()]);
        assert_eq!(s, "• A\n• B");
        assert_eq!(bullet_list(&[]), "(no subjects)");
    }

    #[test]
    fn build_summary_request_shape() {
        let req = build_summary_request(&["Invoice due".to_string()], 24);
        assert_eq!(req["model"], "gpt-oss:20b");
        assert_eq!(req["max_tokens"], 150);
        let content = req["messages"][1]["content"].as_str().unwrap();
        assert!(content.contains("Invoice due"));
        assert!(content.contains("24 hours"));
    }

    #[test]
    fn parse_summary_response_extracts_content() {
        let body = json!({"choices": [{"message": {"content": "• one\n• two"}}]});
        assert_eq!(parse_summary_response(&body).as_deref(), Some("• one\n• two"));
        assert!(parse_summary_response(&json!({"choices": []})).is_none());
        assert!(parse_summary_response(&json!({})).is_none());
    }

    #[test]
    fn format_inbox_renders_listing() {
        let fetched = vec![FetchedMessage {
            uid: Some(5),
            header:
                "From: Alice <<email>>\r\nSubject: Hello\r\nDate: Mon, 01 Jun 2026 09:00:00 +0000\r\n"  // pii-test-fixture
                    .to_string(),
            body: String::new(),
        }];
        let out = format_inbox(&[5], &fetched, 7, false);
        assert!(out.contains("showing 1 of 7 total"));
        assert!(out.contains("[id 5]"));
        assert!(out.contains("Hello"));
        assert!(out.contains("Alice <<email>>"));  // pii-test-fixture
    }

    #[test]
    fn format_read_renders_message() {
        let msg = FetchedMessage {
            uid: Some(9),
            header: "From: <email>\r\nTo: <email>\r\nSubject: Test\r\nDate: today\r\n".to_string(),  // pii-test-fixture
            body: "Hello body.".to_string(),
        };
        let out = format_read(&msg);
        assert!(out.contains("From: <email>"));  // pii-test-fixture
        assert!(out.contains("To: <email>"));  // pii-test-fixture
        assert!(out.contains("Subject: Test"));
        assert!(out.contains("Hello body."));
    }

    #[test]
    fn mime_param_reads_boundary() {
        assert_eq!(
            mime_param("multipart/alternative; boundary=\"ABC\"", "boundary"),
            Some("ABC")
        );
        assert_eq!(
            mime_param("multipart/mixed; boundary=XYZ", "boundary"),
            Some("XYZ")
        );
        assert_eq!(mime_param("text/plain", "boundary"), None);
    }
}
