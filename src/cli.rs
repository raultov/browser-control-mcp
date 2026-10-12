//! Command line of the server.

use clap::Parser;

/// Command-line arguments of `browser-control-mcp`.
///
/// Flags are grouped in small structs (`--help` lists them all the same):
/// what the browser looks like, what the server allows, and the proxy.
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
pub struct Args {
    /// How browsers are launched.
    #[command(flatten)]
    pub browser: BrowserFlags,

    /// What the server allows the agent to do.
    #[command(flatten)]
    pub server: ServerFlags,

    /// Target host for Chrome remote debugging (default: 127.0.0.1)
    #[arg(long, default_value = "127.0.0.1")]
    pub host: String,

    /// Chrome remote debugging port
    #[arg(long, default_value_t = 9222)]
    pub port: u16,

    /// Maximum number of concurrent Chrome instances (default: 8)
    #[arg(long, default_value_t = 8)]
    pub max_instances: usize,

    /// Proxy every browser goes through.
    #[command(flatten)]
    pub proxy: ProxyArgs,
}

/// Launch flags shared by every browser instance.
#[derive(clap::Args, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BrowserFlags {
    /// Run Chrome in headless mode (no GUI). Required for Docker environments.
    #[arg(long)]
    pub headless: bool,

    /// Enables the 'Chrome is being controlled by automated test software' infobar
    #[arg(long)]
    pub enable_automation: bool,

    /// Use the default user profile (sessions, cookies, etc.) instead of a fresh one.
    /// This starts Chrome without --user-data-dir.
    #[arg(long)]
    pub user_profile: bool,
}

/// Server-wide permissions and optional tool groups.
#[derive(clap::Args, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ServerFlags {
    /// Restricted to local addresses only (localhost, 127.0.0.1, 192.168.x.x, *.local)
    #[arg(long)]
    pub local: bool,

    /// Allows tools to copy cookies from the user's real Chrome installation into isolated profile instances.
    #[arg(long)]
    pub allow_cookie_import: bool,

    /// Enables WebMCP testing features in Chrome (--enable-features=WebMCPTesting,DevToolsWebMCPSupport) and exposes WebMCP tools.
    #[arg(long, alias = "webmcp")]
    pub enable_webmcp: bool,
}

/// Proxy settings. `Debug` redacts the credentials, including a
/// `user:pass@` embedded in the server URL, so they can be logged.
#[derive(clap::Args, Clone, Default, PartialEq, Eq)]
pub struct ProxyArgs {
    /// Proxy server URL (e.g. `http://proxy.example.com:8080`). When configured, Chrome instances route traffic through this proxy and proxy tools are exposed.
    #[arg(long, alias = "proxy")]
    pub proxy_server: Option<String>,

    /// Proxy authentication username
    #[arg(long)]
    pub proxy_username: Option<String>,

    /// Proxy authentication password
    #[arg(long)]
    pub proxy_password: Option<String>,
}

impl std::fmt::Debug for ProxyArgs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let secret = |v: &Option<String>| v.as_ref().map(|_| REDACTED);
        let server = self
            .proxy_server
            .as_deref()
            .map(crate::util::url::without_userinfo);
        f.debug_struct("ProxyArgs")
            .field("proxy_server", &server)
            .field("proxy_username", &secret(&self.proxy_username))
            .field("proxy_password", &secret(&self.proxy_password))
            .finish()
    }
}

/// What a secret prints as in logs and `Debug` output.
pub const REDACTED: &str = "<redacted>";

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn test_args_parsing_defaults() {
        let args = Args::parse_from(["browser-control-mcp"]);
        assert_eq!(args.port, 9222);
        assert_eq!(args.host, "127.0.0.1");
        assert_eq!(args.max_instances, 8);
        assert_eq!(
            args.browser,
            BrowserFlags::default(),
            "every launch flag off"
        );
        assert_eq!(args.server, ServerFlags::default(), "every server flag off");
        assert_eq!(args.proxy, ProxyArgs::default(), "no proxy");
    }

    #[test]
    fn test_args_parsing_allow_cookie_import() {
        let args = Args::parse_from(["browser-control-mcp", "--allow-cookie-import"]);
        assert!(args.server.allow_cookie_import);
    }

    #[test]
    fn test_args_parsing_enable_automation() {
        let args = Args::parse_from(["browser-control-mcp", "--enable-automation"]);
        assert!(args.browser.enable_automation);
    }

    #[test]
    fn test_args_parsing_user_profile() {
        let args = Args::parse_from(["browser-control-mcp", "--user-profile"]);
        assert!(args.browser.user_profile);
    }

    #[test]
    fn test_args_parsing_local() {
        let args = Args::parse_from(["browser-control-mcp", "--local"]);
        assert!(args.server.local);
    }

    #[test]
    fn test_args_parsing_custom_port() {
        let args = Args::parse_from(["browser-control-mcp", "--port", "8080"]);
        assert_eq!(args.port, 8080);
    }

    #[test]
    fn test_args_parsing_headless() {
        let args = Args::parse_from(["browser-control-mcp", "--headless"]);
        assert!(args.browser.headless);
    }

    #[test]
    fn test_args_parsing_custom_host() {
        let args = Args::parse_from(["browser-control-mcp", "--host", "host.docker.internal"]);
        assert_eq!(args.host, "host.docker.internal");
    }

    #[test]
    fn test_args_parsing_max_instances() {
        let args = Args::parse_from(["browser-control-mcp", "--max-instances", "16"]);
        assert_eq!(args.max_instances, 16);
    }

    #[test]
    fn test_args_parsing_proxy_server() {
        let args = Args::parse_from([
            "browser-control-mcp",
            "--proxy-server",
            "http://proxy.example.com:8080",
        ]);
        assert_eq!(
            args.proxy.proxy_server,
            Some("http://proxy.example.com:8080".to_string())
        );
    }

    #[test]
    fn test_args_parsing_enable_webmcp() {
        let args = Args::parse_from(["browser-control-mcp", "--enable-webmcp"]);
        assert!(args.server.enable_webmcp);

        let args_alias = Args::parse_from(["browser-control-mcp", "--webmcp"]);
        assert!(args_alias.server.enable_webmcp);
    }

    #[test]
    fn test_args_parsing_proxy_credentials() {
        let args = Args::parse_from([
            "browser-control-mcp",
            "--proxy-server",
            "http://proxy.example.com:8080",
            "--proxy-username",
            "user",
            "--proxy-password",
            "pass",
        ]);
        assert_eq!(
            args.proxy.proxy_server,
            Some("http://proxy.example.com:8080".to_string())
        );
        assert_eq!(args.proxy.proxy_username, Some("user".to_string()));
        assert_eq!(args.proxy.proxy_password, Some("pass".to_string()));
    }

    /// SEC-01: Args Debug never prints secrets.
    #[test]
    fn args_debug_redacts_secrets() {
        // Given arguments carrying proxy credentials in three places
        let args = Args::parse_from([
            "browser-control-mcp",
            "--proxy-server",
            "http://u:p@proxy:8080",
            "--proxy-username",
            "alice",
            "--proxy-password",
            "s3cr3t",
        ]);
        // When they are formatted with Debug
        let text = format!("{args:?}");
        // Then no secret is printed, the redaction marker and the proxy host are
        assert!(!text.contains("s3cr3t"), "password leaked: {text}");
        assert!(!text.contains("u:p@"), "userinfo leaked: {text}");
        assert!(!text.contains("alice"), "username leaked: {text}");
        assert!(text.contains(REDACTED));
        assert!(text.contains("http://proxy:8080"), "{text}");
    }
}
