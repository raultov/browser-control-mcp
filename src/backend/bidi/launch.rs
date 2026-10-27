//! How a Firefox instance is launched or attached (specs D15–D17).

use crate::cli::{BrowserFlags, FirefoxArgs};
use bidi_browser_lite::{BrowserConfig, LaunchMode, ProfileMode};

/// Everything needed to start (or attach to) one Firefox instance.
#[derive(Debug, Clone, Default)]
pub(crate) struct FirefoxSpec {
    /// Headless / user-profile flags (automation infobar does not exist on Firefox).
    pub(crate) browser: BrowserFlags,
    /// Proxy URL (`http://` or `socks5://`, no credentials).
    pub(crate) proxy: Option<String>,
    /// Firefox-only command-line settings.
    pub(crate) firefox: FirefoxArgs,
}

impl FirefoxSpec {
    /// Whether the debugger (RDP) port will be available.
    pub(crate) fn has_debugger(&self) -> bool {
        match self.firefox.firefox_port {
            Some(_) => self.firefox.firefox_rdp_port.is_some(),
            None => !self.firefox.no_firefox_debugger,
        }
    }

    /// The lifecycle configuration, or why it cannot be built.
    pub(crate) fn config(&self) -> Result<BrowserConfig, String> {
        let size = self.browser.window_size;
        let mut builder = BrowserConfig::builder()
            .headless(self.browser.headless)
            .window_size(size.width, size.height);
        builder = match self.firefox.firefox_port {
            Some(port) => {
                let mut b = builder.mode(LaunchMode::AttachOnly).port(port);
                if let Some(rdp) = self.firefox.firefox_rdp_port {
                    b = b.rdp_port(rdp);
                }
                if self.browser.user_profile {
                    b = b.profile(ProfileMode::UserDefault);
                }
                b
            }
            None if self.browser.user_profile => {
                return Err("--user-profile on Firefox needs --firefox-port: attach to your running Firefox (started with --remote-debugging-port) instead of launching one".into());
            }
            None => builder
                .mode(LaunchMode::LaunchNew)
                .profile(ProfileMode::Ephemeral)
                .port(0)
                .rdp(!self.firefox.no_firefox_debugger),
        };
        // bidi-browser-lite refuses a proxy URL with credentials;
        // enable_proxy_auth reads them from --proxy-server instead.
        if let Some(proxy) = &self.proxy {
            builder = builder.proxy(crate::util::url::without_userinfo(proxy));
        }
        if let Some(path) = &self.firefox.firefox_path {
            builder = builder.firefox_path(path.clone());
        }
        Ok(builder.build())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// BID-01: A launched Firefox gets the agreed configuration.
    #[test]
    fn launched_firefox_config() {
        let config = FirefoxSpec::default().config().unwrap();
        assert_eq!(config.mode(), LaunchMode::LaunchNew);
        assert!(matches!(config.profile(), ProfileMode::Ephemeral));
        assert_eq!(config.port(), 0);
        assert_eq!(
            config.rdp_port(),
            Some(0),
            "rdp enabled with an allocated port"
        );
        assert!(!config.headless());
    }

    /// WIN-02: a launched Firefox gets the configured window size.
    #[test]
    fn firefox_config_carries_window_size() {
        let mut spec = FirefoxSpec::default();
        assert!(
            format!("{:?}", spec.config().unwrap()).contains("window_size: Some((1920, 1080))")
        );
        spec.browser.window_size = crate::cli::WindowSize::parse("1280x800").unwrap();
        assert!(format!("{:?}", spec.config().unwrap()).contains("window_size: Some((1280, 800))"));
    }

    /// BID-02: Attach mode maps the CLI ports.
    #[test]
    fn attach_mode_maps_cli_ports() {
        let mut spec = FirefoxSpec::default();
        spec.firefox.firefox_port = Some(9333);
        spec.firefox.firefox_rdp_port = Some(6001);
        let config = spec.config().unwrap();
        assert_eq!(config.mode(), LaunchMode::AttachOnly);
        assert_eq!((config.port(), config.rdp_port()), (9333, Some(6001)));
        assert!(spec.has_debugger());
    }

    /// CLI-04: Firefox flags reach the Firefox configuration.
    #[test]
    fn firefox_flags_reach_instance_spec() {
        let mut spec = FirefoxSpec {
            proxy: Some("http://p:3128".into()),
            ..Default::default()
        };
        spec.browser.headless = true;
        spec.firefox.firefox_path = Some("/opt/ff".into());
        let config = spec.config().unwrap();
        assert!(config.headless());
        assert_eq!(config.mode(), LaunchMode::LaunchNew);
        assert!(spec.has_debugger());
        let text = format!("{config:?}");
        assert!(
            text.contains("/opt/ff") && text.contains("proxy: Some"),
            "{text}"
        );
    }

    /// AUD-LC-08 (Firefox): a proxy URL with credentials launches (they are
    /// left to enable_proxy_auth; bidi-browser-lite refused the URL).
    #[test]
    fn proxy_url_with_credentials_launches() {
        let spec = FirefoxSpec {
            proxy: Some("http://alice:s3cr3t@127.0.0.1:3128".into()),
            ..Default::default()
        };
        let config = spec.config().unwrap();
        assert!(config.validate().is_ok());
    }

    /// CLI-06: --user-profile with a launched Firefox fails clearly.
    #[test]
    fn user_profile_on_launched_firefox_fails() {
        let mut spec = FirefoxSpec::default();
        spec.browser.user_profile = true;
        let err = spec.config().unwrap_err();
        assert!(err.contains("--firefox-port"), "{err}");
    }
}
