//! HTTP header values that must not be handed to the agent by default.

/// Headers carrying credentials or session state (compared case-insensitively).
const SENSITIVE: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "cookie",
    "set-cookie",
];

/// Name fragments of the many non-standard credential headers (`X-Api-Key`,
/// `X-CSRF-Token`, `X-Amz-Security-Token`, `X-Session-Id`...), compared
/// case-insensitively. Challenges such as `WWW-Authenticate` hold no secret
/// and stay readable.
const SENSITIVE_PARTS: &[&str] = &[
    "token", "secret", "api-key", "apikey", "password", "session", "csrf", "xsrf",
];

/// Whether a header's value must be hidden by default.
pub fn is_sensitive(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    SENSITIVE.contains(&name.as_str()) || SENSITIVE_PARTS.iter().any(|part| name.contains(part))
}

/// Replaces the value of every sensitive header in a `{name: value}` object
/// with [`crate::cli::REDACTED`]. Other JSON shapes are left untouched.
pub fn redact_sensitive(headers: &mut serde_json::Value) {
    if let Some(map) = headers.as_object_mut() {
        for (name, value) in map.iter_mut() {
            if is_sensitive(name) {
                *value = serde_json::Value::String(crate::cli::REDACTED.to_string());
            }
        }
    }
}
