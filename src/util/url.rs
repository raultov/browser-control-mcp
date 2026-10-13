//! URL handling that must be consistent everywhere: what may be logged.

/// The URL without `user:password@`, for logs and `Debug` output. A value
/// that does not parse as a URL is replaced by a placeholder rather than
/// printed, because it could be anything (even a bare secret).
pub fn without_userinfo(raw: &str) -> String {
    match url::Url::parse(raw) {
        Ok(mut parsed) => {
            // Both setters only fail for URLs that cannot have credentials.
            let _ = parsed.set_username("");
            let _ = parsed.set_password(None);
            parsed.to_string().trim_end_matches('/').to_string()
        }
        Err(_) => "<unparsable url>".to_string(),
    }
}

/// Scheme, host and port only: what the logs may say about a URL the agent
/// navigates to. Paths and queries can carry tokens.
pub fn redacted(raw: &str) -> String {
    match url::Url::parse(raw) {
        Ok(parsed) => match (parsed.host_str(), parsed.port()) {
            (Some(host), Some(port)) => format!("{}://{host}:{port}", parsed.scheme()),
            (Some(host), None) => format!("{}://{host}", parsed.scheme()),
            (None, _) => format!("{}:", parsed.scheme()),
        },
        Err(_) => "<unparsable url>".to_string(),
    }
}

/// Raw protocol methods that navigate, and so must honour `--local`.
const RAW_NAVIGATION_METHODS: &[&str] = &[
    "Page.navigate",
    "Target.createTarget",
    "browsingContext.navigate",
    // Firefox RDP (a target actor's request type).
    "navigateTo",
];

/// The site of a document URL: its registrable domain (eTLD+1 from the
/// Public Suffix List, so `www.knot.kz` and `docs.knot.kz` are one site but
/// `a.github.io` and `b.github.io` are two), the host itself for IP
/// addresses, `localhost` and hosts without a public suffix, and the scheme
/// (`about:`, `data:`, `file:`...) for documents without a host.
pub fn site(raw: &str) -> String {
    let Ok(parsed) = url::Url::parse(raw) else {
        return raw.to_string();
    };
    match parsed.host() {
        Some(url::Host::Domain(host)) => {
            let host = host.trim_end_matches('.').to_ascii_lowercase();
            match psl::domain_str(&host) {
                Some(domain) => domain.to_string(),
                None => host,
            }
        }
        Some(ip) => ip.to_string(),
        None => format!("{}:", parsed.scheme()),
    }
}

/// The URL a raw protocol command would navigate to, if it is one of the
/// known navigation methods. Arbitrary scripts can still navigate: `--local`
/// is a guard rail, not a sandbox (documented in the README).
pub fn raw_navigation_url<'a>(method: &str, params: &'a serde_json::Value) -> Option<&'a str> {
    RAW_NAVIGATION_METHODS
        .contains(&method)
        .then(|| params.get("url").and_then(serde_json::Value::as_str))
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn without_userinfo_strips_credentials_only() {
        assert_eq!(
            without_userinfo("http://u:p@proxy:8080"),
            "http://proxy:8080"
        );
        assert_eq!(without_userinfo("socks5://h:1080"), "socks5://h:1080");
        assert_eq!(without_userinfo("not a url"), "<unparsable url>");
    }

    #[test]
    fn redacted_keeps_scheme_host_and_port() {
        assert_eq!(
            redacted("http://api.example.com/?token=abc"),
            "http://api.example.com"
        );
        assert_eq!(redacted("https://u:p@h:8443/x#y"), "https://h:8443");
        assert_eq!(redacted("about:blank"), "about:");
        assert_eq!(redacted("%%%"), "<unparsable url>");
    }

    /// CON-02 / CON-07: subdomains share a site; public suffixes do not.
    #[test]
    fn site_is_the_registrable_domain() {
        assert_eq!(site("https://www.knot.kz/#/x"), "knot.kz");
        assert_eq!(site("https://docs.knot.kz/a"), "knot.kz");
        assert_eq!(site("https://a.github.io/"), "a.github.io");
        assert_ne!(site("https://a.github.io/"), site("https://b.github.io/"));
        assert_eq!(site("https://shop.example.co.uk/"), "example.co.uk");
        assert_eq!(site("http://127.0.0.1:8000/p"), "127.0.0.1");
        assert_eq!(site("http://localhost:3000/"), "localhost");
        assert_eq!(site("about:blank"), "about:");
        assert_eq!(site("data:text/html,x"), "data:");
    }
}
