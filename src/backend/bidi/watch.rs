//! What WebDriver BiDi does not report on Firefox, read from the RDP watcher
//! of each tab's debugger: WebSocket frames, page errors (script errors,
//! cookie and security warnings), CSS warnings and platform messages.
//!
//! They land in the same per-tab buffers and shapes as Chrome's, so
//! `get_network_logs` and `get_console_logs` answer alike on both browsers.
//! A message BiDi also reports (script errors, some warnings) is kept once:
//! the watcher's copy, which carries the better labels.

use crate::handler::NetworkRequest;
use crate::handler::NetworkState;
use crate::handler::WebSocketFrame;
use crate::handler::WsClose;
use crate::handler::cdp_domains::event_pump::ListenerHandles;
use crate::handler::cdp_domains::log::{Channel, ConsoleMessage, LogState};
use bidi_browser_lite::Debugger;
use bidi_browser_lite::bidi_lite::rdp::protocol::RdpPacket;
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
    handles.push(watch_websockets(debugger, buffers.network.clone()));
    for &kind in RESOURCE_TYPES.iter().filter(|&&kind| kind != WEBSOCKET) {
        let mut packets = debugger.on_resource(kind);
        let log = buffers.log.clone();
        handles.push(tokio::spawn(async move {
            while let Some(packet) = packets.next().await {
                if let Ok(packet) = packet {
                    log_resources(kind, &packet, &log).await;
                }
            }
        }));
    }
    debugger.watch_resources(RESOURCE_TYPES).await?;
    Ok(handles)
}

/// Puts the console messages of a packet of `kind` resources in `log`.
async fn log_resources(kind: &str, packet: &RdpPacket, log: &Mutex<LogState>) {
    for resource in parse_resources(packet, kind) {
        if let Some(message) = console_message(kind, &resource) {
            log.lock().await.push_deduplicated(message);
        }
    }
}

/// The watcher resource of WebSocket connections and frames.
const WEBSOCKET: &str = "websocket";

/// Follows the WebSocket resources of the tab's document and of its
/// frames: every iframe, same-origin too, is a target of its own whose
/// sockets only it reports (#163). Firefox reports no close for the
/// sockets of a frame that goes away (removed, or navigated): they are
/// closed when its target is destroyed. Its target forms come from the
/// watcher, on another stream; the resources are read first, so a frame's
/// last frames are recorded before it goes.
fn watch_websockets(
    debugger: &Debugger,
    network: Arc<Mutex<NetworkState>>,
) -> tokio::task::JoinHandle<()> {
    let mut resources = debugger.on_any_resource(WEBSOCKET);
    let mut targets = debugger.client().on_actor(debugger.watcher());
    tokio::spawn(async move {
        let mut owners = SocketOwners::default();
        loop {
            tokio::select! {
                biased;
                Some(packet) = resources.next() => {
                    let Ok(packet) = packet else { continue };
                    let mut network = network.lock().await;
                    for resource in parse_resources(&packet, WEBSOCKET) {
                        let id = on_websocket(&mut network, &resource);
                        if let (Some(id), Some(from)) = (id, packet.from.as_deref()) {
                            owners.own(from, id);
                        }
                    }
                }
                Some(packet) = targets.next() => {
                    let Ok(packet) = packet else { continue };
                    owners.on_target_form(&packet.body, &mut *network.lock().await);
                }
                else => break,
            }
        }
    })
}

/// Why the sockets of a frame that went away are closed (#163).
pub(crate) const FRAME_GONE: &str = "the frame was removed or navigated away";

/// How many targets [`SocketOwners`] remembers, and how many connections
/// each.
const OWNERS: usize = 64;
const OWNED: usize = 256;

/// The WebSocket connections of each target of the tab, and which targets
/// are frames, to close a frame's sockets when it goes (#163). Bounded:
/// the oldest targets and connections are forgotten first.
#[derive(Default)]
pub(crate) struct SocketOwners {
    targets: crate::util::bounded::BoundedMap<String, Owned, OWNERS>,
}

/// What [`SocketOwners`] knows of one target.
#[derive(Default)]
struct Owned {
    /// Whether its watcher form says it is a frame, not the tab's document.
    frame: bool,
    connections: std::collections::VecDeque<String>,
}

impl SocketOwners {
    /// Records that the target `actor` reported the connection `id`.
    pub(crate) fn own(&mut self, actor: &str, id: String) {
        self.update(actor, |owned| {
            if !owned.connections.contains(&id) {
                owned.connections.push_back(id);
                if owned.connections.len() > OWNED {
                    owned.connections.pop_front();
                }
            }
        });
    }

    /// Changes what is known of `actor`, known from now on.
    fn update(&mut self, actor: &str, change: impl FnOnce(&mut Owned)) {
        if !self.targets.contains_key(actor) {
            self.targets.insert(actor.to_string(), Owned::default());
        }
        if let Some(owned) = self.targets.get_mut(actor) {
            change(owned);
        }
    }

    /// Applies a watcher `target-available-form` (a frame is announced)
    /// or `target-destroyed-form` (the sockets of a frame that went are
    /// closed, with [`FRAME_GONE`]).
    pub(crate) fn on_target_form(&mut self, body: &Value, network: &mut NetworkState) {
        let target = &body["target"];
        let Some(actor) = target.get("actor").and_then(Value::as_str) else {
            return;
        };
        let frame = target.get("isTopLevelTarget").and_then(Value::as_bool) == Some(false);
        match body.get("type").and_then(Value::as_str) {
            Some("target-available-form") if frame => {
                self.update(actor, |owned| owned.frame = true)
            }
            Some("target-destroyed-form") => {
                let Some(owned) = self.targets.remove(&actor.to_string()) else {
                    return;
                };
                if !(owned.frame || frame) {
                    return;
                }
                for id in &owned.connections {
                    let close = WsClose {
                        at: crate::handler::now_ms(),
                        code: None,
                        reason: Some(FRAME_GONE.to_string()),
                        was_clean: None,
                    };
                    network.close_ws_connection(id, close);
                }
            }
            _ => {}
        }
    }
}

/// A `websocket` resource: a connection opening, or a data frame (control
/// frames are left out, as Chrome does). The connection id is Firefox's
/// `httpChannelId`. Answers the connection it opened or carried a frame
/// of.
pub(crate) fn on_websocket(network: &mut NetworkState, resource: &Value) -> Option<String> {
    let id = resource.get("httpChannelId").map(|id| match id {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    })?;
    let text = |v: &Value, key: &str| v.get(key).and_then(Value::as_str).map(str::to_string);
    match resource.get("wsMessageType").and_then(Value::as_str) {
        Some("webSocketOpened") => {
            let url = text(resource, "effectiveURI").unwrap_or_default();
            network.insert_ws_connection(id.clone(), url);
            Some(id)
        }
        Some(kind @ ("frameSent" | "frameReceived")) => {
            // Firefox may report frames before the socket opened, or only
            // the close frame of one that fails before it opens (#158).
            network.insert_ws_connection(id.clone(), String::new());
            let data = &resource["data"];
            if !matches!(data.get("opCode").and_then(Value::as_u64), Some(1 | 2)) {
                return Some(id);
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
            network.push_ws_frame(id.clone(), frame);
            Some(id)
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
            None
        }
        _ => None,
    }
}

/// The connection a `webSocketClosed` for `id` closes. Firefox gives the
/// id 0 to a socket that failed before it opened, which it reports only by
/// the close frame it sent, if any (#158): that is the newest connection
/// without a URL (it gets the URL of the newest accepted handshake no
/// connection claims), or else one made for the newest WebSocket handshake
/// of the log that no connection claims, under its request id: an accepted
/// one (`101`), else one the server refused or that failed, whose
/// connection then says why (#164).
fn closed_connection(network: &mut NetworkState, id: String) -> Option<String> {
    if id != NEVER_OPENED {
        return Some(id);
    }
    let Unclaimed { accepted, refused } = unclaimed_handshakes(network);
    let (id, handshake) = match newest_without_url(network) {
        // It sent a frame: its handshake was accepted.
        Some(unopened) => (unopened, accepted),
        None => {
            let handshake = accepted.or(refused)?;
            (handshake.request.clone(), Some(handshake))
        }
    };
    let (url, refusal) = handshake.map_or_else(Default::default, |h| (h.url, h.refusal));
    network.insert_ws_connection(id.clone(), url);
    if let Some(refusal) = refusal {
        network.fail_ws_connection(&id, &refusal);
    }
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

/// A WebSocket handshake of the log.
struct Handshake {
    /// Its request id.
    request: String,
    /// Its `ws(s)` URL.
    url: String,
    /// Why no socket opened: the server's answer when it was not `101`,
    /// or the network error; `None` for an accepted handshake.
    refusal: Option<String>,
}

/// The newest handshakes of the log no connection claims.
#[derive(Default)]
struct Unclaimed {
    accepted: Option<Handshake>,
    refused: Option<Handshake>,
}

/// The newest WebSocket handshakes of the log that no connection claims,
/// accepted (`101`) and refused or failed: each connection claims the
/// oldest handshake to its URL, a refused one if it was refused.
fn unclaimed_handshakes(network: &NetworkState) -> Unclaimed {
    let mut claims: std::collections::HashMap<(String, bool), usize> =
        std::collections::HashMap::new();
    for connection in network.ws_connections.values() {
        let error = connection.error.as_deref().unwrap_or_default();
        let refused = error.starts_with(REFUSED) || error.starts_with(FAILED);
        *claims.entry((connection.url.clone(), refused)).or_default() += 1;
    }
    let mut unclaimed = Unclaimed::default();
    for bucket in network.navigations.iter().rev() {
        for id in &bucket.order {
            let Some(handshake) = bucket.requests.get(id).and_then(|r| handshake(id, r)) else {
                continue;
            };
            let refused = handshake.refusal.is_some();
            match claims.get_mut(&(handshake.url.clone(), refused)) {
                Some(count) if *count > 0 => *count -= 1,
                _ if !refused => unclaimed.accepted = Some(handshake),
                _ => unclaimed.refused = Some(handshake),
            }
        }
    }
    unclaimed
}

/// How the error of a connection whose handshake the server refused starts.
const REFUSED: &str = "the handshake was refused: ";

/// How the error of a connection whose handshake failed starts.
const FAILED: &str = "the handshake failed: ";

/// `request` (logged as `id`) as a WebSocket handshake, when it is one
/// that ended: Firefox logs it as `http(s)`, answered `101` when accepted.
/// One answered otherwise, or that failed, is told by the headers only a
/// WebSocket handshake sends (#164).
fn handshake(id: &str, request: &NetworkRequest) -> Option<Handshake> {
    let refusal = match (request.response_status, &request.error) {
        (Some(101), _) => None,
        _ if !sends_websocket_headers(request) => return None,
        (Some(status), _) => {
            let text = request.response_status_text.as_deref().unwrap_or_default();
            let answer = format!("HTTP {status} {text}");
            Some(format!("{REFUSED}{}", answer.trim_end()))
        }
        (None, Some(error)) => Some(format!("{FAILED}{error}")),
        (None, None) => return None,
    };
    let rest = request
        .url
        .strip_prefix("http")
        .filter(|r| r.starts_with("s://") || r.starts_with("://"))?;
    Some(Handshake {
        request: id.to_string(),
        url: format!("ws{rest}"),
        refusal,
    })
}

/// Whether `request` carries the headers of a WebSocket handshake
/// (`Sec-WebSocket-Key`, or `Sec-Fetch-Mode: websocket`).
fn sends_websocket_headers(request: &NetworkRequest) -> bool {
    let headers = request.request_headers.as_ref().and_then(Value::as_object);
    headers.into_iter().flatten().any(|(name, value)| {
        name.eq_ignore_ascii_case("sec-websocket-key")
            || (name.eq_ignore_ascii_case("sec-fetch-mode")
                && value
                    .as_str()
                    .is_some_and(|v| v.eq_ignore_ascii_case("websocket")))
    })
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

    /// A WebSocket handshake the server refused (`status`) or that failed
    /// (`error`), with the request headers Firefox 159 sends.
    fn refused_handshake(path: &str, status: Option<i64>, error: Option<&str>) -> NetworkRequest {
        NetworkRequest {
            url: format!("http://127.0.0.1:8808{path}"),
            method: "GET".into(),
            request_headers: Some(json!({
                "Sec-Fetch-Mode": "websocket",
                "Sec-WebSocket-Key": "neQBoQ+J8sr/A+lw+wFhYA==",
                "Sec-WebSocket-Version": "13",
            })),
            response_status: status,
            response_status_text: status.map(|_| "Forbidden".to_string()),
            error: error.map(str::to_string),
            ..Default::default()
        }
    }

    /// #164: a socket whose handshake the server refused (HTTP 403) is
    /// closed by Firefox with the id 0 and no `101` in the log: it is
    /// listed as closed under its handshake's request id, with its `ws`
    /// URL and the refusal; a plain request is never taken for one.
    #[test]
    fn sockets_refused_at_the_handshake_are_closed() {
        // Given a plain 403 request, then a refused WebSocket handshake
        let mut network = NetworkState::default();
        let plain = NetworkRequest {
            url: "http://127.0.0.1:8808/page".into(),
            response_status: Some(403),
            ..Default::default()
        };
        network.insert_request("r1".into(), plain);
        let refused = refused_handshake("/deny?n=1", Some(403), None);
        network.insert_request("r2".into(), refused);
        let failed = json!({"httpChannelId": 0, "wsMessageType": "webSocketClosed", "code": 1006, "wasClean": false});
        // When the socket closes before it opened
        on_websocket(&mut network, &failed);
        // Then it is closed under the handshake's id, saying why
        let denied = &network.ws_connections["r2"];
        assert_eq!(denied.url, "ws://127.0.0.1:8808/deny?n=1");
        assert!(denied.closed_at.is_some());
        assert_eq!(
            denied.error.as_deref(),
            Some("the handshake was refused: HTTP 403 Forbidden")
        );
        // And another such close, with no handshake left, records nothing
        on_websocket(&mut network, &failed);
        assert_eq!(network.ws_connections.len(), 1);
    }

    /// #164: an accepted handshake no connection claims is still preferred
    /// to a refused one; a handshake that failed without an answer gives
    /// its network error.
    #[test]
    fn accepted_handshakes_come_before_refused_ones() {
        // Given a handshake that failed, then an accepted one
        let mut network = handshakes(&[]);
        let unanswered = refused_handshake("/down", None, Some("NS_ERROR_CONNECTION_REFUSED"));
        network.insert_request("r1".into(), unanswered);
        let accepted = handshakes(&[("r2", "/up")]).find_request_mut("r2").cloned();
        network.insert_request("r2".into(), accepted.unwrap());
        let failed = json!({"httpChannelId": 0, "wsMessageType": "webSocketClosed", "code": 1006});
        // When two sockets close before they opened
        on_websocket(&mut network, &failed);
        on_websocket(&mut network, &failed);
        // Then the accepted handshake is claimed first, then the failed one
        assert_eq!(network.ws_connections["r2"].url, "ws://127.0.0.1:8808/up");
        assert_eq!(network.ws_connections["r2"].error, None);
        let down = &network.ws_connections["r1"];
        assert_eq!(down.url, "ws://127.0.0.1:8808/down");
        assert_eq!(
            down.error.as_deref(),
            Some("the handshake failed: NS_ERROR_CONNECTION_REFUSED")
        );
    }

    /// #163: the sockets of a frame (same-origin too) are closed when its
    /// target is destroyed, Firefox reporting no close for them; those of
    /// the tab's document, and a frame's socket that closed already, keep
    /// what they had.
    #[test]
    fn sockets_of_a_frame_that_goes_are_closed() {
        // Given a socket of the document and two of a frame, one closed
        let mut network = NetworkState::default();
        let mut owners = SocketOwners::default();
        let frame = json!({"type": "target-available-form",
            "target": {"actor": "frame7", "isTopLevelTarget": false}});
        owners.on_target_form(&frame, &mut network);
        for (actor, id, path) in [
            ("top1", "41", "/main"),
            ("frame7", "43", "/f"),
            ("frame7", "44", "/g"),
        ] {
            let opened = json!({"httpChannelId": id, "wsMessageType": "webSocketOpened",
                "effectiveURI": format!("ws://127.0.0.1:8808{path}")});
            let id = on_websocket(&mut network, &opened).unwrap();
            owners.own(actor, id);
        }
        let closed =
            json!({"httpChannelId": "44", "wsMessageType": "webSocketClosed", "code": 1000});
        assert_eq!(on_websocket(&mut network, &closed), None);
        // When the frame is removed, then the document navigates
        for actor in ["frame7", "top1"] {
            let destroyed = json!({"type": "target-destroyed-form", "target": {"actor": actor}});
            owners.on_target_form(&destroyed, &mut network);
        }
        // Then the frame's open socket is closed, saying why
        let gone = &network.ws_connections["43"];
        assert!(gone.closed_at.is_some());
        assert_eq!(gone.close_reason.as_deref(), Some(FRAME_GONE));
        // And the others keep what they had
        assert_eq!(network.ws_connections["44"].close_code, Some(1000));
        assert_eq!(network.ws_connections["44"].close_reason, None);
        assert!(network.ws_connections["41"].closed_at.is_none());
    }

    /// #163: a frame whose resources arrive before its announcement is
    /// still told a frame by its destroyed form; what is remembered stays
    /// bounded.
    #[test]
    fn socket_owners_are_bounded() {
        // Given a frame's socket recorded before the frame was announced
        let mut network = NetworkState::default();
        let mut owners = SocketOwners::default();
        network.insert_ws_connection("43".into(), "ws://a/".into());
        owners.own("frame9", "43".into());
        // When its destroyed form says it was a frame
        let destroyed = json!({"type": "target-destroyed-form",
            "target": {"actor": "frame9", "isTopLevelTarget": false}});
        owners.on_target_form(&destroyed, &mut network);
        // Then its socket is closed
        assert!(network.ws_connections["43"].closed_at.is_some());
        // And many targets and connections stay under the caps
        for n in 0..(OWNERS * 2) {
            for c in 0..(OWNED + 10) {
                owners.own(&format!("t{n}"), format!("{n}-{c}"));
            }
        }
        assert_eq!(owners.targets.len(), OWNERS);
        assert!(
            owners
                .targets
                .values()
                .all(|o| o.connections.len() == OWNED)
        );
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
