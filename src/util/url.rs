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

/// The proxy URL `raw` (`--proxy-server`, or a tool's proxy) as the
/// browsers take it (#110): `host:port` alone gets `http://`, as browsers'
/// proxy settings read it (Chrome's own flag needs the scheme, and was
/// launched with `--proxy-server=<unparsable url>`). Anything else must be
/// `scheme://host[:port]` with an `http`, `https`, `socks4` or `socks5`
/// scheme (Firefox takes `http` and `socks5` only, and says so at launch),
/// or Chrome's `direct://` (no proxy). The scheme is lower-cased so both
/// browsers get the same value. Credentials are kept (`enable_proxy_auth`
/// reads them); the launchers drop them ([`without_userinfo`]). The error
/// repeats at most the scheme, never the rest: it may hold a password.
pub fn proxy_with_scheme(raw: &str) -> Result<String, String> {
    const SHAPE: &str = "the proxy must look like http://host:port (or https://, socks4://, socks5://; host:port alone means http://)";
    let raw = raw.trim();
    let proxy = match raw.split_once("://") {
        Some((scheme, rest)) => format!("{}://{rest}", scheme.to_ascii_lowercase()),
        None => format!("http://{raw}"),
    };
    if proxy == "direct://" {
        return Ok(proxy);
    }
    let Ok(parsed) = url::Url::parse(&proxy) else {
        return Err(SHAPE.to_string());
    };
    let scheme = parsed.scheme();
    if !matches!(scheme, "http" | "https" | "socks4" | "socks5") {
        return Err(format!("{SHAPE}: the scheme '{scheme}' is not a proxy's"));
    }
    let bare = matches!(parsed.path(), "" | "/") && parsed.query().is_none();
    if parsed.host_str().is_none_or(str::is_empty) || !bare || parsed.fragment().is_some() {
        return Err(SHAPE.to_string());
    }
    Ok(proxy)
}

/// The `(username, password)` written in `raw` (e.g. a proxy URL),
/// percent-decoded; `None` without a username.
pub fn userinfo(raw: &str) -> Option<(String, String)> {
    let parsed = url::Url::parse(raw).ok()?;
    let decode = |part: &str| {
        percent_encoding::percent_decode_str(part)
            .decode_utf8_lossy()
            .into_owned()
    };
    let username = Some(parsed.username()).filter(|u| !u.is_empty())?;
    Some((
        decode(username),
        decode(parsed.password().unwrap_or_default()),
    ))
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

    /// AUD-LC-08: the credentials of a proxy URL, decoded.
    #[test]
    fn userinfo_is_decoded() {
        let found = userinfo("http://al%40ice:s3%3Acr3t@p:3128");
        assert_eq!(found, Some(("al@ice".into(), "s3:cr3t".into())));
        assert_eq!(userinfo("http://p:3128"), None);
        assert_eq!(userinfo("not a url"), None);
    }

    /// PROXY-10 (#110): `host:port` gets `http://` (Chrome launched with
    /// `--proxy-server=<unparsable url>` and every navigation failed);
    /// proxies of another shape are refused without repeating the value.
    #[test]
    fn proxies_get_a_scheme_or_are_refused() {
        for (raw, expected) in [
            ("127.0.0.1:18790", "http://127.0.0.1:18790"),
            ("localhost:8080", "http://localhost:8080"),
            (" proxy.example.com:3128 ", "http://proxy.example.com:3128"),
            ("http://u:p@proxy:8080", "http://u:p@proxy:8080"),
            ("socks5://h:1080", "socks5://h:1080"),
            ("HTTPS://h:443", "https://h:443"),
            ("[::1]:8080", "http://[::1]:8080"),
            ("direct://", "direct://"),
            ("u:s3cr3t@h:8080", "http://u:s3cr3t@h:8080"),
        ] {
            assert_eq!(proxy_with_scheme(raw).as_deref(), Ok(expected), "{raw}");
        }
        for raw in [
            "ftp://h:21",
            "http://",
            "http://h:8080/path",
            "not a proxy s3cr3t",
            "",
            "http://u:s3cr3t@:8080",
        ] {
            let error = proxy_with_scheme(raw).unwrap_err();
            assert!(error.contains("http://host:port"), "{raw}: {error}");
            assert!(!error.contains("s3cr3t"), "{raw}: {error}");
        }
    }

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
