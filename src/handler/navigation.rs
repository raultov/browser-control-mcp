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

/// Why a navigation to a URL the browser downloads fails (Chrome answered
/// `net::ERR_ABORTED`, Firefox "Navigated to" the previous page).
pub(crate) const DOWNLOAD: &str = "the URL is a download: the browser saves it instead of opening a page, and the tab stays where it was";

/// How long a document's response may take to reach the tab's request log
/// after `load`: the log is fed by an event listener that runs apart from
/// the tool.
const RESPONSE_WAIT: std::time::Duration = std::time::Duration::from_millis(500);

/// The document request of navigation `id` (Chrome's loader id, BiDi's
/// navigation id) once its response is logged, for an http(s) `url` (other
/// schemes have no HTTP status); `None` after [`RESPONSE_WAIT`].
pub(crate) async fn document_response(
    network: &tokio::sync::Mutex<crate::handler::NetworkState>,
    id: Option<&str>,
    url: &str,
) -> Option<crate::handler::NetworkRequest> {
    let id = id.filter(|_| url.starts_with("http://") || url.starts_with("https://"))?;
    let deadline = tokio::time::Instant::now() + RESPONSE_WAIT;
    loop {
        let found = network
            .lock()
            .await
            .document(id)
            .filter(|request| request.response_status.is_some())
            .cloned();
        if found.is_some() || tokio::time::Instant::now() >= deadline {
            return found;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// Waits for the event `is_load` picks on `events` until `deadline` (the
/// navigation's start plus [`LOAD_TIMEOUT`]); `false` when it does not come.
/// A navigation command can outlast the protocol's own command timeout
/// (Chrome answers `Page.navigate` only once the response headers arrive,
/// Firefox's `navigate` waits for `load`), so a timed-out command is not a
/// failed navigation: the page may still load within the budget.
pub(crate) async fn load_before<I>(
    events: impl tokio_stream::Stream<Item = I> + Unpin,
    deadline: tokio::time::Instant,
    is_load: impl Fn(&I) -> bool,
) -> bool {
    use tokio_stream::StreamExt;
    let mut events = events;
    let load = async {
        while let Some(event) = events.next().await {
            if is_load(&event) {
                return true;
            }
        }
        false
    };
    tokio::time::timeout_at(deadline, load)
        .await
        .unwrap_or(false)
}

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
    /// The HTTP status of the document, when the browser reported one.
    pub(crate) status: Option<i64>,
}

impl NavigationOutcome {
    /// The outcome with the document's HTTP `status`; an error status (400
    /// and up) is also a note, since the page is then the server's error.
    pub(crate) fn with_status(mut self, status: Option<i64>) -> Self {
        if let Some(code) = status.filter(|code| *code >= 400) {
            self.notes.push(format!("the server answered HTTP {code}"));
        }
        self.status = status;
        self
    }

    /// The reply object: `{url, tab_id, browser}` plus `status` and `notes`
    /// when known.
    pub(crate) fn to_json(&self, tab_id: Option<&str>) -> Value {
        let mut reply = json!({"url": self.url, "tab_id": tab_id, "browser": self.browser});
        if let Some(status) = self.status {
            reply["status"] = json!(status);
        }
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

    /// AUD-EV-03 / AUD-EV-04: after a navigation command timed out, `load`
    /// within the remaining budget still counts; nothing until the deadline
    /// is `false`.
    #[tokio::test]
    async fn load_counts_until_the_deadline() {
        let soon = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        let events = tokio_stream::iter(["started", "load"]);
        assert!(load_before(events, soon, |e| *e == "load").await);
        let quiet = tokio_stream::pending::<&str>();
        let past = tokio::time::Instant::now() + std::time::Duration::from_millis(20);
        assert!(!load_before(quiet, past, |e| *e == "load").await);
    }

    /// AUD-EV-11: the document's HTTP status is in the reply; an error
    /// status also gets a note.
    #[test]
    fn reply_has_the_document_status() {
        let ok = NavigationOutcome::default().with_status(Some(200));
        assert_eq!(ok.to_json(None)["status"], 200);
        assert!(ok.to_json(None).get("notes").is_none());
        let missing = NavigationOutcome::default().with_status(Some(404));
        assert_eq!(
            missing.to_json(None)["notes"][0],
            "the server answered HTTP 404"
        );
        let unknown = NavigationOutcome::default().with_status(None);
        assert!(unknown.to_json(None).get("status").is_none());
    }

    /// NAV-01: the reply object has the same keys on both browsers.
    #[test]
    fn reply_has_url_tab_and_browser_ids() {
        // Given a finished navigation
        let outcome = NavigationOutcome {
            url: "http://127.0.0.1:8/final".into(),
            browser: json!({"navigation": "n1"}),
            notes: vec![],
            status: None,
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
