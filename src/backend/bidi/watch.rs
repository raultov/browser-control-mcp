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
            let data = &resource["data"];
            if !matches!(data.get("opCode").and_then(Value::as_u64), Some(1 | 2)) {
                return;
            }
            let frame = WebSocketFrame {
                url: network.ws_connections.get(&id).cloned().unwrap_or_default(),
                payload_data: text(data, "payload").unwrap_or_default(),
                is_sent: kind == "frameSent",
            };
            network.push_ws_frame(id, frame);
        }
        _ => {}
    }
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
        text: error.get("errorMessage")?.as_str()?.to_string(),
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
        assert_eq!(
            network.ws_connections.get("38").unwrap(),
            "ws://127.0.0.1:8808/"
        );
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
