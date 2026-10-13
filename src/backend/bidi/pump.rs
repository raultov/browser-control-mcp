//! The single event pump of a Firefox instance (specs D34): one global
//! subscription, every event routed by browsing context into the state of
//! the tab that owns it (iframes included).

use super::session::FirefoxTabs;
use crate::handler::cdp_domains::log::ConsoleMessage;
use crate::handler::{CustomEvent, NetworkRequest};
use crate::util::json::{opt_string, str_field, string_or_empty};
use crate::util::sync::{read, write};
use bidi_browser_lite::{BrowserClient, WsResponse};
use serde_json::Value;
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
    "log.entryAdded",
    "network.beforeRequestSent",
    "network.responseStarted",
    "network.responseCompleted",
    "network.fetchError",
];

/// What the pump shares with its session.
#[derive(Clone)]
pub(crate) struct PumpState {
    pub(crate) tabs: Arc<RwLock<FirefoxTabs>>,
    pub(crate) lagged: Arc<AtomicU64>,
    pub(crate) custom_modules: Arc<RwLock<Vec<String>>>,
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

/// Routes one event into the owning tab's state.
pub(crate) async fn route(event: &WsResponse, client: &BrowserClient, state: &PumpState) {
    let (Some(method), Some(params)) = (event.method.as_deref(), event.params.as_ref()) else {
        return;
    };
    match method {
        "browsingContext.contextCreated" => on_context_created(params, client, state).await,
        "browsingContext.contextDestroyed" => {
            if let Some(context) = str_field(params, "context") {
                write(&state.tabs).remove_context(context);
            }
        }
        "browsingContext.navigationStarted"
        | "browsingContext.navigationCommitted"
        | "browsingContext.load" => on_navigation(method, params, state).await,
        "log.entryAdded" => {
            if let Some(log) = tab_part(state, params, |t| t.log.clone()) {
                log.lock().await.push_message(console_message(params));
            }
        }
        _ if method.starts_with("network.") => {
            if let Some(network) = tab_part(state, params, |t| t.network.clone()) {
                on_network(method, params, &mut *network.lock().await);
            }
        }
        _ => on_custom(method, params, state).await,
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
        "browsingContext.navigationStarted" => network.lock().await.split_after_navigation(),
        // The final URL (after redirects) of a new document; fragment and
        // history navigations have their own events.
        "browsingContext.navigationCommitted" => {
            if let Some(url) = str_field(params, "url") {
                log.lock().await.on_document_committed(url);
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
            opt_string(params, "method").unwrap_or_else(|| string_or_empty(params, "level"))
        },
        text: string_or_empty(params, "text"),
        timestamp: params
            .get("timestamp")
            .and_then(Value::as_f64)
            .unwrap_or(0.0),
        url: frame.and_then(|f| opt_string(f, "url")),
        line_number: frame
            .and_then(|f| f.get("lineNumber"))
            .and_then(Value::as_i64),
    }
}

/// BiDi headers `[{name, value: {type, value}}]` as `{name: value}`.
fn headers(list: Option<&Value>) -> Option<Value> {
    let list = list?.as_array()?;
    let map = list
        .iter()
        .filter_map(|h| {
            let name = str_field(h, "name")?;
            let value = h
                .get("value")
                .and_then(|v| str_field(v, "value"))
                .unwrap_or_default();
            Some((name.to_string(), Value::String(value.to_string())))
        })
        .collect();
    Some(Value::Object(map))
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
        network.insert_request(
            id.to_string(),
            NetworkRequest {
                url: string_or_empty(request, "url"),
                method: string_or_empty(request, "method"),
                resource_type: opt_string(request, "destination").filter(|d| !d.is_empty()),
                request_headers: headers(request.get("headers")),
                request_post_data: None,
                response_status: None,
                response_status_text: None,
                response_headers: None,
                response_body: None,
                redirects: Vec::new(),
            },
        );
        return;
    }
    let Some(entry) = network.find_request_mut(id) else {
        return;
    };
    if method == "network.fetchError" {
        entry.response_status_text = opt_string(params, "errorText");
        return;
    }
    if let Some(response) = params.get("response") {
        entry.response_status = response.get("status").and_then(Value::as_i64);
        entry.response_status_text = opt_string(response, "statusText");
        entry.response_headers = headers(response.get("headers"));
    }
}

/// Events of modules the agent subscribed to, kept per tab.
async fn on_custom(method: &str, params: &Value, state: &PumpState) {
    let module = method.split('.').next().unwrap_or_default();
    if !read(&state.custom_modules)
        .iter()
        .any(|m| m == module || m == method)
    {
        return;
    }
    let Some(custom) = tab_part(state, params, |t| t.custom.clone()) else {
        return;
    };
    let mut custom = custom.lock().await;
    if custom.events.len() >= crate::handler::cdp_domains::custom::MAX_CUSTOM_EVENTS {
        custom.events.pop_front();
    }
    custom.events.push_back(CustomEvent {
        method: method.to_string(),
        params: params.clone(),
        timestamp: chrono::Utc::now().to_rfc3339(),
    });
}

#[cfg(test)]
mod tests {
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
