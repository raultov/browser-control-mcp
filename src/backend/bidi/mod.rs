//! Firefox over WebDriver BiDi (via `bidi-browser-lite`), with the Firefox
//! RDP for the JavaScript debugger.
//!
//! One `FirefoxSession` per instance owns its `Browser` (and so
//! its single BiDi session), subscribes once to the events the tools need
//! and routes them by browsing context into per-tab state (the pump).

pub(crate) mod debugger;
pub(crate) mod grip;
pub(crate) mod launch;
pub(crate) mod page;
pub(crate) mod proxy;
pub(crate) mod pump;
pub(crate) mod raw;
pub mod remote_value;
pub(crate) mod session;
pub(crate) mod tools;

use super::BrowserKind;
use super::error::BackendError;

/// A `bidi-browser-lite` error as a tool error. Messages are the library's
/// (they never contain command payloads) plus what the agent can do.
pub(crate) fn lifecycle_error(error: bidi_browser_lite::BrowserError) -> BackendError {
    use bidi_browser_lite::BrowserError as E;
    let hint = match &error {
        E::SessionBusy { .. } => {
            " — that Firefox already has an automation session (another client, or an orphan session): stop that client or restart that Firefox"
        }
        E::ExecutableNotFound { .. } => " — install Firefox or pass --firefox-path",
        _ => "",
    };
    BackendError::Launch(format!("Firefox: {error}{hint}"))
}

/// A BiDi command error as a tool error.
pub(crate) fn protocol_error(method: &str, error: &bidi_browser_lite::BidiError) -> BackendError {
    BackendError::Protocol {
        browser: BrowserKind::Firefox,
        method: method.to_string(),
        message: error.to_string(),
    }
}
