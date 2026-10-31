//! Command line of the server.

use clap::Parser;

/// Command-line arguments of `browser-control-mcp`.
///
/// Flags are grouped in small structs (`--help` lists them all the same):
/// what the browser looks like, what the server allows, and the proxy.
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
pub struct Args {
    /// Browser of the default instance; `open_instance` can pick another one per instance.
    #[arg(long = "browser", value_enum, default_value_t = crate::backend::BrowserKind::Chrome)]
    pub browser_kind: crate::backend::BrowserKind,

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

    /// Firefox-only settings.
    #[command(flatten)]
    pub firefox: FirefoxArgs,

    /// Where `upload_file` may read files from.
    #[command(flatten)]
    pub upload: UploadArgs,

    /// Opt-in tool groups.
    #[command(flatten)]
    pub tools: ToolArgs,

    /// Values masked in tool output.
    #[command(flatten)]
    pub secrets: SecretArgs,
}

/// Values the agent should not read back (see `crate::secrets`).
#[derive(clap::Args, Debug, Clone, Default, PartialEq, Eq)]
pub struct SecretArgs {
    /// Dotenv file (NAME=value lines) whose values every tool result and error shows as <secret:NAME>; fill_input and fill_form type the value of a <secret:NAME> they are given. A convenience, not a security boundary.
    #[arg(long = "secrets", value_name = "FILE")]
    pub secrets: Option<std::path::PathBuf>,
}

/// The tool groups listed on top of the default tools: each listed tool
/// costs the agent context on every turn, and these are niche or risky.
#[derive(clap::Args, Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolArgs {
    /// Opt-in tool groups, comma separated: storage, files, raw, perf, emulation, or all.
    #[arg(long = "tool-groups", value_enum, value_delimiter = ',')]
    pub tool_groups: Vec<ToolGroup>,
}

/// A group of tools listed only when `--tool-groups` names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ToolGroup {
    /// `cookies` and `web_storage`: they read and change session tokens.
    Storage,
    /// `upload_file` and `save_pdf`: they read and write files of the
    /// machine.
    Files,
    /// The raw protocol tools and `get_custom_events`: anything goes.
    Raw,
    /// `get_performance_metrics` and `profile_page_performance`.
    Perf,
    /// `emulate`.
    Emulation,
    /// `mock_route`, `list_routes`, `remove_route` and `set_offline`: they
    /// change what the page receives.
    Network,
    /// Every group.
    All,
}

impl ToolGroup {
    /// The name `--tool-groups` takes.
    pub fn name(self) -> &'static str {
        match self {
            Self::Storage => "storage",
            Self::Files => "files",
            Self::Raw => "raw",
            Self::Perf => "perf",
            Self::Emulation => "emulation",
            Self::Network => "network",
            Self::All => "all",
        }
    }
}

/// The directories `upload_file` may read files from: a page given a file
/// gets its contents, so the agent cannot hand it any file of the machine.
#[derive(clap::Args, Debug, Clone, Default, PartialEq, Eq)]
pub struct UploadArgs {
    /// Directory `upload_file` may read files from and `save_pdf` may write PDFs to (repeatable). Default: the server's working directory and the system temp directory.
    #[arg(long = "upload-root")]
    pub upload_roots: Vec<std::path::PathBuf>,
}

/// Firefox-only settings (ignored by Chrome instances).
#[derive(clap::Args, Debug, Clone, Default, PartialEq, Eq)]
pub struct FirefoxArgs {
    /// Firefox binary to launch (default: discovered; also FIREFOX_PATH).
    #[arg(long)]
    pub firefox_path: Option<std::path::PathBuf>,

    /// Attach to a running Firefox's WebDriver BiDi port instead of launching one
    /// (start it with --remote-debugging-port).
    #[arg(long)]
    pub firefox_port: Option<u16>,

    /// Debugger (RDP) port of the attached Firefox (its --start-debugger-server);
    /// without it the debugger tools are unavailable in attach mode.
    #[arg(
        long,
        requires = "firefox_port",
        conflicts_with = "no_firefox_debugger"
    )]
    pub firefox_rdp_port: Option<u16>,

    /// Launch Firefox without its debugger port: no debugger tools, but no local
    /// port that grants chrome privileges either.
    #[arg(long)]
    pub no_firefox_debugger: bool,
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

    /// Window size of the browsers the server launches, WIDTHxHEIGHT (Chrome and
    /// Firefox alike, headless too; an attached browser keeps its window).
    #[arg(long, default_value = "1920x1080", value_name = "WxH", value_parser = WindowSize::parse)]
    pub window_size: WindowSize,
}

/// A browser window size. The same on both browsers, so pages lay out alike
/// (Chrome and Firefox otherwise pick different sizes: 921x648 and 1280x887
/// were measured).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowSize {
    /// Width in CSS pixels.
    pub width: u32,
    /// Height in CSS pixels.
    pub height: u32,
}

impl Default for WindowSize {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
        }
    }
}

impl WindowSize {
    /// Largest side accepted (both browsers cap windows well below it).
    const MAX_SIDE: u32 = 16_384;

    /// Parses `<width>x<height>` (e.g. `1920x1080`), each side 1 to 16384.
    pub fn parse(text: &str) -> Result<Self, String> {
        let invalid =
            || format!("invalid window size '{text}': expected <width>x<height>, e.g. 1920x1080");
        let (width, height) = text.trim().split_once(['x', 'X']).ok_or_else(invalid)?;
        let side = |s: &str| {
            s.trim()
                .parse::<u32>()
                .ok()
                .filter(|n| (1..=Self::MAX_SIDE).contains(n))
                .ok_or_else(invalid)
        };
        Ok(Self {
            width: side(width)?,
            height: side(height)?,
        })
    }
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

    /// IMP-05 / BKD-02: the defaults parse, Chrome is the default browser.
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
        assert_eq!(args.browser_kind, crate::backend::BrowserKind::Chrome);
    }

    /// WIN-03: --window-size parses `<width>x<height>`; anything else is a
    /// clear CLI error; the default is 1920x1080.
    #[test]
    fn window_size_flag_parses() {
        assert_eq!(
            Args::parse_from(["browser-control-mcp"])
                .browser
                .window_size,
            WindowSize {
                width: 1920,
                height: 1080
            }
        );
        let args = Args::parse_from(["browser-control-mcp", "--window-size", "1280x800"]);
        assert_eq!(
            args.browser.window_size,
            WindowSize {
                width: 1280,
                height: 800
            }
        );
        for bad in ["1280", "0x800", "axb", "1280x99999", "1280x"] {
            let error = Args::try_parse_from(["browser-control-mcp", "--window-size", bad])
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("expected <width>x<height>"),
                "{bad}: {error}"
            );
        }
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

    /// CLI-02: --browser accepts only chrome and firefox (CLI-01: default chrome).
    #[test]
    fn browser_flag_accepts_only_known_browsers() {
        use crate::backend::BrowserKind;
        let default = Args::parse_from(["browser-control-mcp"]);
        assert_eq!(default.browser_kind, BrowserKind::Chrome);
        let firefox = Args::parse_from(["browser-control-mcp", "--browser", "firefox"]);
        assert_eq!(firefox.browser_kind, BrowserKind::Firefox);
        let err = Args::try_parse_from(["browser-control-mcp", "--browser", "safari"]).unwrap_err();
        let text = err.to_string();
        assert!(
            text.contains("chrome") && text.contains("firefox"),
            "{text}"
        );
    }

    /// CLI-01: New flags parse with their defaults.
    #[test]
    fn new_flags_have_defaults() {
        let args = Args::parse_from(["browser-control-mcp"]);
        assert_eq!(args.firefox, FirefoxArgs::default());
    }

    /// CLI-03: Firefox attach flags are validated.
    #[test]
    fn firefox_attach_flags_are_validated() {
        let alone = Args::try_parse_from(["browser-control-mcp", "--firefox-rdp-port", "6000"]);
        assert!(alone.is_err(), "--firefox-rdp-port needs --firefox-port");
        let both = Args::try_parse_from([
            "browser-control-mcp",
            "--firefox-port",
            "9222",
            "--firefox-rdp-port",
            "6000",
            "--no-firefox-debugger",
        ]);
        assert!(
            both.is_err(),
            "--no-firefox-debugger conflicts with --firefox-rdp-port"
        );
        let ok = Args::parse_from([
            "browser-control-mcp",
            "--firefox-port",
            "9222",
            "--firefox-rdp-port",
            "6000",
        ]);
        assert_eq!(
            (ok.firefox.firefox_port, ok.firefox.firefox_rdp_port),
            (Some(9222), Some(6000))
        );
    }

    /// DOC-03: Every CLI flag is documented with its per-browser behaviour.
    #[test]
    fn readme_documents_every_flag() {
        use clap::CommandFactory;
        // Given the README flags table
        let readme = include_str!("../README.md");
        let start = readme.find("## CLI flags").expect("CLI flags section");
        let table = &readme[start..];
        let table = &table[..table[3..].find("\n## ").map_or(table.len(), |i| i + 3)];
        // When every long flag of Args is looked up
        for arg in Args::command().get_arguments() {
            let Some(long) = arg.get_long() else { continue };
            if matches!(long, "help" | "version") {
                continue;
            }
            let prefix = format!("| `--{long}");
            let row = table.lines().find(|l| {
                l.strip_prefix(&prefix)
                    .is_some_and(|rest| rest.starts_with(['`', ' ']))
            });
            // Then it has a row with a Chrome and a Firefox cell
            let row = row.unwrap_or_else(|| panic!("--{long} missing from the README"));
            let cells: Vec<&str> = row.split('|').map(str::trim).collect();
            assert!(cells.len() >= 6, "--{long}: {row}");
            assert!(
                !cells[3].is_empty() && !cells[4].is_empty(),
                "--{long}: {row}"
            );
        }
    }
}
