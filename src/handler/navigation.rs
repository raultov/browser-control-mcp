//! The reply contract of `navigate`, `reload` and `open_tab`, shared by both
//! browsers: the agent gets the final URL (after redirects) and the tab id in
//! the same shape whatever the protocol, and a network error is a tool error
//! with the same wording on Chrome and Firefox
//! ([`crate::backend::BackendError::Navigation`]).

use rust_mcp_sdk::schema::CallToolResult;
use serde_json::{Value, json};

/// How long a navigation may take to fire `load` before the tool answers
/// anyway (with a note): a page that never finishes loading must not hang
/// the agent.
pub(crate) const LOAD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// The note a reply carries when `load` did not fire within [`LOAD_TIMEOUT`].
pub(crate) const NOT_LOADED_NOTE: &str =
    "the page had not fired `load` after 30 s; the URL is the one committed so far";

/// What a finished navigation reports.
#[derive(Debug, Default)]
pub(crate) struct NavigationOutcome {
    /// The committed URL of the top-level document, after redirects.
    pub(crate) url: String,
    /// Protocol ids of the navigation (CDP `frameId`/`loaderId`, BiDi
    /// `navigation`), for agents that correlate them with raw events.
    pub(crate) browser: Value,
    /// Whatever the agent should know (e.g. `load` did not fire in time).
    pub(crate) notes: Vec<String>,
}

impl NavigationOutcome {
    /// The reply object: `{url, tab_id, browser}` plus `notes` when any.
    pub(crate) fn to_json(&self, tab_id: Option<&str>) -> Value {
        let mut reply = json!({"url": self.url, "tab_id": tab_id, "browser": self.browser});
        if !self.notes.is_empty() {
            reply["notes"] = json!(self.notes);
        }
        reply
    }

    /// The tool reply: `first_line`, then the reply object as pretty JSON.
    pub(crate) fn reply(&self, first_line: &str, tab_id: Option<&str>) -> CallToolResult {
        let body = serde_json::to_string_pretty(&self.to_json(tab_id)).unwrap_or_default();
        CallToolResult::text_content(vec![format!("{first_line}\n{body}").into()])
    }
}

/// A navigation error ([`crate::backend::BackendError::Navigation`]) of the
/// navigation `open_tab` started: the tab was
/// created and stays open, so the agent can inspect or close it.
pub(crate) fn open_tab_navigation_failed(message: &str, tab_id: &str) -> String {
    format!("{message}. The tab stays open as {tab_id} (close it with close_tab).")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// NAV-01: the reply object has the same keys on both browsers.
    #[test]
    fn reply_has_url_tab_and_browser_ids() {
        // Given a finished navigation
        let outcome = NavigationOutcome {
            url: "http://127.0.0.1:8/final".into(),
            browser: json!({"navigation": "n1"}),
            notes: vec![],
        };
        // When it becomes a reply
        let reply = outcome.to_json(Some("tab-1"));
        // Then it carries the final URL, the tab id and the protocol ids only
        assert_eq!(
            reply,
            json!({"url": "http://127.0.0.1:8/final", "tab_id": "tab-1", "browser": {"navigation": "n1"}})
        );
        // And notes appear only when there are some
        let late = NavigationOutcome {
            notes: vec![NOT_LOADED_NOTE.into()],
            ..outcome
        };
        assert_eq!(late.to_json(None)["notes"][0], NOT_LOADED_NOTE);
    }
}
