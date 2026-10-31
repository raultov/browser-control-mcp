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
pub(crate) mod performance;
pub(crate) mod proxy;
pub(crate) mod pump;
pub(crate) mod raw;
pub mod remote_value;
pub(crate) mod routes;
pub(crate) mod session;
pub(crate) mod storage;
#[cfg(test)]
pub(crate) mod test_support;
pub(crate) mod tools;
pub(crate) mod watch;

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
    let message = match error {
        // A message over the client's 256 MB limit (bidi-lite's
        // MAX_MESSAGE_SIZE) closes the WebSocket; the session reconnects on
        // the next call.
        bidi_browser_lite::BidiError::Disconnected => {
            "the connection to Firefox dropped. A reply over 256 MB (a huge evaluate_js result or page) closes it: keep results small (slice strings, query the DOM). The next call reconnects; tab ids are re-assigned (call list_tabs).".to_string()
        }
        other => other.to_string(),
    };
    BackendError::Protocol {
        browser: BrowserKind::Firefox,
        method: method.to_string(),
        message,
    }
}

#[cfg(test)]
mod lost_connection_tests {
    /// AUD-EV-23 (Firefox): a dropped connection says why and what follows.
    #[test]
    fn lost_connection_is_explained() {
        let error = super::protocol_error(
            "script.evaluate",
            &bidi_browser_lite::BidiError::Disconnected,
        );
        let text = error.to_string();
        assert!(
            text.contains("256 MB") && text.contains("next call reconnects"),
            "{text}"
        );
    }
}
