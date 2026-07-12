//! CONST-02: secret-masking for the constellation aggregation layer.
//!
//! Every `/api/*` response body — whether from a local handler or a proxied
//! backend (`crate::constellation::proxy`) — is walked through
//! [`mask_response`] before it leaves this process. This is the load-bearing
//! security property of the aggregation layer: a browser client only ever
//! reaches Harmony/Chord/Lumina THROUGH this door, so this is the one place
//! a leaked credential in an upstream's JSON response could otherwise reach
//! the operator's browser unredacted.
//!
//! ## Fail-closed masking
//! [`mask_response`] recurses through the whole JSON value (objects, arrays,
//! nested structures) and replaces any STRING value that is secret-shaped —
//! by its own content (a bearer/JWT/provider-key-prefixed pattern) OR by the
//! key name that carries it (`token`, `key`, `password`, `secret`,
//! `credential`, `auth`, case-insensitive, mirroring
//! `crate::gateway_framework::audit`'s own secret-key-name heuristic) — with
//! a masked placeholder. An unknown value that merely LOOKS secret-shaped is
//! masked too (fail-closed): this function has no way to prove a value is
//! safe, only to detect values that are unsafe, so ambiguous cases mask.
//!
//! When a masked field's KEY NAME maps to a known vault-managed credential
//! (the `<PROVIDER>_TOKEN`/`<PROVIDER>_PAT_<NAME>`/`*_API_KEY` conventions
//! this crate already uses — see `crate::config`'s module doc), the
//! placeholder takes the vault-key-reference form `"<vault:KEY_NAME>"` so an
//! operator reading the masked response can see WHICH credential family was
//! redacted without ever seeing its value. An unrecognized key/shape masks
//! to the generic `"***masked***"`.

use serde_json::Value;

const GENERIC_MASK: &str = "***masked***";

/// Recursively mask every secret-shaped string value in `body`, returning a
/// new [`Value`]. Non-string leaves (numbers, bools, null) are never
/// touched — only strings can carry a credential.
pub fn mask_response(body: Value) -> Value {
    mask_value(None, body)
}

fn mask_value(key: Option<&str>, value: Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(k, v)| {
                    let masked = mask_value(Some(&k), v);
                    (k, masked)
                })
                .collect(),
        ),
        Value::Array(items) => {
            Value::Array(items.into_iter().map(|v| mask_value(key, v)).collect())
        }
        Value::String(s) => {
            if key.map(key_looks_secret_shaped).unwrap_or(false) || value_looks_secret_shaped(&s) {
                Value::String(mask_placeholder(key))
            } else {
                Value::String(s)
            }
        }
        other => other,
    }
}

/// The masked placeholder for a given key name: a vault-key-reference form
/// for a recognized credential-family key name, else the generic mask.
fn mask_placeholder(key: Option<&str>) -> String {
    match key.and_then(vault_key_reference) {
        Some(vault_key) => format!("<vault:{vault_key}>"),
        None => GENERIC_MASK.to_string(),
    }
}

/// Map a JSON key name to the vault/env KEY NAME (never a value) that would
/// carry it in this fleet's runtime secret store, per this crate's existing
/// `<PROVIDER>_TOKEN` / `<PROVIDER>_PAT_<NAME>` / `*_API_KEY` naming
/// conventions (see `crate::gitea`/`crate::plane`/`crate::github` module
/// docs). Best-effort: an unrecognized key returns `None` and the generic
/// mask is used instead — this is a readability aid for the operator, never
/// a completeness guarantee (masking itself does not depend on this
/// succeeding).
fn vault_key_reference(key: &str) -> Option<&'static str> {
    let lower = key.to_ascii_lowercase();
    match lower.as_str() {
        "gitea_token" | "gitea_pat" => Some("GITEA_TOKEN"),
        "github_token" | "github_pat" => Some("GITHUB_TOKEN"),
        "plane_api_key" | "plane_pat" => Some("PLANE_API_KEY"),
        "openrouter_api_key" => Some("OPENROUTER_API_KEY"),
        "review_daemon_token" => Some("REVIEW_DAEMON_TOKEN"),
        "infisical_client_secret" => Some("INFISICAL_CLIENT_SECRET"),
        _ if lower.contains("token") => None,
        _ => None,
    }
}

/// Does this JSON key name look like it's meant to carry a secret?
/// Mirrors `crate::gateway_framework::audit::secret_kv_re`'s key vocabulary
/// (token/key/secret/password/credential/auth), applied to a JSON key
/// instead of a free-text log line.
fn key_looks_secret_shaped(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    const MARKERS: [&str; 6] = ["token", "key", "secret", "password", "credential", "auth"];
    // "key" alone is a common, legitimately-non-secret JSON field name
    // (e.g. a map "key"/"value" pair, a UI list `key` prop) — require a
    // more specific marker OR a compound name (`api_key`, `access_key`) to
    // avoid over-masking ordinary data. `token`/`secret`/`password`/
    // `credential`/`auth*` are unambiguous enough to match standalone.
    if lower == "key" {
        return false;
    }
    MARKERS.iter().any(|m| lower.contains(m))
}

/// Does this STRING VALUE itself look like a secret, independent of its key
/// name? Covers the common provider-token prefixes and shapes this fleet's
/// own PII gate already watches for (`sk-`, `ghp_`, `gsk_`, `glpat-`), a
/// three-segment JWT, and a `Bearer <token>` value.
fn value_looks_secret_shaped(s: &str) -> bool {
    const PREFIXES: [&str; 6] = ["sk-", "ghp_", "gsk_", "glpat-", "gho_", "ghs_"];
    if PREFIXES.iter().any(|p| s.starts_with(p)) {
        return true;
    }
    if s.starts_with("Bearer ") && s.len() > 12 {
        return true;
    }
    // A JWT: three dot-separated base64url segments, each non-trivially
    // long (rules out e.g. version strings like "1.2.3").
    let segments: Vec<&str> = s.split('.').collect();
    if segments.len() == 3
        && segments
            .iter()
            .all(|seg| seg.len() >= 10 && seg.chars().all(is_base64url_char))
    {
        return true;
    }
    false
}

fn is_base64url_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '='
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn masks_field_by_secret_key_name() {
        let body = json!({"gitea_token": "abc123reallysecretvalue", "name": "ok"});
        let masked = mask_response(body);
        assert_eq!(masked["gitea_token"], "<vault:GITEA_TOKEN>");
        assert_eq!(masked["name"], "ok");
    }

    #[test]
    fn masks_value_by_secret_shape_even_with_innocuous_key() {
        let body = json!({"note": "<REDACTED-SECRET>"}); // pii-test-fixture
        let masked = mask_response(body);
        assert_eq!(masked["note"], "***masked***");
    }

    #[test]
    fn masks_bearer_token_value() {
        let body = json!({"header_echo": "Bearer abcdefghijklmno"});
        let masked = mask_response(body);
        assert_eq!(masked["header_echo"], "***masked***");
    }

    #[test]
    fn masks_jwt_shaped_value() {
        let jwt = "<REDACTED-SECRET>"; // pii-test-fixture
        let body = json!({"session": jwt});
        let masked = mask_response(body);
        assert_eq!(masked["session"], "***masked***");
    }

    #[test]
    fn recurses_into_nested_objects_and_arrays() {
        let body = json!({
            "outer": {
                "list": [
                    {"github_token": "<REDACTED-SECRET>"}, // pii-test-fixture
                    {"safe": "value"}
                ]
            }
        });
        let masked = mask_response(body);
        assert_eq!(masked["outer"]["list"][0]["github_token"], "<vault:GITHUB_TOKEN>");
        assert_eq!(masked["outer"]["list"][1]["safe"], "value");
    }

    /// Negative property test: plant several distinct secret shapes/keys
    /// across a realistic nested payload and assert NONE of the raw secret
    /// values survive masking anywhere in the serialized output.
    #[test]
    fn negative_property_no_planted_secret_survives_in_serialized_output() {
        let planted_secrets = [
            "<REDACTED-SECRET>",       // pii-test-fixture
            "<REDACTED-SECRET>",   // pii-test-fixture
            "<REDACTED-SECRET>",          // pii-test-fixture
        ];
        let body = json!({
            "system": "harmony",
            "config": {
                "gitea_token": planted_secrets[0],
                "nested": {"api_key_value": planted_secrets[1]},
            },
            "providers": [
                {"name": "openrouter", "openrouter_api_key": planted_secrets[2]},
            ],
            "unrelated_field": "totally fine, not a secret",
        });
        let masked = mask_response(body);
        let serialized = serde_json::to_string(&masked).unwrap();
        for secret in planted_secrets {
            assert!(
                !serialized.contains(secret),
                "planted secret leaked through masking: {secret}"
            );
        }
        assert!(serialized.contains("totally fine, not a secret"));
    }

    #[test]
    fn does_not_mask_ordinary_key_named_key() {
        // "key" alone (e.g. a list-item React-style `key` prop, or a map
        // entry's `key`/`value` pair) is common, legitimate, non-secret
        // data and must not be over-masked.
        let body = json!({"key": "row-42", "value": "some ordinary content"});
        let masked = mask_response(body);
        assert_eq!(masked["key"], "row-42");
        assert_eq!(masked["value"], "some ordinary content");
    }

    #[test]
    fn leaves_non_string_leaves_untouched() {
        let body = json!({"count": 42, "enabled": true, "ratio": 0.5, "nothing": null});
        let masked = mask_response(body.clone());
        assert_eq!(masked, body);
    }
}
