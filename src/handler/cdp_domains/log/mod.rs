pub mod get_console_logs;

use crate::handler::MAX_CONSOLE_MESSAGES;
use crate::util::json::{opt_string, str_field, string_or_empty};
use cdp_browser_lite::WsResponse;
use serde_json::Value;
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Clone, Debug, ::serde::Serialize, ::serde::Deserialize)]
pub struct ConsoleMessage {
    pub source: String,
    pub level: String,
    pub text: String,
    pub timestamp: f64,
    pub url: Option<String>,
    pub line_number: Option<i64>,
    /// Which feed reported it (for de-duplication; not part of the reply).
    #[serde(skip)]
    pub(crate) channel: Channel,
}

/// The feed a console message came from. Firefox reports some messages
/// on both: BiDi `log.entryAdded` and the RDP watcher.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Channel {
    /// The browser's main protocol (CDP, or BiDi on Firefox).
    #[default]
    Primary,
    /// Firefox's RDP watcher resources.
    Watcher,
}

/// How close (ms) two copies of one message from different feeds arrive.
const DUPLICATE_WINDOW_MS: f64 = 2000.0;

/// How many recent messages are compared for duplicates.
const DUPLICATE_LOOKBACK: usize = 64;

#[derive(Default)]
pub(crate) struct LogState {
    pub messages: VecDeque<ConsoleMessage>,
    /// Site of the tab's current document (`util::url::site`), once known.
    site: Option<String>,
    /// Says what the last cross-site navigation cleared, until it is read.
    cleared_note: Option<String>,
}

impl LogState {
    pub(crate) fn push_message(&mut self, msg: ConsoleMessage) {
        self.messages.push_back(msg);
        if self.messages.len() > MAX_CONSOLE_MESSAGES {
            self.messages.pop_front();
        }
    }

    /// Pushes a message that another feed may report too (Firefox): a copy
    /// already pushed by the other feed (same text, close timestamps) is
    /// kept once, with the watcher's labels, which say more.
    pub(crate) fn push_deduplicated(&mut self, msg: ConsoleMessage) {
        let twin = self
            .messages
            .iter_mut()
            .rev()
            .take(DUPLICATE_LOOKBACK)
            .find(|m| {
                m.channel != msg.channel
                    && m.text == msg.text
                    && (m.timestamp - msg.timestamp).abs() <= DUPLICATE_WINDOW_MS
            });
        match twin {
            Some(existing) if msg.channel == Channel::Watcher => *existing = msg,
            Some(_) => {}
            None => self.push_message(msg),
        }
    }

    /// A new top-level document committed at `url`. Moving to another site
    /// clears the buffer (the old site's messages would bury the new
    /// page's); same-site navigations and reloads keep it. Same-document
    /// navigations (fragment, history API) never get here.
    pub(crate) fn on_document_committed(&mut self, url: &str) {
        let site = crate::util::url::site(url);
        if let Some(previous) = self.site.as_deref()
            && previous != site
            && !self.messages.is_empty()
        {
            self.cleared_note = Some(format!(
                "{} console messages from {previous} were cleared when the tab moved to {site}",
                self.messages.len()
            ));
            self.messages.clear();
        }
        self.site = Some(site);
    }

    /// The note of the last cross-site clear, once.
    pub(crate) fn take_cleared_note(&mut self) -> Option<String> {
        self.cleared_note.take()
    }
}

pub(crate) async fn process_log_event(event: &WsResponse, state: &Arc<Mutex<LogState>>) {
    let (Some(method), Some(params)) = (event.method.as_deref(), &event.params) else {
        return;
    };
    if let Some(url) = main_frame_commit(method, params) {
        state.lock().await.on_document_committed(url);
    } else if let Some(message) = console_message(method, params) {
        state.lock().await.push_message(message);
    }
}

/// The URL of a new top-level document (`Page.frameNavigated` of the main
/// frame, which Chrome sends at commit, before the page's scripts run).
fn main_frame_commit<'a>(method: &str, params: &'a Value) -> Option<&'a str> {
    let frame = params
        .get("frame")
        .filter(|_| method == "Page.frameNavigated")?;
    if frame.get("parentId").is_some() {
        return None;
    }
    frame.get("url").and_then(Value::as_str)
}

/// The console message an event describes, if it is one of the three
/// console events.
fn console_message(method: &str, params: &Value) -> Option<ConsoleMessage> {
    match method {
        "Log.entryAdded" => params.get("entry").map(log_entry),
        "Runtime.consoleAPICalled" => Some(console_api_call(params)),
        "Runtime.exceptionThrown" => params
            .get("exceptionDetails")
            .map(|details| exception(params, details)),
        _ => None,
    }
}

fn timestamp(value: &Value) -> f64 {
    value
        .get("timestamp")
        .and_then(Value::as_f64)
        .unwrap_or(0.0)
}

fn log_entry(entry: &Value) -> ConsoleMessage {
    ConsoleMessage {
        source: string_or_empty(entry, "source"),
        level: string_or_empty(entry, "level"),
        text: string_or_empty(entry, "text"),
        timestamp: timestamp(entry),
        url: opt_string(entry, "url"),
        line_number: entry.get("lineNumber").and_then(Value::as_i64),
        channel: Channel::Primary,
    }
}

fn console_api_call(params: &Value) -> ConsoleMessage {
    let text = params
        .get("args")
        .and_then(Value::as_array)
        .map(|args| {
            args.iter()
                .filter_map(arg_text)
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();
    ConsoleMessage {
        source: "console-api".to_string(),
        level: string_or_empty(params, "type"),
        text,
        timestamp: timestamp(params),
        url: None, // Could extract from stackTrace if needed
        line_number: None,
        channel: Channel::Primary,
    }
}

/// One `console.*` argument as text: strings as is, numbers and booleans
/// printed, objects by their description.
fn arg_text(arg: &Value) -> Option<String> {
    let description = str_field(arg, "description").map(str::to_string);
    match arg.get("value") {
        Some(Value::String(s)) => Some(s.clone()),
        Some(val) if val.is_number() || val.is_boolean() => Some(val.to_string()),
        Some(val) => Some(description.unwrap_or_else(|| val.to_string())),
        None => description,
    }
}

fn exception(params: &Value, details: &Value) -> ConsoleMessage {
    let text = details
        .get("exception")
        .and_then(|e| str_field(e, "description"))
        .map(str::to_string)
        .unwrap_or_else(|| string_or_empty(details, "text"));
    ConsoleMessage {
        source: "exception".to_string(),
        level: "error".to_string(),
        text,
        timestamp: timestamp(params),
        url: opt_string(details, "url"),
        line_number: details.get("lineNumber").and_then(Value::as_i64),
        channel: Channel::Primary,
    }
}

pub(crate) fn start_log_listener(
    target: &crate::handler::cdp_domains::cdp_target::CdpTarget,
    state: Arc<Mutex<LogState>>,
) -> crate::handler::cdp_domains::event_pump::ListenerHandles {
    use crate::handler::cdp_domains::event_pump::{ListenerHandles, spawn_state_listener};
    // One ordered stream of every domain (filtered in `process_log_event`):
    // a navigation must clear the buffer before the new page's first
    // messages, which separate per-domain tasks would not guarantee.
    let mut handles = ListenerHandles::default();
    handles.push(spawn_state_listener(
        target,
        "",
        state,
        |event, state| async move { process_log_event(&event, &state).await },
    ));
    handles
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn log_entry_becomes_a_console_message() {
        let params = json!({"entry": {"source": "network", "level": "error", "text": "404",
            "timestamp": 1.5, "url": "https://a/x", "lineNumber": 3}});
        let m = console_message("Log.entryAdded", &params).unwrap();
        assert_eq!(
            (m.source.as_str(), m.level.as_str(), m.text.as_str()),
            ("network", "error", "404")
        );
        assert_eq!(
            (m.timestamp, m.url.as_deref(), m.line_number),
            (1.5, Some("https://a/x"), Some(3))
        );
    }

    #[test]
    fn console_api_arguments_are_joined() {
        let params = json!({"type": "log", "timestamp": 2.0, "args": [
            {"type": "string", "value": "hi"}, {"type": "number", "value": 3},
            {"type": "object", "value": {"a": 1}, "description": "Object"},
            {"type": "object", "description": "Window"}, {"type": "undefined"}]});
        let m = console_message("Runtime.consoleAPICalled", &params).unwrap();
        assert_eq!(m.text, "hi 3 Object Window");
        assert_eq!(
            (m.source.as_str(), m.level.as_str()),
            ("console-api", "log")
        );
    }

    #[test]
    fn exception_prefers_the_exception_description() {
        let params = json!({"timestamp": 4.0, "exceptionDetails": {"text": "Uncaught",
            "url": "https://a/app.js", "lineNumber": 7,
            "exception": {"description": "TypeError: x is undefined"}}});
        let m = console_message("Runtime.exceptionThrown", &params).unwrap();
        assert_eq!(m.text, "TypeError: x is undefined");
        assert_eq!(
            (m.source.as_str(), m.level.as_str(), m.line_number),
            ("exception", "error", Some(7))
        );
        let bare = json!({"exceptionDetails": {"text": "Uncaught"}});
        assert_eq!(
            console_message("Runtime.exceptionThrown", &bare)
                .unwrap()
                .text,
            "Uncaught"
        );
        assert!(console_message("Runtime.executionContextCreated", &bare).is_none());
    }

    fn message(text: &str) -> ConsoleMessage {
        ConsoleMessage {
            source: "console-api".into(),
            level: "log".into(),
            text: text.into(),
            timestamp: 0.0,
            url: None,
            line_number: None,
            channel: Channel::Primary,
        }
    }

    fn event(method: &str, params: Value) -> WsResponse {
        WsResponse {
            method: Some(method.into()),
            params: Some(params),
            ..Default::default()
        }
    }

    fn committed(url: &str) -> WsResponse {
        event(
            "Page.frameNavigated",
            serde_json::json!({"frame": {"id": "F", "url": url}, "type": "Navigation"}),
        )
    }

    async fn texts(state: &Arc<Mutex<LogState>>) -> Vec<String> {
        let st = state.lock().await;
        st.messages.iter().map(|m| m.text.clone()).collect()
    }

    /// CON-01 / CON-02 / CON-04 (Chrome): a cross-site main-frame commit
    /// clears the buffer; subdomains of the site and reloads keep it.
    #[tokio::test]
    async fn cross_site_navigation_clears_the_console() {
        // Given a tab on www.knot.kz with one message
        let state = Arc::new(Mutex::new(LogState::default()));
        process_log_event(&committed("https://www.knot.kz/"), &state).await;
        state.lock().await.push_message(message("home"));
        // When it moves to docs.knot.kz and reloads, the message stays
        process_log_event(&committed("https://docs.knot.kz/a"), &state).await;
        process_log_event(&committed("https://docs.knot.kz/a"), &state).await;
        assert_eq!(texts(&state).await, ["home"]);
        // And an iframe navigating elsewhere changes nothing
        let iframe = event(
            "Page.frameNavigated",
            serde_json::json!({"frame": {"id": "C", "parentId": "F", "url": "https://ads.example/"}}),
        );
        process_log_event(&iframe, &state).await;
        assert_eq!(texts(&state).await, ["home"]);
        // When the tab moves to another site, the buffer is cleared, once noted
        process_log_event(&committed("https://example.com/"), &state).await;
        assert!(texts(&state).await.is_empty());
        let note = state.lock().await.take_cleared_note().unwrap();
        assert_eq!(
            note,
            "1 console messages from knot.kz were cleared when the tab moved to example.com"
        );
        assert!(state.lock().await.take_cleared_note().is_none());
    }

    /// FWS-04: a message both Firefox feeds report is kept once (the
    /// watcher's copy); repeats within one feed are all kept.
    #[test]
    fn duplicates_across_feeds_are_kept_once() {
        let at = |text: &str, ms: f64, channel: Channel| ConsoleMessage {
            timestamp: ms,
            channel,
            source: if channel == Channel::Watcher {
                "security"
            } else {
                "console-api"
            }
            .into(),
            ..message(text)
        };
        let mut state = LogState::default();
        // Watcher first, then BiDi's copy: the copy is dropped
        state.push_deduplicated(at("cookie rejected", 1000.0, Channel::Watcher));
        state.push_deduplicated(at("cookie rejected", 1500.0, Channel::Primary));
        // BiDi first, then the watcher's copy: replaced in place
        state.push_deduplicated(at("boom", 2000.0, Channel::Primary));
        state.push_deduplicated(at("boom", 2100.0, Channel::Watcher));
        // The same text twice from one feed, and long after: all kept
        state.push_deduplicated(at("tick", 3000.0, Channel::Primary));
        state.push_deduplicated(at("tick", 3001.0, Channel::Primary));
        state.push_deduplicated(at("boom", 9000.0, Channel::Watcher));
        let kept: Vec<(&str, &str)> = state
            .messages
            .iter()
            .map(|m| (m.text.as_str(), m.source.as_str()))
            .collect();
        assert_eq!(
            kept,
            [
                ("cookie rejected", "security"),
                ("boom", "security"),
                ("tick", "console-api"),
                ("tick", "console-api"),
                ("boom", "security"),
            ]
        );
    }

    /// CON-03 / CON-07: same-document navigations keep the buffer; sibling
    /// subdomains of a public suffix are different sites.
    #[tokio::test]
    async fn hash_navigation_keeps_and_public_suffix_clears() {
        // Given a tab on a.github.io with one message
        let state = Arc::new(Mutex::new(LogState::default()));
        process_log_event(&committed("https://a.github.io/"), &state).await;
        state.lock().await.push_message(message("a"));
        // When it navigates within the document, nothing is cleared
        let hash = event(
            "Page.navigatedWithinDocument",
            serde_json::json!({"frameId": "F", "url": "https://a.github.io/#x"}),
        );
        process_log_event(&hash, &state).await;
        assert_eq!(texts(&state).await, ["a"]);
        // When it moves to b.github.io, the buffer is cleared
        process_log_event(&committed("https://b.github.io/"), &state).await;
        assert!(texts(&state).await.is_empty());
    }
}
