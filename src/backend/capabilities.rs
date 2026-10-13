//! Browsers, capabilities and how they follow from the browser and the
//! server flags.

use super::error::BackendError;
use std::collections::{BTreeMap, BTreeSet};

/// Which browser an instance runs; the protocol follows from it (Chrome:
/// CDP; Firefox: WebDriver BiDi plus the Firefox RDP for the debugger).
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    serde::Serialize,
    serde::Deserialize,
    clap::ValueEnum,
)]
pub enum BrowserKind {
    /// Chrome or Chromium over the Chrome DevTools Protocol.
    #[default]
    #[serde(rename = "chrome")]
    Chrome,
    /// Firefox over WebDriver BiDi (and RDP for JavaScript debugging).
    #[serde(rename = "firefox")]
    Firefox,
}

impl BrowserKind {
    /// The name the agent uses (`"chrome"`, `"firefox"`).
    pub fn name(self) -> &'static str {
        match self {
            Self::Chrome => "chrome",
            Self::Firefox => "firefox",
        }
    }
}

impl std::fmt::Display for BrowserKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Chrome => "Chrome (CDP)",
            Self::Firefox => "Firefox (BiDi)",
        })
    }
}

/// One thing a tool may need from an instance.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    /// Open, close, list and activate tabs.
    Tabs,
    /// Navigate and reload.
    Navigation,
    /// Evaluate JavaScript in the page.
    Evaluate,
    /// Read the DOM and scroll.
    Dom,
    /// Click and type.
    Input,
    /// Capture screenshots.
    Screenshot,
    /// Buffered console messages.
    ConsoleLogs,
    /// Buffered HTTP requests.
    NetworkLogs,
    /// Buffered WebSocket frames.
    WebsocketFrames,
    /// Events of protocol domains/modules enabled by the agent.
    CustomEvents,
    /// Runtime performance metrics.
    PerformanceMetrics,
    /// Performance tracing.
    Tracing,
    /// Breakpoints, pause, frames, stepping, script search.
    JsDebugger,
    /// Pause before the page's first script.
    PauseOnLoad,
    /// Answer proxy authentication challenges.
    ProxyAuth,
    /// Copy the user's cookies into the instance's profile.
    CookieImport,
    /// Launch presets such as `WEBGL_SOFTWARE`.
    LaunchFeatures,
    /// Chrome's WebMCP testing domain.
    Webmcp,
    /// Raw CDP commands.
    RawCdp,
    /// Raw WebDriver BiDi commands.
    RawBidi,
    /// Raw Firefox RDP requests.
    RawRdp,
}

/// The server flags that add or remove capabilities.
#[derive(Clone, Copy, Debug, Default)]
pub struct CapabilityFlags {
    /// `--allow-cookie-import` and `--enable-webmcp` (`--local` is irrelevant).
    pub server: crate::cli::ServerFlags,
    /// `--proxy-server` is set.
    pub proxy: bool,
    /// Firefox: the debugger (RDP) port is available.
    pub firefox_debugger: bool,
}

/// What one instance can do, with the reason for everything it cannot.
#[derive(Clone, Debug, Default)]
pub struct Capabilities {
    browser: BrowserKind,
    set: BTreeSet<Capability>,
    missing: BTreeMap<Capability, &'static str>,
}

const COMMON: &[Capability] = &[
    Capability::Tabs,
    Capability::Navigation,
    Capability::Evaluate,
    Capability::Dom,
    Capability::Input,
    Capability::Screenshot,
    Capability::ConsoleLogs,
    Capability::NetworkLogs,
    Capability::CustomEvents,
];

impl Capabilities {
    /// The capabilities of an instance of `browser` under `flags`.
    pub fn new(browser: BrowserKind, flags: CapabilityFlags) -> Self {
        let mut caps = Self {
            browser,
            ..Self::default()
        };
        for &c in COMMON {
            caps.grant(c);
        }
        caps.flag(
            Capability::ProxyAuth,
            flags.proxy,
            "the server was started without --proxy-server",
        );
        match browser {
            BrowserKind::Chrome => caps.chrome(flags),
            BrowserKind::Firefox => caps.firefox(flags),
        }
        caps
    }

    fn chrome(&mut self, flags: CapabilityFlags) {
        for c in [
            Capability::WebsocketFrames,
            Capability::PerformanceMetrics,
            Capability::Tracing,
            Capability::JsDebugger,
            Capability::PauseOnLoad,
            Capability::LaunchFeatures,
            Capability::RawCdp,
        ] {
            self.grant(c);
        }
        let no_cookie = "the server was started without --allow-cookie-import";
        self.flag(
            Capability::CookieImport,
            flags.server.allow_cookie_import,
            no_cookie,
        );
        let no_webmcp = "the server was started without --enable-webmcp";
        self.flag(Capability::Webmcp, flags.server.enable_webmcp, no_webmcp);
        let firefox_only = "it is a Firefox protocol";
        self.deny(Capability::RawBidi, firefox_only);
        self.deny(Capability::RawRdp, firefox_only);
    }

    fn firefox(&mut self, flags: CapabilityFlags) {
        self.grant(Capability::RawBidi);
        let no_rdp = "the Firefox debugger port is not available (--no-firefox-debugger, or an attached Firefox without --firefox-rdp-port)";
        for c in [
            Capability::JsDebugger,
            Capability::RawRdp,
            Capability::WebsocketFrames,
        ] {
            self.flag(c, flags.firefox_debugger, no_rdp);
        }
        let no_pause = "neither a preload `debugger;` nor an RDP interrupt pauses a reloading page on Firefox (not implemented yet)";
        self.deny(Capability::PauseOnLoad, no_pause);
        self.deny(
            Capability::PerformanceMetrics,
            "Firefox has no equivalent of CDP's Performance domain",
        );
        self.deny(
            Capability::Tracing,
            "Firefox has no equivalent of CDP's Tracing domain",
        );
        self.deny(Capability::Webmcp, "WebMCP is a Chrome testing feature");
        self.deny(
            Capability::CookieImport,
            "cookie import is not implemented for Firefox profiles",
        );
        self.deny(
            Capability::LaunchFeatures,
            "launch presets are Chrome command-line switches",
        );
        self.deny(Capability::RawCdp, "it is the Chrome protocol");
    }

    fn grant(&mut self, c: Capability) {
        self.missing.remove(&c);
        self.set.insert(c);
    }

    fn deny(&mut self, c: Capability, reason: &'static str) {
        self.set.remove(&c);
        self.missing.insert(c, reason);
    }

    fn flag(&mut self, c: Capability, on: bool, reason_if_off: &'static str) {
        if on {
            self.grant(c);
        } else {
            self.deny(c, reason_if_off);
        }
    }

    /// The browser these capabilities belong to.
    pub fn browser(&self) -> BrowserKind {
        self.browser
    }

    /// Whether `c` is available.
    pub fn has(&self, c: Capability) -> bool {
        self.set.contains(&c)
    }

    /// `Ok` if every capability `tool` needs is available, else the first
    /// missing one as [`BackendError::Unsupported`].
    pub fn require(&self, tool: &str, needs: &[Capability]) -> Result<(), BackendError> {
        match needs.iter().find(|c| !self.has(**c)) {
            None => Ok(()),
            Some(&capability) => Err(BackendError::Unsupported {
                tool: tool.to_string(),
                capability,
                browser: self.browser,
                reason: self
                    .missing
                    .get(&capability)
                    .copied()
                    .unwrap_or("not supported"),
            }),
        }
    }

    /// The available capabilities, sorted.
    pub fn iter(&self) -> impl Iterator<Item = Capability> + '_ {
        self.set.iter().copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Capability::*;

    /// `(proxy, webmcp)` flags with cookie import off and the debugger port on/off.
    fn flags((proxy, webmcp): (bool, bool), debugger: bool) -> CapabilityFlags {
        CapabilityFlags {
            server: crate::cli::ServerFlags {
                enable_webmcp: webmcp,
                ..Default::default()
            },
            proxy,
            firefox_debugger: debugger,
        }
    }

    /// CAP-01: Capabilities follow the browser and the flags.
    #[test]
    fn capabilities_follow_browser_and_flags() {
        let rows = [
            (
                BrowserKind::Chrome,
                flags((false, false), true),
                vec![RawCdp, Tracing],
                vec![RawBidi, Webmcp, ProxyAuth, CookieImport],
            ),
            (
                BrowserKind::Chrome,
                flags((true, true), true),
                vec![Webmcp, ProxyAuth],
                vec![RawRdp],
            ),
            (
                BrowserKind::Firefox,
                flags((false, false), true),
                vec![RawBidi, JsDebugger, RawRdp, WebsocketFrames],
                vec![
                    Tracing,
                    PerformanceMetrics,
                    Webmcp,
                    CookieImport,
                    LaunchFeatures,
                ],
            ),
            (
                BrowserKind::Firefox,
                flags((false, false), false),
                vec![RawBidi],
                vec![JsDebugger, RawRdp, WebsocketFrames],
            ),
        ];
        for (browser, flags, has, lacks) in rows {
            // Given an instance of <browser> with <flags>
            let caps = Capabilities::new(browser, flags);
            // Then it has <has> and lacks <lacks>
            for c in has {
                assert!(caps.has(c), "{browser} {flags:?} must have {c:?}");
            }
            for c in lacks {
                assert!(!caps.has(c), "{browser} {flags:?} must lack {c:?}");
            }
        }
    }

    /// CAP-04: A capability error explains the reason and the alternative.
    #[test]
    fn capability_error_explains_reason() {
        // Given a Firefox instance without its debugger port
        let caps = Capabilities::new(BrowserKind::Firefox, CapabilityFlags::default());
        // When a debugger tool is checked
        let err = caps.require("set_breakpoint", &[JsDebugger]).unwrap_err();
        // Then the error names the tool, the browser and the flag
        let text = err.to_string();
        assert!(
            text.contains("`set_breakpoint` is not available on Firefox (BiDi)"),
            "{text}"
        );
        assert!(text.contains("--no-firefox-debugger"), "{text}");
    }
}
