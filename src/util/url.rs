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
}
