//! HTTP header values that must not be handed to the agent by default.

/// Headers carrying credentials or session state (compared case-insensitively).
const SENSITIVE: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "cookie",
    "set-cookie",
];

/// Replaces the value of every sensitive header in a `{name: value}` object
/// with [`crate::cli::REDACTED`]. Other JSON shapes are left untouched.
pub fn redact_sensitive(headers: &mut serde_json::Value) {
    if let Some(map) = headers.as_object_mut() {
        for (name, value) in map.iter_mut() {
            if SENSITIVE.contains(&name.to_ascii_lowercase().as_str()) {
                *value = serde_json::Value::String(crate::cli::REDACTED.to_string());
            }
        }
    }
}
