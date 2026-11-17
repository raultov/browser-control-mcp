//! What WebDriver BiDi does not report on Firefox, read from the RDP watcher
//! of each tab's debugger: WebSocket frames, page errors (script errors,
//! cookie and security warnings), CSS warnings and platform messages.
//!
//! They land in the same per-tab buffers and shapes as Chrome's, so
//! `get_network_logs` and `get_console_logs` answer alike on both browsers.
//! A message BiDi also reports (script errors, some warnings) is kept once:
//! the watcher's copy, which carries the better labels.

use crate::handler::NetworkState;
use crate::handler::WebSocketFrame;
use crate::handler::WsClose;
use crate::handler::cdp_domains::event_pump::ListenerHandles;
use crate::handler::cdp_domains::log::{Channel, ConsoleMessage, LogState};
use bidi_browser_lite::Debugger;
use bidi_browser_lite::bidi_lite::rdp::resources::parse_resources;
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_stream::StreamExt;

/// The watcher resources read per tab.
pub(crate) const RESOURCE_TYPES: &[&str] = &[
    "websocket",
    "error-message",
    "css-message",
    "platform-message",
];

/// Where a tab's watched resources go.
#[derive(Clone)]
pub(crate) struct TabBuffers {
    /// WebSocket connections and frames.
    pub(crate) network: Arc<Mutex<NetworkState>>,
    /// Console messages.
    pub(crate) log: Arc<Mutex<LogState>>,
}

/// Watches [`RESOURCE_TYPES`] on `debugger`, routing them into `buffers`.
/// The streams exist before `watchResources` is sent: Firefox announces the
/// resources it already has right away.
pub(crate) async fn watch(
    debugger: &Debugger,
    buffers: &TabBuffers,
) -> Result<ListenerHandles, bidi_browser_lite::BidiError> {
    let mut handles = ListenerHandles::default();
    for &kind in RESOURCE_TYPES {
        let mut packets = debugger.on_resource(kind);
        let buffers = buffers.clone();
        handles.push(tokio::spawn(async move {
            while let Some(packet) = packets.next().await {
                let Ok(packet) = packet else { continue };
                for resource in parse_resources(&packet, kind) {
                    apply(kind, &resource, &buffers).await;
                }
            }
        }));
    }
    debugger.watch_resources(RESOURCE_TYPES).await?;
    Ok(handles)
}

/// Routes one resource of `kind` into the tab's buffers.
pub(crate) async fn apply(kind: &str, resource: &Value, buffers: &TabBuffers) {
    if kind == "websocket" {
        on_websocket(&mut *buffers.network.lock().await, resource);
    } else if let Some(message) = console_message(kind, resource) {
        buffers.log.lock().await.push_deduplicated(message);
    }
}

/// A `websocket` resource: a connection opening, or a data frame (control
/// frames are left out, as Chrome does). The connection id is Firefox's
/// `httpChannelId`.
pub(crate) fn on_websocket(network: &mut NetworkState, resource: &Value) {
    let Some(id) = resource.get("httpChannelId").map(|id| match id {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }) else {
        return;
    };
    let text = |v: &Value, key: &str| v.get(key).and_then(Value::as_str).map(str::to_string);
    match resource.get("wsMessageType").and_then(Value::as_str) {
        Some("webSocketOpened") => {
            let url = text(resource, "effectiveURI").unwrap_or_default();
            network.insert_ws_connection(id, url);
        }
        Some(kind @ ("frameSent" | "frameReceived")) => {
            // Firefox may report frames before the socket opened, or only
            // the close frame of one that fails before it opens (#158).
            network.insert_ws_connection(id.clone(), String::new());
            let data = &resource["data"];
            if !matches!(data.get("opCode").and_then(Value::as_u64), Some(1 | 2)) {
                return;
            }
            // Large payloads come as a long string grip: its first part,
            // marked with the full length (as `evaluate_on_call_frame` does).
            let payload = match data.get("payload") {
                Some(Value::String(s)) => s.clone(),
                Some(grip) => match super::grip::to_json(grip) {
                    Value::String(s) => s,
                    _ => String::new(),
                },
                None => String::new(),
            };
            let url = network.ws_url(&id);
            let frame = WebSocketFrame::new(url, payload, kind == "frameSent");
            network.push_ws_frame(id, frame);
        }
        // #153: `code`, `reason` and `wasClean` (Firefox 157), each optional.
        Some("webSocketClosed") => {
            let field = |key: &str| resource.get(key);
            let close = WsClose {
                at: crate::handler::now_ms(),
                code: field("code")
                    .and_then(Value::as_u64)
                    .and_then(|code| u16::try_from(code).ok()),
                reason: field("reason")
                    .and_then(Value::as_str)
                    .filter(|reason| !reason.is_empty())
                    .map(str::to_string),
                was_clean: field("wasClean").and_then(Value::as_bool),
            };
            if let Some(id) = closed_connection(network, id) {
                network.close_ws_connection(&id, close);
            }
        }
        _ => {}
    }
}

/// The connection a `webSocketClosed` for `id` closes. Firefox gives the
/// id 0 to a socket that failed before it opened, which it reports only by
/// the close frame it sent, if any (#158): that is the newest connection
/// without a URL, or else one made for the newest WebSocket handshake of
/// the log that no connection claims, under its request id. A connection
/// without a URL gets that handshake's.
fn closed_connection(network: &mut NetworkState, id: String) -> Option<String> {
    if id != NEVER_OPENED {
        return Some(id);
    }
    let handshake = unclaimed_handshake(network);
    let id = match (newest_without_url(network), &handshake) {
        (Some(unopened), _) => unopened,
        (None, Some((request, _))) => request.clone(),
        (None, None) => return None,
    };
    let url = handshake.map(|(_, url)| url).unwrap_or_default();
    network.insert_ws_connection(id.clone(), url);
    Some(id)
}

/// The `httpChannelId` Firefox gives the close of a socket that never
/// opened.
const NEVER_OPENED: &str = "0";

/// The newest connection still open whose URL is not known: one Firefox
/// reported by its frames only.
fn newest_without_url(network: &NetworkState) -> Option<String> {
    let connections = &network.ws_connections;
    connections
        .keys_oldest_first()
        .rev()
        .find(|id| {
            connections
                .get(*id)
                .is_some_and(|c| c.url.is_empty() && c.closed_at.is_none())
        })
        .cloned()
}

/// The newest WebSocket handshake (an `http(s)` request answered `101`)
/// of the log that no connection claims, as `(request id, ws(s) URL)`:
/// each connection claims the oldest handshake to its URL.
fn unclaimed_handshake(network: &NetworkState) -> Option<(String, String)> {
    let mut claims: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for connection in network.ws_connections.values() {
        *claims.entry(connection.url.as_str()).or_default() += 1;
    }
    let mut unclaimed = None;
    for bucket in network.navigations.iter().rev() {
        for id in &bucket.order {
            let Some(request) = bucket.requests.get(id) else {
                continue;
            };
            let Some(url) = websocket_url(request) else {
                continue;
            };
            match claims.get_mut(url.as_str()) {
                Some(count) if *count > 0 => *count -= 1,
                _ => unclaimed = Some((id.clone(), url)),
            }
        }
    }
    unclaimed
}

/// The `ws(s)` URL of `request` when it is a WebSocket handshake: Firefox
/// logs it as `http(s)`, answered `101`.
fn websocket_url(request: &crate::handler::NetworkRequest) -> Option<String> {
    if request.response_status != Some(101) {
        return None;
    }
    let url = &request.url;
    let rest = url
        .strip_prefix("http")
        .filter(|r| r.starts_with("s://") || r.starts_with("://"))?;
    Some(format!("ws{rest}"))
}

/// An `error-message`, `css-message` or `platform-message` resource as a
/// console message, with a source label that says what it is.
pub(crate) fn console_message(kind: &str, resource: &Value) -> Option<ConsoleMessage> {
    if kind == "platform-message" {
        return Some(ConsoleMessage {
            source: "other".into(),
            level: "info".into(),
            text: resource.get("message")?.as_str()?.to_string(),
            timestamp: resource
                .get("timeStamp")
                .and_then(Value::as_f64)
                .unwrap_or(0.0),
            url: None,
            line_number: None,
            channel: Channel::Watcher,
        });
    }
    let error = resource.get("pageError")?;
    let flag = |key: &str| error.get(key).and_then(Value::as_bool).unwrap_or(false);
    let level = if flag("error") {
        "error"
    } else if flag("warning") || kind == "css-message" {
        "warning"
    } else {
        "info"
    };
    let category = error
        .get("category")
        .and_then(Value::as_str)
        .unwrap_or_default();
    Some(ConsoleMessage {
        source: source_label(kind, category).into(),
        level: level.into(),
        text: error_text(error)?,
        timestamp: error
            .get("timeStamp")
            .and_then(Value::as_f64)
            .unwrap_or(0.0),
        url: error
            .get("sourceName")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        // Firefox counts lines from 1; the console reports CDP's 0-based lines.
        line_number: error
            .get("lineNumber")
            .and_then(Value::as_i64)
            .filter(|&l| l > 0)
            .map(|l| l - 1),
        channel: Channel::Watcher,
    })
}

/// A page error's text; an unhandled rejection's starts with
/// `Uncaught (in promise)`, as on Chrome (ISSUE-80).
fn error_text(error: &Value) -> Option<String> {
    let text = error.get("errorMessage")?.as_str()?;
    let rejection = error.get("isPromiseRejection").and_then(Value::as_bool) == Some(true);
    Some(match rejection {
        true => format!("{}{text}", crate::handler::cdp_domains::log::IN_PROMISE),
        false => text.to_string(),
    })
}

/// What a Firefox console category is about: `javascript` (script errors),
/// `css`, `security` (cookies, mixed content, CSP...), `network`, else
/// `other`.
fn source_label(kind: &str, category: &str) -> &'static str {
    let category = category.to_ascii_lowercase();
    if kind == "css-message" || category.starts_with("css") {
        "css"
    } else if category.contains("javascript") {
        "javascript"
    } else if [
        "cookie",
        "mixed content",
        "csp",
        "security",
        "sri",
        "cors",
        "https-only",
    ]
    .iter()
    .any(|k| category.contains(k))
    {
        "security"
    } else if ["network", "image", "media", "xhr"]
        .iter()
        .any(|k| category.contains(k))
    {
        "network"
    } else {
        "other"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bidi_browser_lite::test_support::fixtures;
    use serde_json::json;

    fn resources(fixture: &str) -> Vec<Value> {
        let packet: Value = serde_json::from_str(fixture).unwrap();
        packet["array"][0][1].as_array().unwrap().clone()
    }

    /// FWS-01: frames are recorded in both directions under their
    /// connection, data frames only.
    #[test]
    fn websocket_frames_are_recorded_both_ways() {
        // Given the frames of an echo session (Firefox 157)
        let mut network = NetworkState::default();
        for resource in resources(fixtures::RDP_RESOURCES_WEBSOCKET) {
            on_websocket(&mut network, &resource);
        }
        // Then the connection and its four data frames (not the close) are kept
        assert_eq!(network.ws_url("38"), "ws://127.0.0.1:8808/");
        let frames = network.ws_frames.get("38").unwrap();
        let summary: Vec<(bool, &str)> = frames
            .iter()
            .map(|f| (f.is_sent, f.payload_data.as_str()))
            .collect();
        assert_eq!(
            summary,
            [
                (true, "Hello WebSocket frame text"),
                (false, "echo: Hello WebSocket frame text"),
                (true, "ABCD"),
                (false, "ABCD"),
            ]
        );
        assert!(frames.iter().all(|f| f.url == "ws://127.0.0.1:8808/"));
    }

    /// #153: the `webSocketClosed` resource closes its connection with
    /// the code, reason and clean flag Firefox gives; a later duplicate
    /// keeps the first close, and missing fields stay unset.
    #[test]
    fn websocket_closes_are_recorded() {
        // Given the echo session, which ends with a normal closure
        let mut network = NetworkState::default();
        let started = crate::handler::now_ms();
        for resource in resources(fixtures::RDP_RESOURCES_WEBSOCKET) {
            on_websocket(&mut network, &resource);
        }
        // Then the connection is closed with its code and reason
        let closed = network.ws_connections.get("38").unwrap().clone();
        assert!(
            closed.closed_at.is_some_and(|at| at >= started),
            "{closed:?}"
        );
        assert_eq!(
            (
                closed.close_code,
                closed.close_reason.as_deref(),
                closed.was_clean
            ),
            (Some(1000), Some("Normal closure"), Some(true))
        );
        // And a second close changes nothing
        let again = json!({"httpChannelId": 38, "wsMessageType": "webSocketClosed", "code": 1006});
        on_websocket(&mut network, &again);
        assert_eq!(network.ws_connections.get("38").unwrap(), &closed);
        // And a close without fields only says it closed
        let opened = json!({"httpChannelId": 39, "wsMessageType": "webSocketOpened", "effectiveURI": "ws://a/"});
        on_websocket(&mut network, &opened);
        on_websocket(
            &mut network,
            &json!({"httpChannelId": 39, "wsMessageType": "webSocketClosed"}),
        );
        let bare = network.ws_connections.get("39").unwrap();
        assert!(bare.closed_at.is_some());
        assert_eq!(
            (bare.close_code, bare.close_reason.as_ref(), bare.was_clean),
            (None, None, None)
        );
    }

    /// A log with a WebSocket handshake (`101`) per `(request id, path)`.
    fn handshakes(paths: &[(&str, &str)]) -> NetworkState {
        let mut network = NetworkState::default();
        for (id, path) in paths {
            let request = crate::handler::NetworkRequest {
                url: format!("http://127.0.0.1:8808{path}"),
                method: "GET".into(),
                response_status: Some(101),
                ..Default::default()
            };
            network.insert_request((*id).into(), request);
        }
        network
    }

    /// #158: Firefox reports a socket that fails before it opens only by
    /// the close frame it sent and a close whose `httpChannelId` is 0: it
    /// is closed under the frame's id, with the URL of the handshake no
    /// other connection has. Frames reported before their connection
    /// opened (Firefox 159 does) get its URL.
    #[test]
    fn sockets_failing_before_they_open_are_closed() {
        // Given two handshakes, /live's and /bad's
        let mut network = handshakes(&[("r1", "/live"), ("r2", "/bad")]);
        // When /live receives a frame before it opens, and /bad fails
        for resource in [
            json!({"httpChannelId": 43, "wsMessageType": "frameReceived", "data": {"opCode": 1, "payload": "hi"}}),
            json!({"httpChannelId": 43, "wsMessageType": "webSocketOpened", "effectiveURI": "ws://127.0.0.1:8808/live"}),
            json!({"httpChannelId": 44, "wsMessageType": "frameSent", "data": {"opCode": 8, "payload": "\u{3}\u{ea}"}}),
            json!({"httpChannelId": 0, "wsMessageType": "webSocketClosed", "code": 1006, "reason": "", "wasClean": false}),
        ] {
            on_websocket(&mut network, &resource);
        }
        // Then the early frame has its connection's URL, which stays open
        assert_eq!(network.ws_frames["43"][0].url, "ws://127.0.0.1:8808/live");
        assert!(network.ws_connections["43"].closed_at.is_none());
        // And /bad is closed under its frame's id, with its handshake's URL
        let bad = &network.ws_connections["44"];
        assert!(bad.closed_at.is_some());
        assert_eq!(
            (bad.url.as_str(), bad.close_code, bad.was_clean),
            ("ws://127.0.0.1:8808/bad", Some(1006), Some(false))
        );
        assert!(!network.ws_connections.contains_key("0"));
    }

    /// #158: a socket that failed before it opened without sending a
    /// frame is closed under its handshake's request id; with no handshake
    /// left to claim, nothing is made up.
    #[test]
    fn frameless_failures_take_their_handshake() {
        // Given /live, open, and two handshakes for /live and /gone
        let mut network = handshakes(&[("r1", "/live"), ("r2", "/gone")]);
        network.insert_ws_connection("43".into(), "ws://127.0.0.1:8808/live".into());
        let failed = json!({"httpChannelId": 0, "wsMessageType": "webSocketClosed", "code": 1006});
        // When a socket closes before it opened
        on_websocket(&mut network, &failed);
        // Then /gone is closed under its request id
        let gone = &network.ws_connections["r2"];
        assert_eq!(gone.url, "ws://127.0.0.1:8808/gone");
        assert!(gone.closed_at.is_some());
        // And another such close, with no handshake left, records nothing
        on_websocket(&mut network, &failed);
        assert_eq!(network.ws_connections.len(), 2);
    }

    /// FWS-01: a long payload (a long string grip) keeps its first part
    /// and says how long it was, never an empty payload.
    #[test]
    fn long_websocket_payloads_are_marked() {
        let mut network = NetworkState::default();
        let opened = &resources(fixtures::RDP_RESOURCES_WEBSOCKET)[0];
        on_websocket(&mut network, opened);
        let long = serde_json::json!({"httpChannelId": 38, "wsMessageType": "frameReceived",
            "data": {"opCode": 1, "type": "received", "payload": {"type": "longString", "initial": "abc", "length": 70000, "actor": "a"}}});
        on_websocket(&mut network, &long);
        let frames = network.ws_frames.get("38").unwrap();
        assert_eq!(frames[0].payload_data, "abc…(70000 chars)");
    }

    /// ISSUE-80 (Firefox): an unhandled rejection the watcher reports says
    /// `Uncaught (in promise)`, as on Chrome, and replaces BiDi's copy.
    #[test]
    fn promise_rejections_say_so() {
        let mut errors = resources(fixtures::RDP_RESOURCES_ERROR_MESSAGE);
        errors[0]["pageError"]["isPromiseRejection"] = json!(true);
        errors[0]["pageError"]["errorMessage"] = json!("Error: rej");
        let rejected = console_message("error-message", &errors[0]).unwrap();
        assert_eq!(rejected.text, "Uncaught (in promise) Error: rej");
        let mut log = crate::handler::cdp_domains::log::LogState::default();
        let mut bidi = rejected.clone();
        (bidi.text, bidi.source, bidi.channel) =
            ("Error: rej".into(), "exception".into(), Channel::Primary);
        log.push_deduplicated(bidi);
        log.push_deduplicated(rejected);
        let texts: Vec<&str> = log.messages.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(texts, ["Uncaught (in promise) Error: rej"]);
    }

    /// FWS-03 (F-04): page errors and CSS warnings become console messages
    /// with honest labels and 0-based lines.
    #[test]
    fn page_errors_get_honest_labels() {
        let errors = resources(fixtures::RDP_RESOURCES_ERROR_MESSAGE);
        let script = console_message("error-message", &errors[0]).unwrap();
        assert_eq!(
            (
                script.source.as_str(),
                script.level.as_str(),
                script.line_number
            ),
            ("javascript", "error", Some(42))
        );
        assert_eq!(
            script.text,
            "ReferenceError: nonExistentFunctionCallHere is not defined"
        );
        let cookie = console_message("error-message", &errors[1]).unwrap();
        assert_eq!(cookie.source, "security");
        let css = resources(fixtures::RDP_RESOURCES_CSS_MESSAGE);
        let css = console_message("css-message", &css[0]).unwrap();
        assert_eq!(
            (css.source.as_str(), css.level.as_str()),
            ("css", "warning")
        );
        let platform = resources(fixtures::RDP_RESOURCES_PLATFORM_MESSAGE);
        let platform = console_message("platform-message", &platform[0]).unwrap();
        assert_eq!(
            (platform.source.as_str(), platform.text.as_str()),
            ("other", "Gecko platform message test notification")
        );
    }
}
