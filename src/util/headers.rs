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
/// case-insensitively, plus the bearer tokens of web SDKs (`X-Appwrite-JWT`,
/// `X-Firebase-AppCheck`; #168). Challenges such as `WWW-Authenticate` hold
/// no secret and stay readable.
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
    "jwt",
    "appcheck",
];

/// Whole words of a header name (between `-`) that name a credential:
/// vendor key headers such as `X-Auth-Key` (Cloudflare) or
/// `Ocp-Apim-Subscription-Key` (Azure) (#144), and the `DPoP` proof of
/// possession (RFC 9449), a signed JWT sent along an access token (#161).
const SENSITIVE_WORDS: &[&str] = &["key", "dpop"];

/// Starts of the words that name authorization or authentication.
const AUTH_PREFIXES: &[&str] = &["authoriz", "authentic", "authn", "authz"];

/// Whether a word of a header name names credentials: `auth` and the
/// authorization and authentication words (`X-Authorization`,
/// `X-Forwarded-Authorization`, `Authentication`, `X-Authn`; #149), but
/// not the challenge `Authenticate` (`WWW-Authenticate`), which holds no
/// secret.
fn is_auth_word(word: &str) -> bool {
    word == "auth" || (word != "authenticate" && AUTH_PREFIXES.iter().any(|p| word.starts_with(p)))
}

/// Whether a header's value must be hidden by default. `_` counts as `-`
/// (`X-API_KEY`).
pub fn is_sensitive(name: &str) -> bool {
    let name = name.to_ascii_lowercase().replace('_', "-");
    SENSITIVE.contains(&name.as_str())
        || SENSITIVE_PARTS.iter().any(|part| name.contains(part))
        || name
            .split('-')
            .any(|word| SENSITIVE_WORDS.contains(&word) || is_auth_word(word))
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

    /// #149: headers whose words start with `auth` carry credentials
    /// (proxied and non-standard authorization headers); the
    /// `Authenticate` challenges do not.
    #[test]
    fn authorization_variants_are_sensitive() {
        for name in [
            "X-Authorization",
            "X-Forwarded-Authorization",
            "Authentication",
            "X-Authn",
            "X-Authz-Token",
            "X_AUTHORIZATION",
        ] {
            assert!(is_sensitive(name), "{name}");
        }
        for name in ["WWW-Authenticate", "Proxy-Authenticate", "X-Author-Name"] {
            assert!(!is_sensitive(name), "{name}");
        }
    }

    /// #161: the `DPoP` proof (RFC 9449) is a credential like
    /// `Authorization`, under any case and in vendor variants.
    #[test]
    fn dpop_proof_is_sensitive() {
        for name in ["DPoP", "dpop", "X-DPoP-Proof", "DPOP_NONCE"] {
            assert!(is_sensitive(name), "{name}");
        }
        assert!(!is_sensitive("X-Dpopular"));
    }

    /// #168: the Appwrite session JWT and the Firebase App Check token are
    /// bearer credentials sent by web SDKs.
    #[test]
    fn jwt_and_app_check_headers_are_sensitive() {
        for name in [
            "X-Appwrite-JWT",
            "X-Firebase-AppCheck",
            "X-JWT-Assertion",
            "x_firebase_appcheck",
        ] {
            assert!(is_sensitive(name), "{name}");
        }
        assert!(!is_sensitive("X-Appwrite-Project"));
    }
}
