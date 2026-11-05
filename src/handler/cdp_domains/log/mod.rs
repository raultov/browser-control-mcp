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

/// Most characters kept of a console message's text: a page can log
/// megabytes, which `get_console_logs` would return whole.
const CONSOLE_TEXT_MAX: usize = 10_000;

/// `msg` with its text within [`CONSOLE_TEXT_MAX`].
fn bounded(mut msg: ConsoleMessage) -> ConsoleMessage {
    msg.text = crate::util::text::capped(&msg.text, CONSOLE_TEXT_MAX);
    msg
}

#[derive(Default)]
pub(crate) struct LogState {
    pub messages: VecDeque<ConsoleMessage>,
    /// Site of the tab's current document (`util::url::site`), once known.
    site: Option<String>,
    /// Says what the last cross-site navigation cleared, until it is read.
    cleared_note: Option<String>,
    /// When the last cross-site document committed (ms since the epoch):
    /// messages of the old site arriving late are dropped.
    cleared_before: Option<f64>,
    /// The problems Chrome diagnosed in the tab (`Audits`, ISSUE-60).
    pub(crate) issues: crate::handler::issues::IssueBuffer,
    /// Firefox: by issue code, the time up to which `get_issues` with
    /// `clear` returned that code's issues (they are read from messages).
    pub(crate) issues_cleared_at: std::collections::BTreeMap<String, f64>,
    /// Same-document navigations of the tab (history API, fragment) so
    /// far, and the URL the last reached, so an action can say it changed
    /// the URL (ISSUE-81).
    pub(crate) same_document: u64,
    pub(crate) same_document_url: Option<String>,
    /// Chrome's id of the tab's main frame, once a navigation named it.
    main_frame: Option<String>,
    /// Script URLs of the workers watched in their own session (Chrome,
    /// see `frame_watch`): the tab's `Log` copies of their console messages
    /// are dropped. At most `frame_watch::MAX_WATCHED` (one per watched
    /// target).
    workers: Vec<String>,
}

impl LogState {
    /// Notes a same-document navigation of the tab to `url`.
    pub(crate) fn note_same_document(&mut self, url: &str) {
        self.same_document += 1;
        self.same_document_url = Some(url.to_string());
    }

    /// Notes that the worker running `url` reports its console itself.
    pub(crate) fn watch_worker(&mut self, url: &str) {
        self.workers.push(url.to_string());
    }

    /// Forgets one watch of the worker running `url` (it detached).
    pub(crate) fn forget_worker(&mut self, url: &str) {
        if let Some(at) = self.workers.iter().position(|w| w == url) {
            self.workers.remove(at);
        }
    }

    /// Whether a worker running `url` is watched.
    pub(crate) fn watches_worker(&self, url: &str) -> bool {
        self.workers.iter().any(|w| w == url)
    }

    /// Whether `message` is the tab's copy of a watched worker's message.
    fn is_watched_worker_copy(&self, message: &ConsoleMessage) -> bool {
        message.source == "worker"
            && message
                .url
                .as_deref()
                .is_some_and(|url| self.watches_worker(url))
    }

    pub(crate) fn push_message(&mut self, msg: ConsoleMessage) {
        self.push_bounded(bounded(msg));
    }

    /// Pushes a message whose text is already bounded.
    fn push_bounded(&mut self, msg: ConsoleMessage) {
        self.messages.push_back(msg);
        if self.messages.len() > MAX_CONSOLE_MESSAGES {
            self.messages.pop_front();
        }
    }

    /// Pushes a message that another feed may report too (Firefox): a copy
    /// already pushed by the other feed (same text, close timestamps) is
    /// kept once, with the watcher's labels, which say more.
    pub(crate) fn push_deduplicated(&mut self, msg: ConsoleMessage) {
        if self
            .cleared_before
            .is_some_and(|at| msg.timestamp > 0.0 && msg.timestamp < at)
        {
            return;
        }
        let msg = bounded(msg);
        let twin = self
            .messages
            .iter_mut()
            .rev()
            .take(DUPLICATE_LOOKBACK)
            .find(|m| {
                m.channel != msg.channel
                    && without_in_promise(&m.text) == without_in_promise(&msg.text)
                    && (m.timestamp - msg.timestamp).abs() <= DUPLICATE_WINDOW_MS
            });
        match twin {
            Some(existing) if msg.channel == Channel::Watcher => *existing = msg,
            Some(_) => {}
            None => self.push_bounded(msg),
        }
    }

    /// A new top-level document committed at `url` (at `at`, ms since the
    /// epoch, when the browser says). Moving to another site clears the
    /// buffer (the old site's messages would bury the new page's); same-site
    /// navigations and reloads keep it. Same-document navigations (fragment,
    /// history API) never get here. With `at`, messages stamped at or after
    /// it stay (Firefox reports some over another connection, which can
    /// overtake the commit), and later ones stamped before it are dropped.
    pub(crate) fn on_document_committed(&mut self, url: &str, at: Option<f64>) {
        let site = crate::util::url::site(url);
        if let Some(previous) = self.site.as_deref()
            && previous != site
        {
            let before = self.messages.len();
            match at {
                Some(at) => self.messages.retain(|m| m.timestamp >= at),
                None => self.messages.clear(),
            }
            let cleared = before - self.messages.len();
            if cleared > 0 {
                self.cleared_note = Some(format!(
                    "{cleared} console messages from {previous} were cleared when the tab moved to {site}"
                ));
            }
            self.cleared_before = at;
            // The old site's issues too (ISSUE-60).
            self.issues.reset();
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
        let mut state = state.lock().await;
        state.main_frame = params["frame"]["id"].as_str().map(str::to_string);
        state.on_document_committed(url, None);
    } else if method == "Audits.issueAdded" {
        if let Some(mut issue) = crate::handler::issues::from_chrome(params) {
            // Its node ids are only valid in that session (ISSUE-85).
            issue.session = event.session_id.clone();
            state.lock().await.issues.push(issue);
        }
    } else if method == "Page.navigatedWithinDocument" {
        let mut state = state.lock().await;
        let frame = params["frameId"].as_str();
        let main = state.main_frame.is_none() || state.main_frame.as_deref() == frame;
        if let (true, Some(url)) = (main, params["url"].as_str()) {
            state.note_same_document(url);
        }
    } else if let Some(message) = console_message(method, params) {
        let mut state = state.lock().await;
        if !state.is_watched_worker_copy(&message) {
            state.push_message(message);
        }
    }
}

/// A console level in the vocabulary both browsers report, lowercase:
/// Firefox's console API names `console.warn` `warn` where Chrome (and
/// Firefox's own warnings) say `warning`, so one `level_filter` works on
/// both.
pub(crate) fn canonical_level(level: &str) -> String {
    let level = level.to_lowercase();
    if level == "warn" {
        "warning".to_string()
    } else {
        level
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

/// Most properties or items a console preview shows.
const PREVIEW_ITEMS: usize = 10;

/// Most characters of a string inside a preview (Chrome cuts its own at 100).
const PREVIEW_STRING_CHARS: usize = 100;

/// `items` between `open` and `close` as the console previews them, with
/// `…` when there are more (`overflow`, or over [`PREVIEW_ITEMS`]).
fn listed(open: &str, close: &str, items: Vec<String>, overflow: bool) -> String {
    let more = overflow || items.len() > PREVIEW_ITEMS;
    let mut shown: Vec<String> = items.into_iter().take(PREVIEW_ITEMS).collect();
    if more {
        shown.push("…".to_string());
    }
    format!("{open}{}{close}", shown.join(", "))
}

/// An object as the console previews it: `{n: 1, s: 'x'}`.
pub(crate) fn object_preview(entries: Vec<(String, String)>, overflow: bool) -> String {
    let items = entries
        .into_iter()
        .map(|(name, value)| format!("{name}: {value}"))
        .collect();
    listed("{", "}", items, overflow)
}

/// An array as the console previews it: `[1, 2, 3]`.
pub(crate) fn array_preview(items: Vec<String>, overflow: bool) -> String {
    listed("[", "]", items, overflow)
}

/// A string inside a preview: quoted, and cut at [`PREVIEW_STRING_CHARS`].
pub(crate) fn quoted(text: &str) -> String {
    match text.char_indices().nth(PREVIEW_STRING_CHARS) {
        Some((cut, _)) => format!("'{}…'", &text[..cut]),
        None => format!("'{text}'"),
    }
}

/// A CDP object preview (`RemoteObject.preview`) as text: plain objects
/// and arrays, the rest (errors, maps, nodes...) by their description.
fn preview_text(preview: &Value) -> Option<String> {
    let properties = preview.get("properties")?.as_array()?;
    let overflow = preview.get("overflow").and_then(Value::as_bool) == Some(true);
    let value = |property: &Value| {
        let text = str_field(property, "value").unwrap_or_default();
        match str_field(property, "type") {
            Some("string") => quoted(text),
            Some("undefined") => "undefined".to_string(),
            _ => text.to_string(),
        }
    };
    let description = str_field(preview, "description").unwrap_or_default();
    match str_field(preview, "subtype") {
        Some("array") => Some(array_preview(
            properties.iter().map(value).collect(),
            overflow,
        )),
        None if str_field(preview, "type") == Some("object") => {
            let entries = properties
                .iter()
                .map(|p| {
                    (
                        str_field(p, "name").unwrap_or_default().to_string(),
                        value(p),
                    )
                })
                .collect();
            let object = object_preview(entries, overflow);
            Some(match description {
                "Object" | "" => object,
                class => format!("{class} {object}"),
            })
        }
        _ => None,
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
    // Where the call was made; evaluated code has no URL.
    let frame = params
        .pointer("/stackTrace/callFrames/0")
        .filter(|frame| str_field(frame, "url").is_some_and(|url| !url.is_empty()));
    ConsoleMessage {
        source: "console-api".to_string(),
        level: string_or_empty(params, "type"),
        text,
        timestamp: timestamp(params),
        url: frame.and_then(|f| opt_string(f, "url")),
        line_number: frame
            .and_then(|f| f.get("lineNumber"))
            .and_then(Value::as_i64),
        channel: Channel::Primary,
    }
}

/// One `console.*` argument as text: strings as is, numbers and booleans
/// printed, objects and arrays previewed, the rest by their description.
fn arg_text(arg: &Value) -> Option<String> {
    if let Some(text) = arg.get("preview").and_then(preview_text) {
        return Some(text);
    }
    if str_field(arg, "type") == Some("undefined") {
        return Some("undefined".to_string());
    }
    let description = str_field(arg, "description").map(str::to_string);
    match arg.get("value") {
        Some(Value::String(s)) => Some(s.clone()),
        Some(val) if val.is_number() || val.is_boolean() => Some(val.to_string()),
        Some(val) => Some(description.unwrap_or_else(|| val.to_string())),
        None => description,
    }
}

/// How an unhandled promise rejection's text starts, on both browsers
/// (Chrome's `exceptionDetails.text`; Firefox's watcher flags it).
pub(crate) const IN_PROMISE: &str = "Uncaught (in promise) ";

/// `text` without [`IN_PROMISE`]: Firefox reports a rejection twice, only
/// its watcher knowing it is one.
fn without_in_promise(text: &str) -> &str {
    text.strip_prefix(IN_PROMISE).unwrap_or(text)
}

fn exception(params: &Value, details: &Value) -> ConsoleMessage {
    let description = details
        .get("exception")
        .and_then(|e| str_field(e, "description"));
    let rejection =
        str_field(details, "text").is_some_and(|t| t.starts_with(IN_PROMISE.trim_end()));
    let text = match description {
        Some(described) if rejection && !described.starts_with("Uncaught") => {
            format!("{IN_PROMISE}{described}")
        }
        Some(described) => described.to_string(),
        None => string_or_empty(details, "text"),
    };
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

    /// ISSUE-75 (Chrome): a watched worker's console comes from its own
    /// session (with its real level); the tab's `Log` copy of it (`source:
    /// "worker"`, `console.log` as `info`) is dropped. Other workers' copies
    /// are kept.
    #[tokio::test]
    async fn watched_workers_are_not_logged_twice() {
        // Given a tab watching worker w.js
        let state = Arc::new(Mutex::new(LogState::default()));
        state.lock().await.watch_worker("http://a/w.js");
        let entry = |url: &str| json!({"method": "Log.entryAdded", "params": {"entry": {"source": "worker", "level": "info", "text": "hi", "url": url, "timestamp": 1.0}}});
        // When the tab reports both workers' messages
        for url in ["http://a/w.js", "http://a/other.js"] {
            let event: WsResponse = serde_json::from_value(entry(url)).unwrap();
            process_log_event(&event, &state).await;
        }
        // Then only the unwatched worker's is kept
        let urls: Vec<Option<String>> = state
            .lock()
            .await
            .messages
            .iter()
            .map(|m| m.url.clone())
            .collect();
        assert_eq!(urls, [Some("http://a/other.js".to_string())]);
        // And once it stops, its messages are kept again
        state.lock().await.forget_worker("http://a/w.js");
        let event: WsResponse = serde_json::from_value(entry("http://a/w.js")).unwrap();
        process_log_event(&event, &state).await;
        assert_eq!(state.lock().await.messages.len(), 2);
    }

    /// ISSUE-19: console arguments print as the console shows them (object
    /// and array previews, not "Object"), and the call's location is kept
    /// when the code has a URL (evaluated code has none).
    #[test]
    fn console_api_arguments_are_previewed() {
        let events: Value = serde_json::from_str(include_str!(
            "../../../../tests/fixtures/cdp/runtime-console-api-called.json"
        ))
        .unwrap();
        let first = console_message("Runtime.consoleAPICalled", &events[0]["params"]).unwrap();
        assert_eq!(
            first.text,
            "hello from page {n: 1, s: 'x', nested: Object} [1, 2, 3] 42 null"
        );
        assert_eq!((first.url, first.line_number), (None, None));
        let error = console_message("Runtime.consoleAPICalled", &events[2]["params"]).unwrap();
        assert!(
            error.text.starts_with("Error: boom-error"),
            "{}",
            error.text
        );
        // A call from a page script keeps its location
        let mut from_page = events[1]["params"].clone();
        from_page["stackTrace"]["callFrames"][0]["url"] = json!("http://a/app.js");
        from_page["stackTrace"]["callFrames"][0]["lineNumber"] = json!(6);
        let warn = console_message("Runtime.consoleAPICalled", &from_page).unwrap();
        assert_eq!(
            (warn.url.as_deref(), warn.line_number),
            (Some("http://a/app.js"), Some(6))
        );
    }

    /// AUD-OB-13: a huge console message is kept cut, on both feeds.
    #[test]
    fn long_console_text_is_capped() {
        let params = json!({"entry": {"source": "javascript", "level": "info", "text": "x".repeat(3_000_000), "timestamp": 1.0}});
        let mut state = LogState::default();
        state.push_message(console_message("Log.entryAdded", &params).unwrap());
        state.push_deduplicated(console_message("Log.entryAdded", &params).unwrap());
        for message in &state.messages {
            assert!(message.text.starts_with(&"x".repeat(CONSOLE_TEXT_MAX)));
            assert!(
                message
                    .text
                    .ends_with("[cut: the first 10000 of 3000000 characters]")
            );
        }
    }

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
        assert_eq!(m.text, "hi 3 Object Window undefined");
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

    /// ISSUE-60 (Chrome): the tab keeps the issues Chrome reports
    /// (`Audits.issueAdded`, folded), and forgets them on another site, as
    /// its console messages.
    #[tokio::test]
    async fn issues_are_kept_until_another_site() {
        let state = Arc::new(Mutex::new(LogState::default()));
        let send = |method: &str, params: Value| WsResponse {
            method: Some(method.into()),
            params: Some(params),
            ..Default::default()
        };
        let cors = json!({"issue": {"code": "CorsIssue", "details": {"corsIssueDetails": {"corsErrorStatus": {"corsError": "MissingAllowOriginHeader"}, "request": {"url": "http://b/api"}}}}});
        let page = |url: &str| json!({"frame": {"id": "F", "url": url}});
        process_log_event(&send("Page.frameNavigated", page("http://a/")), &state).await;
        for _ in 0..2 {
            process_log_event(&send("Audits.issueAdded", cors.clone()), &state).await;
        }
        // (the same details twice are one report sent again, ISSUE-85)
        let kept = state.lock().await.issues.list();
        assert_eq!((kept.len(), kept[0].count), (1, 1));
        process_log_event(&send("Page.frameNavigated", page("http://a/other")), &state).await;
        assert_eq!(state.lock().await.issues.list().len(), 1, "same site");
        process_log_event(&send("Page.frameNavigated", page("http://c.test/")), &state).await;
        assert!(state.lock().await.issues.list().is_empty());
    }

    /// ISSUE-81 (Chrome): the main frame's same-document navigations
    /// (history API, fragment) are counted with the URL reached; an
    /// iframe's are not.
    #[tokio::test]
    async fn same_document_navigations_are_counted() {
        let state = Arc::new(Mutex::new(LogState::default()));
        let events = [
            (
                "Page.frameNavigated",
                json!({"frame": {"id": "F", "url": "http://a/"}}),
            ),
            (
                "Page.navigatedWithinDocument",
                json!({"frameId": "F", "url": "http://a/pushed", "navigationType": "historyApi"}),
            ),
            (
                "Page.navigatedWithinDocument",
                json!({"frameId": "C", "url": "http://a/ad#x", "navigationType": "fragment"}),
            ),
        ];
        for (method, params) in events {
            let event = WsResponse {
                method: Some(method.into()),
                params: Some(params),
                ..Default::default()
            };
            process_log_event(&event, &state).await;
        }
        let st = state.lock().await;
        assert_eq!(
            (st.same_document, st.same_document_url.as_deref()),
            (1, Some("http://a/pushed"))
        );
    }

    /// ISSUE-80 (Chrome): an unhandled rejection says `(in promise)`
    /// (Chrome's `text`), not only `Uncaught`.
    #[test]
    fn rejections_say_in_promise() {
        let params = json!({"exceptionDetails": {"text": "Uncaught (in promise)",
            "exception": {"description": "Error: rej\n    at x (http://a/:1:1)"}}});
        let m = console_message("Runtime.exceptionThrown", &params).unwrap();
        assert_eq!(
            m.text,
            "Uncaught (in promise) Error: rej\n    at x (http://a/:1:1)"
        );
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

    /// REV-F11-5: with the commit time, messages the new page logged before
    /// the commit event arrived stay, and the old site's late ones go.
    #[test]
    fn commit_time_keeps_early_new_page_messages() {
        let at = |text: &str, ms: f64| ConsoleMessage {
            timestamp: ms,
            ..message(text)
        };
        let mut state = LogState::default();
        state.on_document_committed("https://a.test/", Some(0.0));
        state.push_deduplicated(at("old", 1000.0));
        state.push_deduplicated(at("new early", 2500.0));
        // When b.test commits at 2000
        state.on_document_committed("https://b.test/", Some(2000.0));
        // And a late message of a.test arrives
        state.push_deduplicated(at("old late", 1900.0));
        state.push_deduplicated(at("new", 2600.0));
        let texts: Vec<&str> = state.messages.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(texts, ["new early", "new"]);
        assert_eq!(
            state.take_cleared_note().unwrap(),
            "1 console messages from a.test were cleared when the tab moved to b.test"
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
