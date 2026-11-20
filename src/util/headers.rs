//! HTTP header values that must not be handed to the agent by default.
//!
//! An allowlist: a header's value is shown only when it is a standard
//! header known to carry no credential (content negotiation, caching,
//! CORS, fetch metadata, client hints, security policies, request ids...).
//! Every other header, standard credential or vendor's own, is hidden: a
//! credential hides under any name (`X-WP-Nonce`, `CB-ACCESS-SIGN`,
//! `X-Honeycomb-Team`...), and a list of the sensitive names missed a new
//! one each test round (#144, #149, #161, #168, #175).

/// Headers whose value is shown (compared lowercase, `_` as `-`). Not
/// here on purpose: `Sec-WebSocket-Protocol` (apps send their token in
/// it), `Sec-WebSocket-Key`, `Baggage` (any key-value pairs) and `From`
/// (an e-mail address).
const SHOWN: &[&str] = &[
    // Content negotiation and the body
    "accept",
    "accept-charset",
    "accept-encoding",
    "accept-language",
    "accept-patch",
    "accept-post",
    "accept-ranges",
    "content-disposition",
    "content-encoding",
    "content-language",
    "content-length",
    "content-location",
    "content-range",
    "content-type",
    "range",
    "te",
    "trailer",
    "transfer-encoding",
    "vary",
    // Caching and validation
    "age",
    "cache-control",
    "etag",
    "expires",
    "if-match",
    "if-modified-since",
    "if-none-match",
    "if-range",
    "if-unmodified-since",
    "last-modified",
    "pragma",
    // Navigation and connection
    "allow",
    "alt-svc",
    "alt-used",
    "connection",
    "date",
    "expect",
    "host",
    "keep-alive",
    "link",
    "location",
    "origin",
    "priority",
    "referer",
    "retry-after",
    "server",
    "upgrade",
    "upgrade-insecure-requests",
    "user-agent",
    "via",
    "dnt",
    "save-data",
    "purpose",
    "sec-purpose",
    "service-worker",
    "service-worker-allowed",
    // Security and reporting policies
    "content-security-policy",
    "content-security-policy-report-only",
    "document-policy",
    "nel",
    "origin-agent-cluster",
    "permissions-policy",
    "referrer-policy",
    "report-to",
    "reporting-endpoints",
    "strict-transport-security",
    "timing-allow-origin",
    "x-content-type-options",
    "x-dns-prefetch-control",
    "x-frame-options",
    "x-permitted-cross-domain-policies",
    "x-xss-protection",
    "accept-ch",
    "critical-ch",
    // Challenges: they say how to authenticate, and hold no secret
    "proxy-authenticate",
    "www-authenticate",
    // Request ids, tracing and infrastructure
    "cf-cache-status",
    "cf-ray",
    "forwarded",
    "server-timing",
    "traceparent",
    "tracestate",
    "x-cache",
    "x-correlation-id",
    "x-forwarded-for",
    "x-forwarded-host",
    "x-forwarded-port",
    "x-forwarded-proto",
    "x-powered-by",
    "x-request-id",
    "x-requested-with",
    "x-robots-tag",
    // WebSocket handshake
    "sec-websocket-accept",
    "sec-websocket-extensions",
    "sec-websocket-version",
    // HTTP/2 and HTTP/3 pseudo-headers
    ":authority",
    ":method",
    ":path",
    ":scheme",
    ":status",
];

/// Starts of the names of standard header families whose value is shown:
/// CORS, client hints, fetch metadata, cross-origin isolation and rate
/// limits.
const SHOWN_PREFIXES: &[&str] = &[
    "access-control-",
    "cross-origin-",
    "ratelimit",
    "sec-ch-",
    "sec-fetch-",
    "x-ratelimit-",
];

/// Whether a header's value must be hidden by default: every header but
/// the standard ones in `SHOWN` and `SHOWN_PREFIXES`. `_` counts as
/// `-` (`X-API_KEY`).
pub fn is_sensitive(name: &str) -> bool {
    let name = name.trim().to_ascii_lowercase().replace('_', "-");
    !(SHOWN.contains(&name.as_str()) || SHOWN_PREFIXES.iter().any(|p| name.starts_with(p)))
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
    use super::{SHOWN, SHOWN_PREFIXES, is_sensitive, redact_sensitive};

    /// Credential headers seen in the wild and in earlier reports: the
    /// standard ones, vendor key, token, signature and session headers
    /// (#144, #149, #161, #168, #175, AUD-OB-03), and those a probe of 31
    /// plausible names found shown in clear.
    const CREDENTIALS: &[&str] = &[
        "Authorization",
        "Proxy-Authorization",
        "Cookie",
        "Set-Cookie",
        "X-Api-Key",
        "X-API_KEY",
        "x-goog-api-key",
        "X-CSRF-Token",
        "X-XSRF-TOKEN",
        "X-Session-Id",
        "X-Auth-Key",
        "x_auth_token",
        "Ocp-Apim-Subscription-Key",
        "X-Functions-Key",
        "X-Amz-Credential",
        "X-Amz-Security-Token",
        "X-Authorization",
        "X-Forwarded-Authorization",
        "Authentication",
        "X-Authn",
        "X-Authz-Token",
        "DPoP",
        "X-DPoP-Proof",
        "X-Appwrite-JWT",
        "X-Firebase-AppCheck",
        "X-JWT-Assertion",
        "X-GitHub-OTP",
        "X-Vercel-Protection-Bypass",
        "X-Honeycomb-Team",
        "X-WP-Nonce",
        "X-JFrog-Art-Api",
        "CB-ACCESS-SIGN",
        "CB-ACCESS-KEY",
        "X-BAPI-SIGN",
        "X-NF-Sign",
        "X-Ovh-Application",
        "X-Ovh-Consumer",
        "X-WSSE",
        "X-Amzn-Oidc-Data",
        "X-Amzn-Oidc-Accesstoken",
        "X-Zumo-Application",
        "X-SID",
        "X-Bearer",
        "X-Ms-Client-Principal",
        "X-Userinfo",
        "X-IBM-Client-Id",
        "X-IBM-Client-Secret",
        "X-Shopify-Hmac-Sha256",
        "X-Shopify-Access-Token",
        "X-Access-Code",
        "X-PIN",
        "X-Ticket",
        "X-License",
        "X-Parse-Application-Id",
        "X-Parse-REST-API-Key",
        "X-FB-DTSG",
        "X-Passwd",
        "X-Instagram-Claim",
        "X-Magic",
        "X-Cred",
        "X-Pass",
        "X-Trace-Id",
        "Idempotency-Key",
        "PRIVATE-TOKEN",
        "X-Hub-Signature-256",
        "Stripe-Signature",
        "Sec-WebSocket-Protocol",
        "Sec-WebSocket-Key",
        "Baggage",
        "From",
    ];

    /// The credential headers are hidden, however they are written.
    #[test]
    fn credential_headers_are_hidden() {
        for name in CREDENTIALS {
            assert!(is_sensitive(name), "{name}");
            assert!(is_sensitive(&name.to_ascii_lowercase()), "{name}");
        }
    }

    /// The standard headers listed as shown are shown, in any case and
    /// with `_` for `-`, and so are the families shown by prefix.
    #[test]
    fn standard_headers_are_shown() {
        for name in SHOWN {
            assert!(!is_sensitive(name), "{name}");
            assert!(!is_sensitive(&name.to_ascii_uppercase()), "{name}");
            assert!(!is_sensitive(&name.replace('-', "_")), "{name}");
        }
        for name in [
            "Access-Control-Allow-Origin",
            "Access-Control-Request-Headers",
            "Cross-Origin-Opener-Policy",
            "Sec-CH-UA-Platform",
            "Sec-Fetch-Mode",
            "X-RateLimit-Remaining",
            "RateLimit-Policy",
            " Content-Type ",
        ] {
            assert!(!is_sensitive(name), "{name}");
        }
    }

    /// A name the allowlist does not know is hidden: an app's own header
    /// may carry anything. Names shown before only because no credential
    /// word was in them are hidden too.
    #[test]
    fn unknown_headers_are_hidden() {
        // Given names made of random letters (a fixed sequence)
        let mut seed: u64 = 0x2545_F491_4F6C_DD1D;
        for _ in 0..500 {
            let mut name = String::from("x-");
            for _ in 0..8 {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                name.push(char::from(b'a' + u8::try_from(seed % 26).unwrap_or(0)));
            }
            // Then each is hidden
            assert!(is_sensitive(&name), "{name}");
        }
        for name in [
            "X-Keyboard",
            "X-Team",
            "X-Appwrite-Project",
            "X-Dpopular",
            "X-Hotplug",
            "X-Footprint",
            "X-Honeycomb-Dataset",
            "X-Author-Name",
            "Accept-Token",
            "Content-Secret",
        ] {
            assert!(is_sensitive(name), "{name}");
        }
    }

    /// Guard on the allowlist itself: no name shown, nor any prefix, has
    /// a word that names a credential (a challenge `*-authenticate` and
    /// the `:authority` pseudo-header, the host, aside).
    #[test]
    fn the_allowlist_names_no_credential() {
        let words = [
            "token",
            "secret",
            "password",
            "cookie",
            "session",
            "key",
            "sign",
            "jwt",
            "otp",
            "csrf",
            "xsrf",
            "credential",
            "bypass",
            "auth",
            "nonce",
            "protocol",
        ];
        for name in SHOWN.iter().chain(SHOWN_PREFIXES) {
            let challenge = name.ends_with("-authenticate") || *name == ":authority";
            for word in words {
                assert!(
                    !name.contains(word) || (word == "auth" && challenge),
                    "{name} has {word}"
                );
            }
        }
    }

    /// Only the values of hidden headers are replaced, in an object of
    /// headers; another JSON shape is left alone.
    #[test]
    fn only_hidden_values_are_replaced() {
        let mut headers = serde_json::json!({
            "Content-Type": "text/plain", "X-WP-Nonce": "n-1", "Cookie": "s=1"
        });
        redact_sensitive(&mut headers);
        assert_eq!(headers["Content-Type"], "text/plain");
        assert_eq!(headers["X-WP-Nonce"], crate::cli::REDACTED);
        assert_eq!(headers["Cookie"], crate::cli::REDACTED);
        let mut list = serde_json::json!(["Cookie", "s=1"]);
        redact_sensitive(&mut list);
        assert_eq!(list, serde_json::json!(["Cookie", "s=1"]));
    }
}
