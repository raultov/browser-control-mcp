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
    "token",
    "secret",
    "api-key",
    "apikey",
    "password",
    "session",
    "csrf",
    "xsrf",
    "credential",
];

/// Whole words of a header name (between `-`) that name a credential:
/// vendor key headers such as `X-Auth-Key` (Cloudflare) or
/// `Ocp-Apim-Subscription-Key` (Azure) (#144).
const SENSITIVE_WORDS: &[&str] = &["key", "auth"];

/// Whether a header's value must be hidden by default. `_` counts as `-`
/// (`X-API_KEY`).
pub fn is_sensitive(name: &str) -> bool {
    let name = name.to_ascii_lowercase().replace('_', "-");
    SENSITIVE.contains(&name.as_str())
        || SENSITIVE_PARTS.iter().any(|part| name.contains(part))
        || name.split('-').any(|word| SENSITIVE_WORDS.contains(&word))
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

#[cfg(test)]
mod tests {
    use super::is_sensitive;

    /// #144: vendor API key headers, and names written with `_`, are
    /// credentials; challenges and ordinary headers are not.
    #[test]
    fn vendor_key_headers_are_sensitive() {
        for name in [
            "X-Auth-Key",
            "Ocp-Apim-Subscription-Key",
            "X-API_KEY",
            "x_auth_token",
            "X-Functions-Key",
            "X-Amz-Credential",
        ] {
            assert!(is_sensitive(name), "{name}");
        }
        for name in ["WWW-Authenticate", "Content-Type", "X-Keyboard", "Accept"] {
            assert!(!is_sensitive(name), "{name}");
        }
    }
}
