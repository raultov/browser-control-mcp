//! Command line of the server.

use clap::Parser;

/// Command-line arguments of `browser-control-mcp`.
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
pub struct Args {
    /// Restricted to local addresses only (localhost, 127.0.0.1, 192.168.x.x, *.local)
    #[arg(long)]
    pub local: bool,

    /// Enables the 'Chrome is being controlled by automated test software' infobar
    #[arg(long)]
    pub enable_automation: bool,

    /// Use the default user profile (sessions, cookies, etc.) instead of a fresh one.
    /// This starts Chrome without --user-data-dir.
    #[arg(long)]
    pub user_profile: bool,

    /// Allows tools to copy cookies from the user's real Chrome installation into isolated profile instances.
    #[arg(long)]
    pub allow_cookie_import: bool,

    /// Run Chrome in headless mode (no GUI). Required for Docker environments.
    #[arg(long)]
    pub headless: bool,

    /// Target host for Chrome remote debugging (default: 127.0.0.1)
    #[arg(long, default_value = "127.0.0.1")]
    pub host: String,

    /// Chrome remote debugging port
    #[arg(long, default_value_t = 9222)]
    pub port: u16,

    /// Maximum number of concurrent Chrome instances (default: 8)
    #[arg(long, default_value_t = 8)]
    pub max_instances: usize,

    /// Proxy server URL (e.g. `http://proxy.example.com:8080`). When configured, Chrome instances route traffic through this proxy and proxy tools are exposed.
    #[arg(long, alias = "proxy")]
    pub proxy_server: Option<String>,

    /// Proxy authentication username
    #[arg(long)]
    pub proxy_username: Option<String>,

    /// Proxy authentication password
    #[arg(long)]
    pub proxy_password: Option<String>,

    /// Enables WebMCP testing features in Chrome (--enable-features=WebMCPTesting,DevToolsWebMCPSupport) and exposes WebMCP tools.
    #[arg(long, alias = "webmcp")]
    pub enable_webmcp: bool,
}

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
        assert!(!args.local);
        assert!(!args.enable_automation);
        assert!(!args.headless);
        assert!(!args.user_profile);
        assert!(!args.allow_cookie_import);
        assert!(!args.enable_webmcp);
        assert_eq!(args.proxy_server, None);
        assert_eq!(args.proxy_username, None);
        assert_eq!(args.proxy_password, None);
    }

    #[test]
    fn test_args_parsing_allow_cookie_import() {
        let args = Args::parse_from(["browser-control-mcp", "--allow-cookie-import"]);
        assert!(args.allow_cookie_import);
    }

    #[test]
    fn test_args_parsing_enable_automation() {
        let args = Args::parse_from(["browser-control-mcp", "--enable-automation"]);
        assert!(args.enable_automation);
    }

    #[test]
    fn test_args_parsing_user_profile() {
        let args = Args::parse_from(["browser-control-mcp", "--user-profile"]);
        assert!(args.user_profile);
    }

    #[test]
    fn test_args_parsing_local() {
        let args = Args::parse_from(["browser-control-mcp", "--local"]);
        assert!(args.local);
    }

    #[test]
    fn test_args_parsing_custom_port() {
        let args = Args::parse_from(["browser-control-mcp", "--port", "8080"]);
        assert_eq!(args.port, 8080);
    }

    #[test]
    fn test_args_parsing_headless() {
        let args = Args::parse_from(["browser-control-mcp", "--headless"]);
        assert!(args.headless);
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
            args.proxy_server,
            Some("http://proxy.example.com:8080".to_string())
        );
    }

    #[test]
    fn test_args_parsing_enable_webmcp() {
        let args = Args::parse_from(["browser-control-mcp", "--enable-webmcp"]);
        assert!(args.enable_webmcp);

        let args_alias = Args::parse_from(["browser-control-mcp", "--webmcp"]);
        assert!(args_alias.enable_webmcp);
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
            args.proxy_server,
            Some("http://proxy.example.com:8080".to_string())
        );
        assert_eq!(args.proxy_username, Some("user".to_string()));
        assert_eq!(args.proxy_password, Some("pass".to_string()));
    }
}
