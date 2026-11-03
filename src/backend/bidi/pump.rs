//! The single event pump of a Firefox instance (specs D34): one global
//! subscription, every event routed by browsing context into the state of
//! the tab that owns it (iframes included).

use super::session::FirefoxTabs;
use crate::handler::NetworkRequest;
use crate::handler::cdp_domains::log::ConsoleMessage;
use crate::util::json::{opt_string, str_field, string_or_empty};
use crate::util::sync::{read, write};
use bidi_browser_lite::{BrowserClient, WsResponse};
use serde_json::{Map, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use tokio::sync::broadcast::{self, error::RecvError};

/// The events the pump subscribes to.
pub(crate) const EVENTS: &[&str] = &[
    "browsingContext.contextCreated",
    "browsingContext.contextDestroyed",
    "browsingContext.navigationStarted",
    "browsingContext.navigationCommitted",
    "browsingContext.load",
    "browsingContext.userPromptOpened",
    "browsingContext.userPromptClosed",
    "log.entryAdded",
    "network.beforeRequestSent",
    "network.responseStarted",
    "network.responseCompleted",
    "network.fetchError",
];

/// Events subscribed when Firefox knows them: an older one refuses the
/// whole subscription that names them, which is then made without.
pub(crate) const OPTIONAL_EVENTS: &[&str] = &[
    "browsingContext.downloadWillBegin",
    "browsingContext.downloadEnd",
    "network.authRequired",
];

/// What the pump shares with its session.
#[derive(Clone)]
pub(crate) struct PumpState {
    pub(crate) tabs: Arc<RwLock<FirefoxTabs>>,
    pub(crate) lagged: Arc<AtomicU64>,
    pub(crate) custom_modules: Arc<RwLock<Vec<String>>>,
    /// The instance's downloads (#77).
    pub(crate) downloads: Arc<tokio::sync::Mutex<crate::handler::downloads::DownloadState>>,
}

/// Starts the pump task.
pub(crate) fn spawn(
    mut events: broadcast::Receiver<WsResponse>,
    client: BrowserClient,
    state: PumpState,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match events.recv().await {
                Ok(event) => route(&event, &client, &state).await,
                Err(RecvError::Lagged(skipped)) => {
                    tracing::warn!(skipped, "Firefox event stream lagged");
                    state.lagged.fetch_add(skipped, Ordering::Relaxed);
                }
                Err(RecvError::Closed) => break,
            }
        }
    })
}

/// The context an event belongs to: `params.context`, or
/// `params.source.context` for `log.entryAdded` (spike H10).
pub(crate) fn event_context(params: &Value) -> Option<&str> {
    str_field(params, "context")
        .or_else(|| params.get("source").and_then(|s| str_field(s, "context")))
}

/// Routes one event into the owning tab's state, and into its custom
/// events when the agent subscribed to it (also the events the server
/// handles itself: ISSUE-71).
pub(crate) async fn route(event: &WsResponse, client: &BrowserClient, state: &PumpState) {
    let (Some(method), Some(params)) = (event.method.as_deref(), event.params.as_ref()) else {
        return;
    };
    // Read before the handler: it forgets a closed tab.
    let tab_lifecycle = is_tab_lifecycle(method, params, state);
    handle(method, params, client, state).await;
    on_custom(method, params, state, tab_lifecycle).await;
}

/// Whether `method` opens or closes a tab (a top-level context): an event
/// about the browser's tabs rather than a page's, kept on the active tab.
fn is_tab_lifecycle(method: &str, params: &Value, state: &PumpState) -> bool {
    match method {
        "browsingContext.contextCreated" => str_field(params, "parent").is_none(),
        "browsingContext.contextDestroyed" => str_field(params, "context").is_some_and(|c| {
            read(&state.tabs)
                .tabs
                .values()
                .any(|t| t.tab.context_id() == c)
        }),
        _ => false,
    }
}

/// The server's own handling of an event.
async fn handle(method: &str, params: &Value, client: &BrowserClient, state: &PumpState) {
    match method {
        "browsingContext.contextCreated" => on_context_created(params, client, state).await,
        "browsingContext.contextDestroyed" => on_context_destroyed(params, state).await,
        "browsingContext.navigationStarted"
        | "browsingContext.navigationCommitted"
        | "browsingContext.load" => on_navigation(method, params, state).await,
        "browsingContext.userPromptOpened" | "browsingContext.userPromptClosed" => {
            on_prompt(method, params, state).await;
        }
        "browsingContext.downloadWillBegin" => on_download(params, state).await,
        "browsingContext.downloadEnd" => on_download_end(params, state).await,
        "log.entryAdded" => on_log(params, state).await,
        _ if method.starts_with("network.") => {
            if let Some(network) = tab_part(state, params, |t| t.network.clone()) {
                on_network(method, params, &mut *network.lock().await);
            }
        }
        _ => {}
    }
}

/// Keeps a console message in the tab owning the event's context; a
/// CORS block it reports marks the request (see [`cors_block`]).
async fn on_log(params: &Value, state: &PumpState) {
    let message = console_message(params);
    if let Some((url, reason)) = cors_block(&message.text)
        && let Some(network) = tab_part(state, params, |t| t.network.clone())
    {
        let error = format!("blocked by CORS: {reason}");
        network.lock().await.note_blocked(url, &error);
    }
    if let Some(log) = tab_part(state, params, |t| t.log.clone()) {
        log.lock().await.push_deduplicated(message);
    }
}

/// The URL and reason of Firefox's console error for a cross-origin
/// response the page may not read (`Cross-Origin Request Blocked: … the
/// remote resource at <URL>. (Reason: <reason>). …`): Firefox reports the
/// request answered, with its body, and says so nowhere else (ISSUE-78).
fn cors_block(text: &str) -> Option<(&str, &str)> {
    let rest = text.strip_prefix("Cross-Origin Request Blocked:")?;
    let (_, rest) = rest.split_once("remote resource at ")?;
    let (url, rest) = rest.split_once(". (Reason: ")?;
    let (reason, _) = rest.split_once(')')?;
    Some((url, reason))
}

/// Notes in the tab owning the event's context that a navigation became a
/// download (`navigate` then fails saying so), and records the download
/// (#77).
async fn on_download(params: &Value, state: &PumpState) {
    let url = string_or_empty(params, "url");
    let network = tab_part(state, params, |t| t.network.clone());
    if let (Some(navigation), Some(network)) = (str_field(params, "navigation"), network) {
        network.lock().await.note_download(navigation, &url);
    }
    let Some(id) = download_id(params) else {
        return;
    };
    let tab = event_context(params).and_then(|context| read(&state.tabs).tab_of(context));
    let name = str_field(params, "suggestedFilename").unwrap_or(id);
    let download = crate::handler::downloads::Download::begun(id, &url, name, tab);
    state.downloads.lock().await.begin(download);
}

/// A download's id: `download`, or the id of the navigation that became
/// one where Firefox names no download.
fn download_id(params: &Value) -> Option<&str> {
    str_field(params, "download").or_else(|| str_field(params, "navigation"))
}

/// Completes (with its file and size) or cancels a download that ended.
async fn on_download_end(params: &Value, state: &PumpState) {
    use crate::handler::downloads::DownloadStatus;
    let Some(id) = download_id(params) else {
        return;
    };
    let path = str_field(params, "filepath").map(std::path::PathBuf::from);
    // Firefox reports no size: the file's, when the server can see it.
    let size = match &path {
        Some(path) => tokio::fs::metadata(path).await.ok().map(|m| m.len()),
        None => None,
    };
    let mut downloads = state.downloads.lock().await;
    let Some(download) = downloads.get_mut(id) else {
        return;
    };
    if str_field(params, "status") == Some("complete") {
        download.state = DownloadStatus::Completed;
        download.path = path;
        download.received_bytes = size;
        download.total_bytes = size;
    } else {
        download.state = DownloadStatus::Canceled;
    }
}

/// Keeps the open JavaScript dialog of the tab owning the event's context.
async fn on_prompt(method: &str, params: &Value, state: &PumpState) {
    if let Some(dialog) = tab_part(state, params, |t| t.dialog.clone()) {
        let opened = (method == "browsingContext.userPromptOpened")
            .then(|| crate::handler::dialogs::OpenDialog::from_firefox(params));
        dialog.lock().await.open = opened;
    }
}

/// A piece of the state of the tab owning the event's context.
fn tab_part<T>(
    state: &PumpState,
    params: &Value,
    pick: impl Fn(&super::session::FirefoxTab) -> T,
) -> Option<T> {
    let tabs = read(&state.tabs);
    let id = tabs.tab_of(event_context(params)?)?;
    tabs.tabs.get(&id).map(pick)
}

async fn on_context_created(params: &Value, client: &BrowserClient, state: &PumpState) {
    let Some(context) = str_field(params, "context") else {
        return;
    };
    match str_field(params, "parent") {
        Some(parent) => {
            write(&state.tabs)
                .parents
                .insert(context.to_string(), parent.to_string());
        }
        None => {
            // A tab opened by the page (or by `open_tab`, which dedupes).
            if let Ok(tab) = client.attach(context).await {
                let url = string_or_empty(params, "url");
                let _ = write(&state.tabs).register(tab, None, &url);
            }
        }
    }
}

async fn on_navigation(method: &str, params: &Value, state: &PumpState) {
    let Some(context) = str_field(params, "context") else {
        return;
    };
    let top = {
        let mut tabs = write(&state.tabs);
        let Some(id) = tabs.tab_of(context) else {
            return;
        };
        let Some(tab) = tabs.tabs.get_mut(&id) else {
            return;
        };
        if tab.tab.context_id() != context {
            return; // an iframe navigating: not a new page
        }
        if let Some(url) = str_field(params, "url") {
            tab.url = url.to_string();
        }
        (tab.network.clone(), tab.log.clone())
    };
    let (network, log) = top;
    match method {
        "browsingContext.navigationStarted" => network.lock().await.split_after_navigation(None),
        // The final URL (after redirects) of a new document; fragment and
        // history navigations have their own events.
        "browsingContext.navigationCommitted" => {
            if let Some(url) = str_field(params, "url") {
                let at = params.get("timestamp").and_then(Value::as_f64);
                log.lock().await.on_document_committed(url, at);
            }
        }
        _ => {}
    }
}

/// `log.entryAdded` as the console message Chrome's tools report.
pub(crate) fn console_message(params: &Value) -> ConsoleMessage {
    let javascript = str_field(params, "type") == Some("javascript");
    let frame = params.pointer("/stackTrace/callFrames/0");
    ConsoleMessage {
        source: if javascript {
            "exception"
        } else {
            "console-api"
        }
        .to_string(),
        level: if javascript {
            "error".to_string()
        } else {
            let method = str_field(params, "method").or_else(|| str_field(params, "level"));
            crate::handler::cdp_domains::log::canonical_level(method.unwrap_or_default())
        },
        text: args_text(params).unwrap_or_else(|| string_or_empty(params, "text")),
        timestamp: params
            .get("timestamp")
            .and_then(Value::as_f64)
            .unwrap_or(0.0),
        url: frame.and_then(|f| opt_string(f, "url")),
        line_number: frame
            .and_then(|f| f.get("lineNumber"))
            .and_then(Value::as_i64),
        channel: Default::default(),
    }
}

/// The console arguments printed as Chrome previews them (Firefox's own
/// text says `Object(3)`), or `None` when one is a value BiDi does not
/// serialize (an error, a node, a map) or the first is a format string
/// (`%s`): Firefox's text then says it better.
fn args_text(params: &Value) -> Option<String> {
    let args = params.get("args")?.as_array()?;
    let first = args.first()?;
    if str_field(first, "type") == Some("string")
        && str_field(first, "value").is_some_and(|text| text.contains('%'))
    {
        return None;
    }
    let printed: Option<Vec<String>> = args.iter().map(|arg| bidi_value(arg, true)).collect();
    Some(printed?.join(" "))
}

/// A BiDi remote value as the console prints it: at the top level strings
/// as is, objects and arrays previewed; inside a preview strings quoted and
/// objects and arrays by name, as Chrome's previews show them.
fn bidi_value(arg: &Value, top: bool) -> Option<String> {
    let value = arg.get("value");
    match str_field(arg, "type")? {
        "string" => {
            let text = value?.as_str()?;
            Some(if top {
                text.to_string()
            } else {
                crate::handler::cdp_domains::log::quoted(text)
            })
        }
        "number" | "boolean" => Some(match value? {
            // NaN, -0, Infinity and -Infinity come as strings.
            Value::String(special) => special.clone(),
            other => other.to_string(),
        }),
        "bigint" => Some(format!("{}n", value?.as_str()?)),
        "null" => Some("null".to_string()),
        "undefined" => Some("undefined".to_string()),
        "array" if top => bidi_array(value?),
        "object" if top => bidi_object(value?),
        "array" => Some(match value.and_then(Value::as_array) {
            Some(items) => format!("Array({})", items.len()),
            None => "Array".to_string(),
        }),
        "object" => Some("Object".to_string()),
        _ => None,
    }
}

/// A serialized array previewed: `[1, 2, 3]`.
fn bidi_array(items: &Value) -> Option<String> {
    let items: Option<Vec<String>> = items
        .as_array()?
        .iter()
        .map(|item| bidi_value(item, false))
        .collect();
    Some(crate::handler::cdp_domains::log::array_preview(
        items?, false,
    ))
}

/// A serialized object (`[[key, value], ...]`) previewed: `{n: 1}`.
fn bidi_object(pairs: &Value) -> Option<String> {
    let entries: Option<Vec<(String, String)>> = pairs
        .as_array()?
        .iter()
        .map(|pair| {
            let key = pair.get(0)?;
            let key = key
                .as_str()
                .map(str::to_string)
                .or_else(|| bidi_value(key, false))?;
            Some((key, bidi_value(pair.get(1)?, false)?))
        })
        .collect();
    Some(crate::handler::cdp_domains::log::object_preview(
        entries?, false,
    ))
}

/// BiDi headers `[{name, value: {type, value}}]` as `{name: value}`; a
/// repeated header (several Set-Cookie) keeps every value, joined by
/// newlines as Chrome reports them.
fn headers(list: Option<&Value>) -> Option<Value> {
    let list = list?.as_array()?;
    let mut map = Map::new();
    for header in list {
        let Some(name) = str_field(header, "name") else {
            continue;
        };
        let value = header
            .get("value")
            .and_then(|v| str_field(v, "value"))
            .unwrap_or_default();
        let joined = match map.get(name).and_then(Value::as_str) {
            Some(before) => format!("{before}\n{value}"),
            None => value.to_string(),
        };
        map.insert(name.to_string(), Value::String(joined));
    }
    Some(Value::Object(map))
}

/// A BiDi request's type: its `destination` (`document`, `script`...),
/// else its `initiatorType` (`fetch`, `xmlhttprequest`), which a fetch or
/// an XHR has with an empty `destination` (ISSUE-78).
fn resource_type(request: &Value) -> Option<String> {
    let non_empty = |key: &str| opt_string(request, key).filter(|value| !value.is_empty());
    non_empty("destination").or_else(|| non_empty("initiatorType"))
}

/// Applies one `network.*` event to a tab's request log (spike H13).
pub(crate) fn on_network(method: &str, params: &Value, network: &mut crate::handler::NetworkState) {
    let Some(request) = params.get("request") else {
        return;
    };
    let Some(id) = str_field(request, "request") else {
        return;
    };
    if method == "network.beforeRequestSent" {
        if let Some(navigation) = str_field(params, "navigation") {
            network.note_document(navigation.to_string(), id.to_string());
        }
        network.insert_request(
            id.to_string(),
            NetworkRequest {
                url: string_or_empty(request, "url"),
                method: string_or_empty(request, "method"),
                resource_type: resource_type(request),
                request_headers: headers(request.get("headers")),
                has_request_body: request.get("bodySize").and_then(Value::as_u64) > Some(0),
                ..Default::default()
            },
        );
        return;
    }
    // A navigation behind HTTP authentication: Firefox shows its prompt and
    // the navigation waits (`navigate` ends on it, see `tools::unless_auth`).
    if method == "network.authRequired" && str_field(params, "navigation").is_some() {
        network.note_auth(id);
    }
    let Some(entry) = network.find_request_mut(id) else {
        return;
    };
    if method == "network.fetchError" {
        entry.error = opt_string(params, "errorText");
        entry.finished = true;
        return;
    }
    if method == "network.responseCompleted" {
        entry.finished = true;
    }
    if let Some(response) = params.get("response") {
        entry.response_status = response.get("status").and_then(Value::as_i64);
        entry.response_status_text = opt_string(response, "statusText");
        entry.response_headers = headers(response.get("headers"));
    }
}

/// Forgets a destroyed context; when it was a tab, the browser picked its
/// own front tab, so the active one is brought back to the front.
async fn on_context_destroyed(params: &Value, state: &PumpState) {
    let Some(context) = str_field(params, "context") else {
        return;
    };
    let tab = read(&state.tabs)
        .tabs
        .values()
        .any(|t| t.tab.context_id() == context);
    write(&state.tabs).remove_context(context);
    if tab {
        super::session::front_active(&state.tabs).await;
    }
}

/// Events of modules the agent subscribed to, kept per tab: the tab
/// owning their context; the active tab for a tab opening or closing
/// (`tab_lifecycle`) and for an event no tab owns (no context, such as
/// `script.realmDestroyed`, or a context already gone).
async fn on_custom(method: &str, params: &Value, state: &PumpState, tab_lifecycle: bool) {
    let module = method.split('.').next().unwrap_or_default();
    if !read(&state.custom_modules)
        .iter()
        .any(|m| m == module || m == method)
    {
        return;
    }
    let owner = match tab_lifecycle {
        true => None,
        false => tab_part(state, params, |t| t.custom.clone()),
    };
    let Some(custom) = owner.or_else(|| active_custom(state)) else {
        return;
    };
    crate::handler::cdp_domains::custom::push_event(&mut *custom.lock().await, method, params);
}

/// The active tab's custom events.
fn active_custom(
    state: &PumpState,
) -> Option<Arc<tokio::sync::Mutex<crate::handler::CustomState>>> {
    let tabs = read(&state.tabs);
    let active = tabs.active.as_ref()?;
    tabs.tabs.get(active).map(|t| t.custom.clone())
}

#[cfg(test)]
mod tests {
    /// ISSUE-19: Firefox console arguments print as on Chrome (its own
    /// text says "Object(3) Array(3)"); an argument BiDi does not serialize
    /// (an error) keeps Firefox's text.
    #[test]
    fn console_arguments_print_as_on_chrome() {
        let events: Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/bidi/event-log-console-args.json"
        ))
        .unwrap();
        let first = console_message(&events[0]["params"]);
        assert_eq!(
            first.text,
            "hello from page {n: 1, s: 'x', nested: Object} [1, 2, 3] 42 null"
        );
        let location = (first.url.as_deref(), first.line_number);
        assert_eq!(location, (Some("http://127.0.0.1:8765/form.html"), Some(0)));
        assert_eq!(
            console_message(&events[2]["params"]).text,
            "Error: boom-error"
        );
    }

    use super::*;
    use bidi_browser_lite::test_support::{MockBidi, MockBidiBehavior};
    use serde_json::json;

    fn event(method: &str, params: Value) -> WsResponse {
        serde_json::from_value(json!({"type": "event", "method": method, "params": params}))
            .unwrap()
    }

    /// A pump state with tab-1 on the mock's context "ctx-0".
    async fn one_tab(mock: &MockBidi) -> (BrowserClient, PumpState) {
        let client = BrowserClient::connect(
            &format!("127.0.0.1:{}", mock.port()),
            std::time::Duration::from_secs(5),
        )
        .await
        .unwrap();
        let state = PumpState {
            tabs: Default::default(),
            lagged: Default::default(),
            custom_modules: Default::default(),
            downloads: Default::default(),
        };
        let tab = client.attach("ctx-0").await.unwrap();
        write(&state.tabs)
            .register(tab, None, "about:home")
            .unwrap();
        (client, state)
    }

    fn fixture(text: &str, context: &str) -> Value {
        let text = text.replace("0eb3f315-4513-4c76-9af3-5d0ab1823fac", context);
        let value: Value = serde_json::from_str(&text).unwrap();
        value["params"].clone()
    }

    /// BID-07: Console events are routed by params.source.context.
    #[tokio::test]
    async fn console_routed_by_source_context() {
        let mock = MockBidi::start(MockBidiBehavior::Ready).await.unwrap();
        let (client, state) = one_tab(&mock).await;
        let params = fixture(
            include_str!("../../../tests/fixtures/bidi/event-log-console-probe.json"),
            "ctx-0",
        );
        route(&event("log.entryAdded", params), &client, &state).await;
        let log = read(&state.tabs).tabs["tab-1"].log.clone();
        let messages = log.lock().await.messages.clone();
        assert_eq!(messages.len(), 1);
        assert!(messages[0].text.contains("hello from probe"));
        assert_eq!(messages[0].source, "console-api");
    }

    /// AUD-OB-02: `console.warn` is level `warning`, as on Chrome and as
    /// Firefox's own warnings (BiDi names the method `warn`).
    #[test]
    fn console_warn_is_level_warning() {
        let params = json!({"type": "console", "method": "warn", "level": "warn", "text": "w", "timestamp": 1});
        assert_eq!(console_message(&params).level, "warning");
        let params = json!({"type": "console", "method": "log", "level": "info", "text": "l", "timestamp": 1});
        assert_eq!(console_message(&params).level, "log");
    }

    /// BKD-08: tab events register and drop tabs.
    #[tokio::test]
    async fn tab_events_register_and_drop_tabs() {
        // Given one known tab
        let mock = MockBidi::start(MockBidiBehavior::Ready).await.unwrap();
        let (client, state) = one_tab(&mock).await;
        // When the page opens a top-level context
        let created = json!({"context": "ctx-7", "url": "http://127.0.0.1/"});
        route(
            &event("browsingContext.contextCreated", created),
            &client,
            &state,
        )
        .await;
        // Then it becomes a tab
        assert_eq!(read(&state.tabs).tabs.len(), 2);
        // And when it is destroyed, the tab goes away
        let destroyed = json!({"context": "ctx-7"});
        route(
            &event("browsingContext.contextDestroyed", destroyed),
            &client,
            &state,
        )
        .await;
        assert_eq!(read(&state.tabs).tabs.len(), 1);
    }

    /// The methods in tab `id`'s custom buffer, in order.
    async fn custom_methods(state: &PumpState, id: &str) -> Vec<String> {
        let custom = read(&state.tabs).tabs[id].custom.clone();
        let events = custom.lock().await.events.clone();
        events.into_iter().map(|e| e.method).collect()
    }

    /// ISSUE-71 (Firefox): an event the agent subscribed to reaches
    /// get_custom_events even when the server handles it too; an event no
    /// tab owns (a tab opening or closing, a realm destroyed) goes to the
    /// active tab instead of being dropped.
    #[tokio::test]
    async fn subscribed_events_reach_the_custom_buffer() {
        // Given tab-1, active, and an agent subscribed to two modules
        let mock = MockBidi::start(MockBidiBehavior::Ready).await.unwrap();
        let (client, state) = one_tab(&mock).await;
        write(&state.custom_modules).extend(["browsingContext".into(), "script".into()]);
        // When tab-1 loads, a tab opens and closes, and a realm is destroyed
        let load =
            json!({"context": "ctx-0", "navigation": "n1", "timestamp": 1, "url": "http://a/"});
        let events = [
            ("browsingContext.load", load),
            (
                "browsingContext.contextCreated",
                json!({"context": "ctx-7", "url": "about:blank"}),
            ),
            (
                "browsingContext.contextDestroyed",
                json!({"context": "ctx-7"}),
            ),
            ("script.realmDestroyed", json!({"realm": "r1"})),
        ];
        for (method, params) in events {
            route(&event(method, params), &client, &state).await;
        }
        // Then the server handled them, and the agent sees them all on tab-1
        assert_eq!(read(&state.tabs).tabs["tab-1"].url, "http://a/");
        assert_eq!(read(&state.tabs).tabs.len(), 1);
        assert_eq!(
            custom_methods(&state, "tab-1").await,
            [
                "browsingContext.load",
                "browsingContext.contextCreated",
                "browsingContext.contextDestroyed",
                "script.realmDestroyed"
            ]
        );
    }

    /// BID-08 / BID-09: iframe events route to their tab; iframes are not tabs.
    #[tokio::test]
    async fn iframe_events_route_to_top_level() {
        let mock = MockBidi::start(MockBidiBehavior::Ready).await.unwrap();
        let (client, state) = one_tab(&mock).await;
        let created = json!({"context": "frame-1", "parent": "ctx-0", "url": "about:blank"});
        route(
            &event("browsingContext.contextCreated", created),
            &client,
            &state,
        )
        .await;
        assert_eq!(read(&state.tabs).tabs.len(), 1, "an iframe is not a tab");
        let params = fixture(
            include_str!("../../../tests/fixtures/bidi/event-before-request-sent.json"),
            "frame-1",
        );
        let params =
            serde_json::from_str::<Value>(&params.to_string().replace("17179869", "frame-1"))
                .unwrap();
        let mut params = params;
        params["context"] = json!("frame-1");
        route(&event("network.beforeRequestSent", params), &client, &state).await;
        let network = read(&state.tabs).tabs["tab-1"].network.clone();
        assert_eq!(network.lock().await.iter_all().len(), 1);
    }

    /// AUD-OB-14 (Firefox): only network.responseCompleted finishes a
    /// request (responseStarted only brings the headers).
    #[test]
    fn response_completed_finishes_the_request() {
        let mut network = crate::handler::NetworkState::default();
        let request =
            json!({"request": "R1", "url": "http://a/sse", "method": "GET", "headers": []});
        on_network(
            "network.beforeRequestSent",
            &json!({"request": request.clone()}),
            &mut network,
        );
        let answer = json!({"request": request, "response": {"status": 200, "headers": []}});
        on_network("network.responseStarted", &answer, &mut network);
        assert!(!network.find_request_mut("R1").unwrap().finished);
        on_network("network.responseCompleted", &answer, &mut network);
        assert!(network.find_request_mut("R1").unwrap().finished);
    }

    /// ISSUE-78 (Firefox): each redirect hop keeps its status and
    /// `Location` (Firefox reuses the request id: a 302 `responseCompleted`,
    /// then a `beforeRequestSent` with `redirectCount` 1).
    #[test]
    fn redirect_hops_keep_status_and_location() {
        let mut network = crate::handler::NetworkState::default();
        let request = |url: &str| json!({"request": "R1", "url": url, "method": "GET", "headers": [], "destination": "", "initiatorType": "fetch"});
        let sent = |url: &str, count: u64| json!({"request": request(url), "redirectCount": count});
        let location =
            json!([{"name": "Location", "value": {"type": "string", "value": "/api/json"}}]);
        let hop = json!({"request": request("http://a/redirect"), "redirectCount": 0, "response": {"status": 302, "statusText": "Found", "headers": location}});
        on_network(
            "network.beforeRequestSent",
            &sent("http://a/redirect", 0),
            &mut network,
        );
        on_network("network.responseCompleted", &hop, &mut network);
        on_network(
            "network.beforeRequestSent",
            &sent("http://a/api/json", 1),
            &mut network,
        );
        let entry = network.find_request_mut("R1").unwrap().clone();
        assert_eq!(entry.url, "http://a/api/json");
        let expected = crate::handler::RedirectHop {
            url: "http://a/redirect".into(),
            status: Some(302),
            location: Some("/api/json".into()),
        };
        assert_eq!(entry.redirects, [expected]);
        assert_eq!((entry.response_status, entry.finished), (None, false));
    }

    /// ISSUE-78 (Firefox): a fetch or XHR has a resource type (its
    /// `initiatorType`; `destination` is empty), and a request with a body
    /// says so (`bodySize`), so the body is asked for.
    #[test]
    fn fetches_have_a_resource_type() {
        let mut network = crate::handler::NetworkState::default();
        let request = |id: &str, initiator: Value, size: u64| json!({"request": {"request": id, "url": "http://a/x", "method": "POST", "headers": [], "destination": "", "initiatorType": initiator, "bodySize": size}});
        on_network(
            "network.beforeRequestSent",
            &request("F", json!("fetch"), 7),
            &mut network,
        );
        on_network(
            "network.beforeRequestSent",
            &request("X", json!("xmlhttprequest"), 0),
            &mut network,
        );
        on_network(
            "network.beforeRequestSent",
            &request("B", Value::Null, 2),
            &mut network,
        );
        let mut kind = |id: &str| {
            let entry = network.find_request_mut(id).unwrap();
            (entry.resource_type.clone(), entry.has_request_body)
        };
        assert_eq!(kind("F"), (Some("fetch".into()), true));
        assert_eq!(kind("X"), (Some("xmlhttprequest".into()), false));
        assert_eq!(kind("B"), (None, true));
    }

    /// ISSUE-78 (Firefox): Firefox answers a cross-origin request the page
    /// may not read with its status and body, and says so only in the
    /// console: that message marks the request as blocked.
    #[test]
    fn cors_console_errors_mark_the_request() {
        let text = "Cross-Origin Request Blocked: The Same Origin Policy disallows reading the remote resource at http://b/api/cors. (Reason: CORS header \u{2018}Access-Control-Allow-Origin\u{2019} missing). Status code: 200.";
        let (url, reason) = cors_block(text).unwrap();
        assert_eq!(url, "http://b/api/cors");
        assert_eq!(
            reason,
            "CORS header \u{2018}Access-Control-Allow-Origin\u{2019} missing"
        );
        assert_eq!(cors_block("Uncaught TypeError: x"), None);
        let mut network = crate::handler::NetworkState::default();
        let sent = json!({"request": {"request": "R1", "url": "http://b/api/cors", "method": "GET", "headers": []}});
        on_network("network.beforeRequestSent", &sent, &mut network);
        network.note_blocked(url, &format!("blocked by CORS: {reason}"));
        let entry = network.find_request_mut("R1").unwrap();
        assert_eq!(
            entry.error.as_deref(),
            Some(
                "blocked by CORS: CORS header \u{2018}Access-Control-Allow-Origin\u{2019} missing"
            )
        );
    }

    /// AUD-OB-17 (Firefox): repeated headers (several Set-Cookie) are kept,
    /// joined by newlines as Chrome does (only the last one was kept).
    #[test]
    fn repeated_headers_are_joined() {
        let header = |name: &str, value: &str| json!({"name": name, "value": {"type": "string", "value": value}});
        let list = json!([
            header("Set-Cookie", "a=1"),
            header("Content-Type", "text/html"),
            header("Set-Cookie", "b=2")
        ]);
        assert_eq!(
            headers(Some(&list)),
            Some(json!({"Set-Cookie": "a=1\nb=2", "Content-Type": "text/html"}))
        );
    }

    /// AUD-OB-07 (Firefox): a fetch error is the request's error, as on
    /// Chrome (it was put in the status text).
    #[test]
    fn fetch_error_is_the_request_error() {
        let mut network = crate::handler::NetworkState::default();
        let request =
            json!({"request": "R1", "url": "http://nope.invalid/", "method": "GET", "headers": []});
        on_network(
            "network.beforeRequestSent",
            &json!({"request": request.clone()}),
            &mut network,
        );
        let failed = json!({"request": request, "errorText": "NS_ERROR_UNKNOWN_HOST"});
        on_network("network.fetchError", &failed, &mut network);
        let entry = network.find_request_mut("R1").unwrap();
        assert_eq!(entry.error.as_deref(), Some("NS_ERROR_UNKNOWN_HOST"));
        assert_eq!(entry.response_status_text, None);
    }

    /// ISSUE-50: a navigation that asks for HTTP authentication is noted
    /// (with the request, whose response has the challenge); a request of
    /// a page script asking for it is not a navigation.
    #[tokio::test]
    async fn authentication_requests_are_noted() {
        let mock = MockBidi::start(MockBidiBehavior::Ready).await.unwrap();
        let (client, state) = one_tab(&mock).await;
        let response = json!({"status": 401, "headers": [{"name": "WWW-Authenticate", "value": {"type": "string", "value": "Basic realm=\"bench\""}}],
            "authChallenges": [{"scheme": "Basic", "realm": "bench"}]});
        for (id, navigation) in [("R1", json!(null)), ("R2", json!("n7"))] {
            let request = json!({"request": id, "url": "http://a/auth", "method": "GET"});
            let sent = json!({"context": "ctx-0", "navigation": navigation, "request": request});
            route(&event("network.beforeRequestSent", sent), &client, &state).await;
            let asked = json!({"context": "ctx-0", "navigation": navigation, "request": request, "response": response, "isBlocked": false});
            route(&event("network.authRequired", asked), &client, &state).await;
        }
        let tab = read(&state.tabs).tabs["tab-1"].clone();
        let mut network = tab.network.lock().await;
        let asked = network.auth_asked_since(0).unwrap();
        assert_eq!(asked.url, "http://a/auth");
        assert_eq!(
            asked.response_header("www-authenticate"),
            Some("Basic realm=\"bench\"")
        );
        assert!(
            network.auth_asked_since(1).is_none(),
            "the script's request counted"
        );
    }

    /// ISSUE-39: a navigation Firefox turned into a download is noted in
    /// its tab's network state, by navigation id.
    #[tokio::test]
    async fn downloads_are_noted_by_navigation() {
        let mock = MockBidi::start(MockBidiBehavior::Ready).await.unwrap();
        let (client, state) = one_tab(&mock).await;
        let download = json!({"context": "ctx-0", "navigation": "n7", "url": "http://a/file.csv", "suggestedFilename": "file.csv"});
        route(
            &event("browsingContext.downloadWillBegin", download),
            &client,
            &state,
        )
        .await;
        let tab = read(&state.tabs).tabs["tab-1"].clone();
        let network = tab.network.lock().await;
        assert!(network.is_download("n7"));
        assert!(!network.is_download("n8"));
    }

    /// ISSUE-77 (Firefox): a download is recorded when it begins, with its
    /// tab, and completed with its file and size (or canceled) when it ends.
    #[tokio::test]
    async fn downloads_are_recorded_until_they_end() {
        use crate::handler::downloads::DownloadStatus;
        let mock = MockBidi::start(MockBidiBehavior::Ready).await.unwrap();
        let (client, state) = one_tab(&mock).await;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("data.csv");
        std::fs::write(&file, "a,b\n1,2\n").unwrap();
        let begin = |id: &str| json!({"context": "ctx-0", "download": id, "navigation": "n1", "url": "http://a/download", "suggestedFilename": "data.csv"});
        let events = [
            ("browsingContext.downloadWillBegin", begin("d1")),
            ("browsingContext.downloadWillBegin", begin("d2")),
            (
                "browsingContext.downloadEnd",
                json!({"context": "ctx-0", "download": "d1", "navigation": "n1", "status": "complete", "filepath": file}),
            ),
            (
                "browsingContext.downloadEnd",
                json!({"context": "ctx-0", "download": "d2", "navigation": "n1", "status": "canceled", "filepath": null}),
            ),
        ];
        for (method, params) in events {
            route(&event(method, params), &client, &state).await;
        }
        let downloads = state.downloads.lock().await.since(0);
        let first = &downloads[0];
        assert_eq!(
            (first.state, first.tab_id.as_deref()),
            (DownloadStatus::Completed, Some("tab-1"))
        );
        assert_eq!(
            (first.path.as_deref(), first.received_bytes),
            (Some(file.as_path()), Some(8))
        );
        assert_eq!(downloads[1].state, DownloadStatus::Canceled);
    }

    /// BID-11: A top-level navigation starts a new bucket and updates the URL.
    #[tokio::test]
    async fn navigation_splits_network_and_updates_url() {
        let mock = MockBidi::start(MockBidiBehavior::Ready).await.unwrap();
        let (client, state) = one_tab(&mock).await;
        let request = |id: &str| json!({"context": "ctx-0", "request": {"request": id, "url": "http://a/", "method": "GET"}});
        route(
            &event("network.beforeRequestSent", request("R1")),
            &client,
            &state,
        )
        .await;
        let nav = json!({"context": "ctx-0", "url": "http://b/", "navigation": "n"});
        route(
            &event("browsingContext.navigationStarted", nav),
            &client,
            &state,
        )
        .await;
        route(
            &event("network.beforeRequestSent", request("R2")),
            &client,
            &state,
        )
        .await;
        let tab = read(&state.tabs).tabs["tab-1"].clone();
        assert_eq!(tab.url, "http://b/");
        let network = tab.network.lock().await;
        assert_eq!(network.iter_current().len(), 1);
        assert_eq!(network.iter_all().len(), 2);
    }

    /// CON-01 (Firefox): a committed cross-site navigation clears the tab's
    /// console; a reload of the same site keeps it.
    #[tokio::test]
    async fn committed_cross_site_navigation_clears_console() {
        // Given tab-1 on github.com with one console message
        let mock = MockBidi::start(MockBidiBehavior::Ready).await.unwrap();
        let (client, state) = one_tab(&mock).await;
        let committed = fixture(
            include_str!("../../../tests/fixtures/bidi/event-navigation-committed.json"),
            "ctx-0",
        );
        route(
            &event("browsingContext.navigationCommitted", committed.clone()),
            &client,
            &state,
        )
        .await;
        let probe = fixture(
            include_str!("../../../tests/fixtures/bidi/event-log-console-probe.json"),
            "ctx-0",
        );
        route(&event("log.entryAdded", probe), &client, &state).await;
        let log = read(&state.tabs).tabs["tab-1"].log.clone();
        // When the same site commits again, the message stays
        route(
            &event("browsingContext.navigationCommitted", committed.clone()),
            &client,
            &state,
        )
        .await;
        assert_eq!(log.lock().await.messages.len(), 1);
        // When another site commits, the console is cleared and the URL is final
        let mut elsewhere = committed;
        elsewhere["url"] = json!("https://example.com/");
        route(
            &event("browsingContext.navigationCommitted", elsewhere),
            &client,
            &state,
        )
        .await;
        assert!(log.lock().await.messages.is_empty());
        assert_eq!(read(&state.tabs).tabs["tab-1"].url, "https://example.com/");
    }

    /// BID-20: BiDi network events become the neutral request record.
    #[test]
    fn network_events_become_request_record() {
        let mut network = crate::handler::NetworkState::default();
        let sent = fixture(
            include_str!("../../../tests/fixtures/bidi/event-before-request-sent.json"),
            "c",
        );
        let done = fixture(
            include_str!("../../../tests/fixtures/bidi/event-response-completed.json"),
            "c",
        );
        on_network("network.beforeRequestSent", &sent, &mut network);
        on_network("network.responseCompleted", &done, &mut network);
        let all = network.iter_all();
        let request = all.values().next().unwrap();
        assert_eq!(
            (request.method.as_str(), request.response_status),
            ("GET", Some(200))
        );
        assert!(request.url.ends_with("/api"));
        let headers = request.request_headers.as_ref().unwrap();
        assert!(headers.get("Cookie").is_some(), "{headers}");
    }
}
