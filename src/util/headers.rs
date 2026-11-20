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
    // CORS
    "access-control-allow-credentials",
    "access-control-allow-headers",
    "access-control-allow-methods",
    "access-control-allow-origin",
    "access-control-allow-private-network",
    "access-control-expose-headers",
    "access-control-max-age",
    "access-control-request-headers",
    "access-control-request-method",
    "access-control-request-private-network",
    // Cross-origin isolation
    "cross-origin-embedder-policy",
    "cross-origin-embedder-policy-report-only",
    "cross-origin-opener-policy",
    "cross-origin-opener-policy-report-only",
    "cross-origin-resource-policy",
    // Rate limits
    "ratelimit",
    "ratelimit-limit",
    "ratelimit-policy",
    "ratelimit-remaining",
    "ratelimit-reset",
    "x-ratelimit-limit",
    "x-ratelimit-remaining",
    "x-ratelimit-reset",
    "x-ratelimit-resource",
    "x-ratelimit-used",
    // HTTP/2 and HTTP/3 pseudo-headers
    ":authority",
    ":method",
    ":path",
    ":scheme",
    ":status",
];

/// Starts of the names of the header families whose value is shown: client
/// hints and fetch metadata, which only the browser sends (a page cannot
/// set a `Sec-` header), and whose members grow. Other families (CORS,
/// rate limits) are listed name by name in [`SHOWN`]: an app may name its
/// own header `Access-Control-Token`.
const SHOWN_PREFIXES: &[&str] = &["sec-ch-", "sec-fetch-"];

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
            "\tcontent_TYPE\t",
            "ACCESS_CONTROL_ALLOW_ORIGIN",
            ":STATUS",
        ] {
            assert!(!is_sensitive(name), "{name}");
        }
    }

    /// A name that only starts like a shown family, or is near a shown
    /// name, is hidden: an app's own `Access-Control-Token`, a
    /// `Content-Secret`, an `X-Request-Id-Signature`.
    #[test]
    fn names_near_shown_ones_are_hidden() {
        for name in [
            "Access-Control-Token",
            "Access-Control-Api-Key",
            "Cross-Origin-Secret",
            "RateLimit-Key",
            "X-RateLimit-Token",
            "X-Request-Id-Signature",
            "Content-Type-Token",
            "Accept-Token",
            "Etag-Secret",
            "Location-Token",
            "Authority",
            ":authorization",
            "Sec-WebSocket-Protocol",
            "Sec-Token",
            "WWW-Authenticate-Token",
            "Proxy-Authenticate-Key",
        ] {
            assert!(is_sensitive(name), "{name}");
        }
    }

    /// Names no server sends are hidden, without a panic: empty, only
    /// punctuation, outside ASCII, control characters, very long.
    #[test]
    fn odd_names_are_hidden() {
        let long = "x".repeat(100_000);
        for name in [
            "",
            " ",
            ":",
            "-",
            "_",
            "X-\u{166}oken",
            "Content-Type\u{0}",
            "\u{feff}Accept",
            &long,
        ] {
            assert!(is_sensitive(name), "{name:?}");
        }
    }

    /// A hidden header's value is replaced whatever its JSON type (a list
    /// of values, a number, null, an object); names differing only in case
    /// are each handled; an empty object stays empty.
    #[test]
    fn hidden_values_of_any_type_are_replaced() {
        let mut headers = serde_json::json!({
            "Set-Cookie": ["a=1", "b=2"], "X-Count": 3, "X-Null": null, "X-Obj": {"k": "v"},
            "cookie": "c=1", "COOKIE": "d=2", "Vary": ["Accept", "Origin"]
        });
        redact_sensitive(&mut headers);
        for name in [
            "Set-Cookie",
            "X-Count",
            "X-Null",
            "X-Obj",
            "cookie",
            "COOKIE",
        ] {
            assert_eq!(headers[name], crate::cli::REDACTED, "{name}");
        }
        assert_eq!(headers["Vary"], serde_json::json!(["Accept", "Origin"]));
        let mut empty = serde_json::json!({});
        redact_sensitive(&mut empty);
        assert_eq!(empty, serde_json::json!({}));
        for mut other in [
            serde_json::json!("Cookie: a"),
            serde_json::json!(null),
            serde_json::json!(7),
        ] {
            let before = other.clone();
            redact_sensitive(&mut other);
            assert_eq!(other, before);
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
    /// a word that names a credential, but the ones reviewed: the
    /// challenges `*-Authenticate` (how to authenticate), `:authority` (the
    /// host) and `Access-Control-Allow-Credentials` (`true` or nothing).
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
        let reviewed = [
            ("www-authenticate", "auth"),
            ("proxy-authenticate", "auth"),
            (":authority", "auth"),
            ("access-control-allow-credentials", "credential"),
        ];
        for name in SHOWN.iter().chain(SHOWN_PREFIXES) {
            for word in words {
                assert!(
                    !name.contains(word) || reviewed.contains(&(*name, word)),
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
