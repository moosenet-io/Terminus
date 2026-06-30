//! Email send over SMTP. IMPLEMENTED BY GOOG-03.
//!
//! Registers `google_email_send`: build a plain-text message and deliver it via
//! Gmail SMTP (smtp.gmail.com:587, STARTTLS) authenticated with the account's
//! App Password. No OAuth — works with any STARTTLS SMTP provider.

use async_trait::async_trait;
use serde_json::{json, Value};

use lettre::message::Mailbox;
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};

use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::tool::RustTool;
use super::GoogleConfig;

const SMTP_HOST: &str = "smtp.gmail.com";
const SMTP_PORT: u16 = 587;

/// Validate that a string looks like an email address: a single `@`, with a
/// non-empty local part and a domain that contains a dot positioned after the
/// `@` (so `<email>` passes but `a@b` and `a.b@c` fail).
fn looks_like_email(s: &str) -> bool {
    let s = s.trim();
    let at = match s.find('@') {
        Some(i) => i,
        None => return false,
    };
    // exactly one '@'
    if s.rfind('@') != Some(at) {
        return false;
    }
    let local = &s[..at];
    let domain = &s[at + 1..];
    if local.is_empty() || domain.is_empty() {
        return false;
    }
    // domain must contain a dot that is not the first or last character
    match domain.find('.') {
        Some(dot) => dot > 0 && dot < domain.len() - 1,
        None => false,
    }
}

/// google_email_send — send a plain-text email from the configured account.
struct GoogleEmailSend {
    cfg: GoogleConfig,
}

#[async_trait]
impl RustTool for GoogleEmailSend {
    fn name(&self) -> &str {
        "google_email_send"
    }

    fn description(&self) -> &str {
        "Send a plain-text email from the configured Google account over SMTP (STARTTLS). \
         Args: to (recipient email), subject, body."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "to": {
                    "type": "string",
                    "description": "Recipient email address"
                },
                "subject": {
                    "type": "string",
                    "description": "Email subject line"
                },
                "body": {
                    "type": "string",
                    "description": "Plain-text email body"
                }
            },
            "required": ["to", "subject", "body"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let to = args["to"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgument("to is required".into()))?
            .trim();
        let subject = args["subject"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgument("subject is required".into()))?;
        let body = args["body"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgument("body is required".into()))?;

        if !looks_like_email(to) {
            return Err(ToolError::InvalidArgument(format!(
                "'{to}' does not look like a valid email address"
            )));
        }

        let from_mbox: Mailbox = self
            .cfg
            .email
            .parse()
            .map_err(|e| ToolError::InvalidArgument(format!("invalid sender address: {e}")))?;
        let to_mbox: Mailbox = to
            .parse()
            .map_err(|e| ToolError::InvalidArgument(format!("invalid recipient address: {e}")))?;

        let message = Message::builder()
            .from(from_mbox)
            .to(to_mbox)
            .subject(subject)
            .body(body.to_string())
            .map_err(|e| ToolError::InvalidArgument(format!("failed to build message: {e}")))?;

        let creds = Credentials::new(self.cfg.email.clone(), self.cfg.app_password.clone());

        let mailer: AsyncSmtpTransport<Tokio1Executor> =
            AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(SMTP_HOST)
                .map_err(|e| ToolError::Http(format!("SMTP transport setup failed: {e}")))?
                .port(SMTP_PORT)
                .credentials(creds)
                .build();

        mailer
            .send(message)
            .await
            .map_err(|e| ToolError::Http(format!("SMTP send failed: {e}")))?;

        Ok(format!("Email sent to {to} — subject: {subject}"))
    }
}

/// Register: google_email_send.
pub fn register(registry: &mut ToolRegistry, cfg: &GoogleConfig) {
    registry.register_or_replace(Box::new(GoogleEmailSend { cfg: cfg.clone() }));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> GoogleConfig {
        GoogleConfig {
            email: "<email>".into(),
            app_password: "secret".into(),
            peter_email: "<email>".into(),
            lumina_calendar_id: None,
            extra_calendars: vec![],
        }
    }

    #[test]
    fn accepts_valid_emails() {
        assert!(looks_like_email("<email>"));
        assert!(looks_like_email("<email>"));
        assert!(looks_like_email("  <email>  "));
    }

    #[test]
    fn rejects_invalid_emails() {
        assert!(!looks_like_email(""));
        assert!(!looks_like_email("nodomain"));
        assert!(!looks_like_email("missingat.example.com"));
        assert!(!looks_like_email("no@dot")); // no dot in domain
        assert!(!looks_like_email("@example.com")); // empty local
        assert!(!looks_like_email("user@")); // empty domain
        assert!(!looks_like_email("user@.com")); // dot at start of domain
        assert!(!looks_like_email("user@example.")); // dot at end of domain
        assert!(!looks_like_email("two@@example.com")); // double @
        assert!(!looks_like_email("a@<email>")); // two @
    }

    #[test]
    fn tool_metadata_is_correct() {
        let tool = GoogleEmailSend { cfg: cfg() };
        assert_eq!(tool.name(), "google_email_send");
        let params = tool.parameters();
        assert_eq!(params["type"], "object");
        let required = params["required"].as_array().expect("required array");
        assert!(required.iter().any(|v| v == "to"));
        assert!(required.iter().any(|v| v == "subject"));
        assert!(required.iter().any(|v| v == "body"));
    }

    #[test]
    fn message_construction_succeeds() {
        // Build a Message exactly as execute() does — no network involved.
        let from: Mailbox = cfg().email.parse().expect("sender parses");
        let to: Mailbox = "<email>".parse().expect("recipient parses");
        let msg = Message::builder()
            .from(from)
            .to(to)
            .subject("Hello")
            .body("Body text".to_string())
            .expect("message builds");
        let bytes = msg.formatted();
        let raw = String::from_utf8_lossy(&bytes);
        assert!(raw.contains("Subject: Hello"));
        assert!(raw.contains("Body text"));
        assert!(raw.contains("<email>"));
    }

    #[tokio::test]
    async fn execute_rejects_bad_recipient_without_network() {
        let tool = GoogleEmailSend { cfg: cfg() };
        let err = tool
            .execute(json!({"to": "not-an-email", "subject": "s", "body": "b"}))
            .await
            .expect_err("bad recipient must fail before sending");
        match err {
            ToolError::InvalidArgument(_) => {}
            other => panic!("expected InvalidArgument, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn execute_requires_all_args() {
        let tool = GoogleEmailSend { cfg: cfg() };
        for bad in [
            json!({"subject": "s", "body": "b"}),
            json!({"to": "<email>", "body": "b"}),
            json!({"to": "<email>", "subject": "s"}),
        ] {
            let err = tool.execute(bad).await.expect_err("missing arg must fail");
            assert!(matches!(err, ToolError::InvalidArgument(_)));
        }
    }
}
