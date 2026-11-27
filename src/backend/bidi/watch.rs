//! What WebDriver BiDi does not report on Firefox, read from the RDP watcher
//! of each tab's debugger: WebSocket frames, page errors (script errors,
//! cookie and security warnings), CSS warnings, platform messages and the
//! console of the tab's workers ([`super::workers`]).
//!
//! They land in the same per-tab buffers and shapes as Chrome's, so
//! `get_network_logs` and `get_console_logs` answer alike on both browsers.
//! A message BiDi also reports (script errors, some warnings) is kept once:
//! the watcher's copy, which carries the better labels.

use super::workers::{Worker, WorkerConsole};
use crate::handler::NetworkRequest;
use crate::handler::NetworkState;
use crate::handler::WebSocketFrame;
use crate::handler::WsClose;
use crate::handler::cdp_domains::event_pump::ListenerHandles;
use crate::handler::cdp_domains::log::{Channel, ConsoleMessage, LogState};
use crate::handler::cdp_domains::network::{FRAME_GONE, NAVIGATED_AWAY};
use crate::util::bounded::BoundedMap;
use bidi_browser_lite::Debugger;
use bidi_browser_lite::bidi_lite::rdp::protocol::RdpPacket;
use bidi_browser_lite::bidi_lite::rdp::resources::{WorkerTarget, parse_resources};
use serde_json::Value;
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_stream::StreamExt;

/// The watcher resources read per tab.
pub(crate) const RESOURCE_TYPES: &[&str] = &[
    "websocket",
    ERROR_MESSAGE,
    "css-message",
    "platform-message",
    CONSOLE_MESSAGE,
];

/// Where a tab's watched resources go.
#[derive(Clone)]
pub(crate) struct TabBuffers {
    /// WebSocket connections and frames.
    pub(crate) network: Arc<Mutex<NetworkState>>,
    /// Console messages.
    pub(crate) log: Arc<Mutex<LogState>>,
    /// What the tab's workers' console needs remembered.
    pub(crate) workers: Arc<Mutex<WorkerConsole>>,
}

/// Watches [`RESOURCE_TYPES`] on `debugger`, routing them into `buffers`.
/// The streams exist before `watchResources` is sent: Firefox announces the
/// resources it already has right away. Page errors come from every
/// target of the tab: each iframe, same-origin too, is one, whose errors
/// only it reports, and BiDi's copy does not say an error was an
/// unhandled rejection.
pub(crate) async fn watch(
    debugger: &Debugger,
    buffers: &TabBuffers,
) -> Result<ListenerHandles, bidi_browser_lite::BidiError> {
    let mut handles = ListenerHandles::default();
    handles.push(watch_websockets(debugger, buffers.network.clone()));
    handles.push(watch_worker_console(debugger, buffers));
    let page_feeds = |kind: &&&str| ![WEBSOCKET, CONSOLE_MESSAGE].contains(*kind);
    for &kind in RESOURCE_TYPES.iter().filter(page_feeds) {
        let mut packets = match kind {
            ERROR_MESSAGE => debugger.on_any_resource(kind),
            _ => debugger.on_resource(kind),
        };
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
    // Best effort: a Firefox that cannot follow them keeps the rest.
    if let Err(error) = debugger.watch_workers().await {
        tracing::debug!(%error, "watching the dedicated workers failed");
    }
    if let Err(error) = debugger.watch_service_workers().await {
        tracing::debug!(%error, "watching the service workers failed");
    }
    Ok(handles)
}

/// The watcher resource of console calls: the page's come from BiDi, so
/// only the workers' targets' are read (#213).
const CONSOLE_MESSAGE: &str = "console-message";

/// How many worker targets [`watch_worker_console`] remembers the id of.
const WORKER_TARGETS: usize = 256;

/// Reads the console of the tab's dedicated and service workers from their
/// own targets into the tab's log (see [`super::workers`]). Their target
/// forms come from the watcher, on another stream, read first: a worker's
/// `id`, which a replay under a new actor keeps.
fn watch_worker_console(debugger: &Debugger, buffers: &TabBuffers) -> tokio::task::JoinHandle<()> {
    let mut dedicated = debugger.on_worker_resource(CONSOLE_MESSAGE);
    let mut service = debugger.on_service_worker_resource(CONSOLE_MESSAGE);
    let mut targets = debugger.client().on_actor(debugger.watcher());
    let (log, workers) = (buffers.log.clone(), buffers.workers.clone());
    tokio::spawn(async move {
        let mut ids: BoundedMap<String, String, WORKER_TARGETS> = BoundedMap::default();
        loop {
            let (packet, is_service) = tokio::select! {
                biased;
                Some(packet) = targets.next() => {
                    if let Ok(packet) = packet
                        && packet.r#type.as_deref() == Some("target-available-form")
                        && let Some(worker) = WorkerTarget::from_form(&packet.body)
                    {
                        let id = worker.id.unwrap_or_else(|| worker.actor.clone());
                        ids.insert(worker.actor, id);
                    }
                    continue;
                }
                Some(packet) = dedicated.next() => (packet, false),
                Some(packet) = service.next() => (packet, true),
                else => break,
            };
            let Ok(packet) = packet else { continue };
            let actor = packet.from.clone().unwrap_or_default();
            let id = ids.get(&actor).cloned().unwrap_or_else(|| actor.clone());
            let worker = Worker {
                actor: &actor,
                id: &id,
                service: is_service,
            };
            let messages: Vec<_> = {
                let mut workers = workers.lock().await;
                parse_resources(&packet, CONSOLE_MESSAGE)
                    .iter()
                    .filter_map(|resource| workers.message(worker, resource))
                    .collect()
            };
            let mut log = log.lock().await;
            for message in messages {
                log.push_deduplicated(message);
            }
        }
    })
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

/// The watcher resource of page errors (script errors, rejections, and
/// Firefox's own reports: cookies, security).
const ERROR_MESSAGE: &str = "error-message";

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

/// How many targets with sockets [`SocketOwners`] remembers: as many as
/// the connections the log keeps, so every listed socket keeps its owner
/// (#172).
const OWNERS: usize = crate::handler::MAX_WS_CONNECTIONS;
/// How many connections it remembers of each target.
const OWNED: usize = 256;
/// How many frames it remembers, with sockets or not: a frame is forgotten
/// when it goes, so this only caps a page with that many frames at once.
const FRAMES: usize = 1024;

/// The WebSocket connections of each target of the tab, and which targets
/// are frames, to close a frame's sockets when it goes (#163). Bounded:
/// the oldest targets and connections are forgotten first. The frames are
/// kept apart from the targets with sockets, so frames without sockets
/// never push out those with some (#172).
#[derive(Default)]
pub(crate) struct SocketOwners {
    /// The connections of each target that reported one.
    targets: BoundedMap<String, VecDeque<String>, OWNERS>,
    /// The targets announced as frames, not the tab's document.
    frames: BoundedMap<String, (), FRAMES>,
}

impl SocketOwners {
    /// Records that the target `actor` reported the connection `id`.
    pub(crate) fn own(&mut self, actor: &str, id: String) {
        if !self.targets.contains_key(actor) {
            self.targets.insert(actor.to_string(), VecDeque::new());
        }
        let Some(connections) = self.targets.get_mut(actor) else {
            return;
        };
        if !connections.contains(&id) {
            connections.push_back(id);
            if connections.len() > OWNED {
                connections.pop_front();
            }
        }
    }

    /// Applies a watcher `target-available-form` or `target-destroyed-form`.
    /// A frame announced is remembered; the sockets of a frame that went
    /// are closed, with [`FRAME_GONE`]. The sockets of the tab's document
    /// are closed, with [`NAVIGATED_AWAY`], when its target goes or when a
    /// new top-level target replaces it (a navigation to another site moves
    /// the tab to another process): Firefox may report only the close frame
    /// it sent, and for a popup no `target-destroyed-form` at all.
    pub(crate) fn on_target_form(&mut self, body: &Value, network: &mut NetworkState) {
        let target = &body["target"];
        let Some(actor) = target.get("actor").and_then(Value::as_str) else {
            return;
        };
        let top = target.get("isTopLevelTarget").and_then(Value::as_bool);
        match (body.get("type").and_then(Value::as_str), top) {
            (Some("target-available-form"), Some(false)) => {
                self.frames.insert(actor.to_string(), ());
            }
            (Some("target-available-form"), Some(true)) => self.replace_documents(actor, network),
            (Some("target-destroyed-form"), _) => self.on_target_gone(actor, top, network),
            _ => {}
        }
    }

    /// A target went: its sockets are closed if it was a frame or the
    /// tab's document.
    fn on_target_gone(&mut self, actor: &str, top: Option<bool>, network: &mut NetworkState) {
        let actor = actor.to_string();
        let announced = self.frames.remove(&actor).is_some();
        let Some(connections) = self.targets.remove(&actor) else {
            return;
        };
        let reason = match (announced || top == Some(false), top) {
            (true, _) => FRAME_GONE,
            (false, Some(true)) => NAVIGATED_AWAY,
            (false, _) => return,
        };
        close_all(network, &connections, reason);
    }

    /// The top-level target `actor` is the tab's document now: the sockets
    /// of every other target with sockets that is not a frame (the
    /// documents before it) are closed.
    fn replace_documents(&mut self, actor: &str, network: &mut NetworkState) {
        let before: Vec<String> = self
            .targets
            .keys()
            .filter(|known| *known != actor && !self.frames.contains_key(*known))
            .cloned()
            .collect();
        for gone in before {
            if let Some(connections) = self.targets.remove(&gone) {
                close_all(network, &connections, NAVIGATED_AWAY);
            }
        }
    }
}

/// Closes `connections` for `reason`; one Firefox reported without its
/// opening (it opened before the tab was watched) first takes the URL of
/// its lone handshake (see [`claim_lone_handshake`]).
fn close_all(network: &mut NetworkState, connections: &VecDeque<String>, reason: &str) {
    for id in connections {
        if network.ws_url(id).is_empty() {
            claim_lone_handshake(network, id);
        }
        let close = WsClose {
            at: crate::handler::now_ms(),
            code: None,
            reason: Some(reason.to_string()),
            was_clean: None,
        };
        network.close_ws_connection(id, close);
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
            network.insert_ws_connection(id.clone(), url.clone());
            move_guessed_close(network, &url);
            Some(id)
        }
        Some(kind @ ("frameSent" | "frameReceived")) => {
            on_frame(network, &id, kind == "frameSent", &resource["data"]);
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
            if id != NEVER_OPENED {
                // Its opening and frames may all have come before the watch.
                network.insert_ws_connection(id.clone(), String::new());
                claim_lone_handshake(network, &id);
            }
            if let Some(id) = closed_connection(network, id) {
                network.close_ws_connection(&id, close);
            }
            None
        }
        _ => None,
    }
}

/// A frame of the connection `id`, `sent` or received, whose resource
/// `data` is given.
fn on_frame(network: &mut NetworkState, id: &str, sent: bool, data: &Value) {
    // Firefox may report frames before the socket opened, or only the
    // close frame of one that fails before it opens (#158).
    network.insert_ws_connection(id.to_string(), String::new());
    match data.get("opCode").and_then(Value::as_u64) {
        Some(1 | 2) => {}
        Some(CLOSE_FRAME) => return close_frame(network, id, sent, data.get("payload")),
        _ => return,
    }
    // Not on a close frame: a socket that fails before it opens has its
    // close (id 0) take the handshake (see `closed_connection`).
    claim_lone_handshake(network, id);
    // Large payloads come as a long string grip: its first part, marked
    // with the full length (as `evaluate_on_call_frame` does).
    let payload = match data.get("payload") {
        Some(Value::String(s)) => s.clone(),
        Some(grip) => match super::grip::to_json(grip) {
            Value::String(s) => s,
            _ => String::new(),
        },
        None => String::new(),
    };
    let url = network.ws_url(id);
    let frame = WebSocketFrame::new(url, payload, sent);
    network.push_ws_frame(id.to_string(), frame);
}

/// A connection Firefox reports only by its frames or its close, its
/// opening missed (a popup's socket that opened before the tab was
/// watched), takes the URL of the accepted handshake no connection
/// claims, when there is only that one and it is the only connection
/// without a URL: then it can be no other socket's.
fn claim_lone_handshake(network: &mut NetworkState, id: &str) {
    let mut without_url = network
        .ws_connections
        .iter()
        .filter(|(_, c)| c.url.is_empty());
    let alone =
        without_url.next().is_some_and(|(only, _)| only == id) && without_url.next().is_none();
    if !alone {
        return;
    }
    let unclaimed = unclaimed_handshakes(network);
    if let (1, Some(handshake)) = (unclaimed.accepted_count, unclaimed.accepted) {
        network.insert_ws_connection(id.to_string(), handshake.url);
    }
}

/// The opcode of a WebSocket close frame.
const CLOSE_FRAME: u64 = 8;

/// A close frame of the connection `id`, `sent` or received: once both
/// sides sent theirs, the closing handshake is over and the connection is
/// closed cleanly, with the code and reason of the peer's frame. Firefox
/// reports no `webSocketClosed` for the socket of a dedicated worker it
/// terminated, only the close frames (#178); a `webSocketClosed` that comes
/// replaces this close.
fn close_frame(network: &mut NetworkState, id: &str, sent: bool, payload: Option<&Value>) {
    let Some(connection) = network.ws_connections.get_mut(id) else {
        return;
    };
    let closing = &mut connection.closing;
    if sent {
        closing.sent = true;
    } else {
        closing.received = Some(close_payload(payload));
    }
    let Some((code, reason)) = closing.received.clone().filter(|_| closing.sent) else {
        return;
    };
    if connection.closed_at.is_some() {
        return;
    }
    let close = WsClose {
        at: crate::handler::now_ms(),
        code,
        reason,
        was_clean: Some(true),
    };
    network.close_ws_connection(id, close);
    if let Some(connection) = network.ws_connections.get_mut(id) {
        connection.closing.inferred = true;
    }
}

/// The code and reason of a close frame's payload, which Firefox gives as
/// a string of one character per byte: the code in the first two, then
/// the reason's UTF-8 bytes.
fn close_payload(payload: Option<&Value>) -> crate::handler::CloseFrame {
    let bytes: Option<Vec<u8>> = payload
        .and_then(Value::as_str)
        .and_then(|text| text.chars().map(|c| u8::try_from(c).ok()).collect());
    let Some(bytes) = bytes else {
        return (None, None);
    };
    let code = match bytes.as_slice() {
        [high, low, ..] => Some(u16::from_be_bytes([*high, *low])),
        _ => None,
    };
    let reason = bytes
        .get(2..)
        .filter(|rest| !rest.is_empty())
        .map(|rest| String::from_utf8_lossy(rest).into_owned());
    (code, reason)
}

/// The connection a `webSocketClosed` for `id` closes. Firefox gives the
/// id 0 to a socket that failed before it opened, which it reports only by
/// the close frame it sent, if any (#158): that is the newest connection
/// without a URL (it gets the URL of the oldest accepted handshake no
/// connection claims), or else one made for a WebSocket handshake of the
/// log that no connection claims, under its request id: the newest one the
/// server refused or that failed, whose connection then says why (#164),
/// else the oldest accepted one (`101`; the server dropped it after
/// accepting it). An accepted handshake no connection claims yet may be a
/// live socket whose opening is not reported yet: it comes last, so a
/// refusal's close is never put on it (#171), and its connection is a
/// guess, moved to another handshake when that socket opens (#174).
fn closed_connection(network: &mut NetworkState, id: String) -> Option<String> {
    if id != NEVER_OPENED {
        return Some(id);
    }
    let Unclaimed {
        accepted, refused, ..
    } = unclaimed_handshakes(network);
    let (id, handshake, guessed) = match newest_without_url(network) {
        // It sent a frame: its handshake was accepted.
        Some(unopened) => (unopened, accepted, false),
        None => {
            let guessed = refused.is_none();
            let handshake = refused.or(accepted)?;
            (handshake.request.clone(), Some(handshake), guessed)
        }
    };
    let (url, refusal) = handshake.map_or_else(Default::default, |h| (h.url, h.refusal));
    network.insert_ws_connection(id.clone(), url);
    if let Some(refusal) = refusal {
        network.fail_ws_connection(&id, &refusal);
    }
    if let Some(connection) = network.ws_connections.get_mut(&id) {
        connection.guessed = guessed;
    }
    Some(id)
}

/// A socket to `url` opened: a close put on an accepted handshake to
/// `url` no socket claimed was put on this live socket's handshake (its
/// opening came after the close). The close moves to the oldest accepted
/// handshake still unclaimed, the socket the server really dropped, or is
/// dropped when there is none (#174).
fn move_guessed_close(network: &mut NetworkState, url: &str) {
    let guessed = network
        .ws_connections
        .keys_oldest_first()
        .find(|id| {
            network
                .ws_connections
                .get(*id)
                .is_some_and(|c| c.guessed && c.url == url)
        })
        .cloned();
    let Some(close) = guessed
        .and_then(|id| network.ws_connections.remove(&id))
        .and_then(|wrong| wrong.close())
    else {
        return;
    };
    let Some(handshake) = unclaimed_handshakes(network).accepted else {
        return;
    };
    let id = handshake.request;
    network.insert_ws_connection(id.clone(), handshake.url);
    network.close_ws_connection(&id, close);
    if let Some(connection) = network.ws_connections.get_mut(&id) {
        connection.guessed = true;
    }
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

/// The handshakes of the log no connection claims (see
/// [`unclaimed_handshakes`]).
#[derive(Default)]
struct Unclaimed {
    accepted: Option<Handshake>,
    /// How many accepted handshakes no connection claims.
    accepted_count: usize,
    refused: Option<Handshake>,
}

/// The WebSocket handshakes of the log that no connection claims: the
/// oldest accepted (`101`) and the newest refused or failed: each
/// connection claims the oldest handshake to its URL, a refused one if it
/// was refused.
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
                _ if !refused => {
                    unclaimed.accepted_count += 1;
                    unclaimed.accepted.get_or_insert(handshake);
                }
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
            channel: Channel::Watcher,
            ..Default::default()
        });
    }
    let error = resource.get("pageError")?;
    let category = error
        .get("category")
        .and_then(Value::as_str)
        .unwrap_or_default();
    Some(ConsoleMessage {
        source: source_label(kind, category).into(),
        level: page_error_level(kind, error).into(),
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
        document: error
            .get("innerWindowID")
            .and_then(Value::as_u64)
            .map(|id| format!("window-{id}")),
        uncaught: thrown(error),
        target_type: None,
    })
}

/// Most stack frames kept with an uncaught error (`get_console_logs`
/// shows the first 5; `include_details` all of them).
const MAX_STACK_FRAMES: usize = 20;

/// A page error's text; an unhandled rejection's starts with
/// `Uncaught (in promise)`, as on Chrome (ISSUE-80). An uncaught error (or
/// rejection, or a worker's error) is followed by its stack, as Chrome
/// gives it (#204: Firefox lists it apart, in `stacktrace`).
fn error_text(error: &Value) -> Option<String> {
    let text = error.get("errorMessage")?.as_str()?;
    let rejection = error.get("isPromiseRejection").and_then(Value::as_bool) == Some(true);
    let mut out = match rejection {
        true => format!("{}{text}", crate::handler::cdp_domains::log::IN_PROMISE),
        false => text.to_string(),
    };
    if thrown(error) {
        let frames = error.get("stacktrace").and_then(Value::as_array);
        let frames = frames.into_iter().flatten().map(|frame| StackFrame {
            function: str_of(frame, "functionName"),
            url: str_of(frame, "filename"),
            line: frame.get("lineNumber").and_then(Value::as_u64).unwrap_or(0),
            column: frame
                .get("columnNumber")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        });
        out.push_str(&v8_stack(frames));
    }
    Some(out)
}

/// The console level of the page error `error`, a `kind` resource.
fn page_error_level(kind: &str, error: &Value) -> &'static str {
    let flag = |key: &str| error.get(key).and_then(Value::as_bool).unwrap_or(false);
    if flag("error") {
        "error"
    } else if flag("warning") || kind == "css-message" {
        "warning"
    } else {
        "info"
    }
}

/// The category of a dedicated worker's script errors (it says nothing of
/// JavaScript, and they carry no `hasException`).
const WORKER_CATEGORY: &str = "Web Worker";

/// Whether the page error `error` reports a value the page's code threw
/// that nothing caught: it carries the exception, is an unhandled
/// rejection, or a worker's script error; not a warning or a report of
/// Firefox's own (a cookie rejected, a CSP block).
fn thrown(error: &Value) -> bool {
    let flag = |key: &str| error.get(key).and_then(Value::as_bool) == Some(true);
    let worker = error.get("category").and_then(Value::as_str) == Some(WORKER_CATEGORY);
    flag("hasException") || flag("isPromiseRejection") || worker
}

/// The string `key` of `value`, or an empty one.
fn str_of<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or_default()
}

/// One frame of a stack, its line and column counted from 1.
pub(crate) struct StackFrame<'a> {
    pub(crate) function: &'a str,
    pub(crate) url: &'a str,
    pub(crate) line: u64,
    pub(crate) column: u64,
}

/// A stack as V8 writes it after an error's message, one
/// `\n    at function (url:line:column)` per frame (`at url:line:column`
/// for one without a name), at most [`MAX_STACK_FRAMES`], without
/// Firefox's own frames (`chrome://`, `resource://`: its Remote Agent
/// running `evaluate_js`), as Chrome leaves out its own.
pub(crate) fn v8_stack<'a>(frames: impl Iterator<Item = StackFrame<'a>>) -> String {
    let page = |frame: &StackFrame<'_>| !is_firefox_own(frame.url);
    let lines = frames.filter(page).take(MAX_STACK_FRAMES).map(|frame| {
        let place = format!("{}:{}:{}", frame.url, frame.line, frame.column);
        match frame.function {
            "" => format!("\n    at {place}"),
            name => format!("\n    at {name} ({place})"),
        }
    });
    lines.collect()
}

/// Whether `url` is a script of Firefox's own (`chrome://`, `resource://`:
/// its Remote Agent running `evaluate_js`), in no page.
pub(crate) fn is_firefox_own(url: &str) -> bool {
    url.starts_with("chrome://") || url.starts_with("resource://")
}

/// What a Firefox console category is about: `javascript` (script errors),
/// `css`, `security` (cookies, mixed content, CSP...), `network`, else
/// `other`.
fn source_label(kind: &str, category: &str) -> &'static str {
    let worker = category == WORKER_CATEGORY;
    let category = category.to_ascii_lowercase();
    if kind == "css-message" || category.starts_with("css") {
        "css"
    } else if category.contains("javascript") || worker {
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
            network.insert_request((*id).into(), accepted_handshake(path));
        }
        network
    }

    /// A WebSocket handshake the server accepted (`101`).
    fn accepted_handshake(path: &str) -> NetworkRequest {
        NetworkRequest {
            url: format!("http://127.0.0.1:8808{path}"),
            method: "GET".into(),
            response_status: Some(101),
            ..Default::default()
        }
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

    /// #171: a socket that never opened was most likely refused, or its
    /// handshake failed: a refused or failed handshake no connection claims
    /// is preferred to an accepted one, which may be a live socket whose
    /// opening is not reported yet; a handshake that failed without an
    /// answer gives its network error.
    #[test]
    fn refused_handshakes_come_before_accepted_ones() {
        // Given a handshake that failed, then an accepted one
        let mut network = handshakes(&[]);
        let unanswered = refused_handshake("/down", None, Some("NS_ERROR_CONNECTION_REFUSED"));
        network.insert_request("r1".into(), unanswered);
        network.insert_request("r2".into(), accepted_handshake("/up"));
        let failed = json!({"httpChannelId": 0, "wsMessageType": "webSocketClosed", "code": 1006});
        // When a socket closes before it opened
        on_websocket(&mut network, &failed);
        // Then the failed handshake is claimed, with its error
        let down = &network.ws_connections["r1"];
        assert_eq!(down.url, "ws://127.0.0.1:8808/down");
        assert_eq!(
            down.error.as_deref(),
            Some("the handshake failed: NS_ERROR_CONNECTION_REFUSED")
        );
        assert!(!network.ws_connections.contains_key("r2"));
        // And when /up opens, it stays open; a second close finds nothing
        let opened = json!({"httpChannelId": 45, "wsMessageType": "webSocketOpened",
            "effectiveURI": "ws://127.0.0.1:8808/up"});
        on_websocket(&mut network, &opened);
        on_websocket(&mut network, &failed);
        assert!(network.ws_connections["45"].closed_at.is_none());
        assert_eq!(network.ws_connections.len(), 2);
    }

    /// #171: a burst of refused and dropped handshakes next to live
    /// sockets whose opening comes late: each refusal or drop is closed
    /// under its own handshake, and no live socket is listed closed.
    #[test]
    fn refusals_in_a_burst_never_close_a_live_socket() {
        // Given 3 refused, 2 dropped and 2 accepted handshakes, interleaved
        let mut network = NetworkState::default();
        for (id, request) in [
            ("d1", refused_handshake("/deny?n=1", Some(403), None)),
            ("a1", accepted_handshake("/echo?live=1")),
            (
                "x1",
                refused_handshake("/drop?n=1", None, Some("NS_ERROR_NET_RESET")),
            ),
            ("d2", refused_handshake("/deny?n=2", Some(403), None)),
            ("a2", accepted_handshake("/echo?live=2")),
            (
                "x2",
                refused_handshake("/drop?n=2", None, Some("NS_ERROR_NET_RESET")),
            ),
            ("d3", refused_handshake("/deny?n=3", Some(403), None)),
        ] {
            network.insert_request(id.into(), request);
        }
        let failed = json!({"httpChannelId": 0, "wsMessageType": "webSocketClosed", "code": 1006});
        // When the 5 failures close before the live sockets report opening
        for _ in 0..5 {
            on_websocket(&mut network, &failed);
        }
        for (id, live) in [("51", 1), ("52", 2)] {
            let opened = json!({"httpChannelId": id, "wsMessageType": "webSocketOpened",
                "effectiveURI": format!("ws://127.0.0.1:8808/echo?live={live}")});
            on_websocket(&mut network, &opened);
        }
        // Then each refusal and drop is closed under its own handshake
        for id in ["d1", "d2", "d3", "x1", "x2"] {
            let connection = &network.ws_connections[id];
            assert!(connection.closed_at.is_some(), "{id}");
            assert!(connection.error.is_some(), "{id}");
        }
        // And the live sockets are open, each listed once
        assert_eq!(network.ws_connections.len(), 7);
        assert!(network.ws_connections["51"].closed_at.is_none());
        assert!(network.ws_connections["52"].closed_at.is_none());
    }

    /// #174: a socket the server accepted then dropped closes with the id
    /// 0 while live sockets opened with it have not reported opening yet:
    /// its close goes to the oldest accepted handshake no socket claims,
    /// and moves when that one's socket opens, so no live socket is ever
    /// listed closed and the dropped one is.
    #[test]
    fn a_dropped_accepted_socket_never_closes_a_live_one() {
        // Given live /echo?a=1, refused /deny, dropped /acc-close, unanswered
        // /drop and live /echo?a=2, opened together
        let mut network = NetworkState::default();
        for (id, request) in [
            ("e1", accepted_handshake("/echo?r=1&a=1")),
            ("d1", refused_handshake("/deny?r=1", Some(403), None)),
            ("c1", accepted_handshake("/acc-close?r=1")),
            (
                "x1",
                refused_handshake("/drop?r=1", None, Some("NS_ERROR_NET_RESET")),
            ),
            ("e2", accepted_handshake("/echo?r=1&a=2")),
        ] {
            network.insert_request(id.into(), request);
        }
        let failed = json!({"httpChannelId": 0, "wsMessageType": "webSocketClosed", "code": 1006, "wasClean": false});
        // When the three failures close before the live sockets open
        for _ in 0..3 {
            on_websocket(&mut network, &failed);
        }
        for (id, live) in [("61", 1), ("62", 2)] {
            let opened = json!({"httpChannelId": id, "wsMessageType": "webSocketOpened",
                "effectiveURI": format!("ws://127.0.0.1:8808/echo?r=1&a={live}")});
            on_websocket(&mut network, &opened);
        }
        // Then the live sockets are open, each listed once
        assert!(network.ws_connections["61"].closed_at.is_none());
        assert!(network.ws_connections["62"].closed_at.is_none());
        let closed: Vec<&str> = network
            .ws_connections
            .values()
            .filter(|c| c.closed_at.is_some())
            .map(|c| c.url.as_str())
            .collect();
        // And the dropped, refused and unanswered sockets are closed, once
        let mut closed = closed;
        closed.sort_unstable();
        assert_eq!(
            closed,
            [
                "ws://127.0.0.1:8808/acc-close?r=1",
                "ws://127.0.0.1:8808/deny?r=1",
                "ws://127.0.0.1:8808/drop?r=1",
            ]
        );
        assert_eq!(network.ws_connections.len(), 5);
        let dropped = &network.ws_connections["c1"];
        assert_eq!(
            (dropped.close_code, dropped.was_clean),
            (Some(1006), Some(false))
        );
    }

    /// #174: a close put on a live socket's handshake, with no other
    /// accepted handshake left, is dropped when the socket opens.
    #[test]
    fn a_wrong_guess_with_nothing_left_is_dropped() {
        // Given one accepted handshake whose socket opens late
        let mut network = handshakes(&[("e1", "/echo")]);
        let failed = json!({"httpChannelId": 0, "wsMessageType": "webSocketClosed", "code": 1006});
        on_websocket(&mut network, &failed);
        assert!(network.ws_connections["e1"].closed_at.is_some());
        // When it opens
        let opened = json!({"httpChannelId": 70, "wsMessageType": "webSocketOpened",
            "effectiveURI": "ws://127.0.0.1:8808/echo"});
        on_websocket(&mut network, &opened);
        // Then only the live socket is listed, open
        assert_eq!(network.ws_connections.len(), 1);
        assert!(network.ws_connections["70"].closed_at.is_none());
    }

    /// The close frame `kind` (`frameSent`, `frameReceived`) of
    /// connection `id` with `payload`.
    fn close_frame_of(id: u64, kind: &str, payload: &str) -> Value {
        json!({"httpChannelId": id, "wsMessageType": kind,
            "data": {"opCode": 8, "payload": payload}})
    }

    /// #178: Firefox reports only the close frames of a terminated
    /// dedicated worker's socket, no `webSocketClosed`: once both sides
    /// sent theirs, the socket is closed cleanly with the peer's code and
    /// reason; one side's frame alone closes nothing.
    #[test]
    fn a_closing_handshake_closes_the_socket() {
        // Given a worker's open socket (Firefox 159's resources)
        let mut network = NetworkState::default();
        let opened = json!({"httpChannelId": 42, "wsMessageType": "webSocketOpened",
            "effectiveURI": "ws://127.0.0.1:8808/echo?k=worker"});
        on_websocket(&mut network, &opened);
        // When the browser sends its close frame (1001, going away)
        on_websocket(
            &mut network,
            &close_frame_of(42, "frameSent", "\u{3}\u{e9}"),
        );
        // Then it is still open
        assert!(network.ws_connections["42"].closed_at.is_none());
        // And when the server answers with its own, it is closed, cleanly
        on_websocket(
            &mut network,
            &close_frame_of(42, "frameReceived", "\u{3}\u{e9}bye \u{c3}\u{a9}"),
        );
        let closed = &network.ws_connections["42"];
        assert!(closed.closed_at.is_some());
        assert_eq!(
            (
                closed.close_code,
                closed.close_reason.as_deref(),
                closed.was_clean
            ),
            (Some(1001), Some("bye é"), Some(true))
        );
        // And no frame is listed for the close frames
        assert!(network.ws_frames.get("42").is_none_or(VecDeque::is_empty));
        // And a webSocketClosed that comes after replaces it
        let reported = json!({"httpChannelId": 42, "wsMessageType": "webSocketClosed",
            "code": 1000, "reason": "", "wasClean": true});
        on_websocket(&mut network, &reported);
        assert_eq!(network.ws_connections["42"].close_code, Some(1000));
        assert_eq!(network.ws_connections["42"].close_reason, None);
    }

    /// A socket that opened before the tab was watched (a popup's, which
    /// the watcher attaches to on the first tool that reads it) is known
    /// only by its frames and its close: it takes the URL of the one
    /// accepted handshake no connection claims, when it is the only
    /// connection without one; with two candidates either way, none.
    #[test]
    fn a_socket_opened_before_the_watch_takes_its_lone_handshake() {
        // Given a popup's handshake, its socket's opening missed
        let mut network = handshakes(&[("h1", "/echo?popup")]);
        // When its close frame and its close come
        on_websocket(
            &mut network,
            &close_frame_of(42, "frameSent", "\u{3}\u{e8}"),
        );
        let closed = json!({"httpChannelId": 42, "wsMessageType": "webSocketClosed", "code": 1005});
        on_websocket(&mut network, &closed);
        // Then it is listed closed with the handshake's URL
        let socket = &network.ws_connections["42"];
        assert_eq!(socket.url, "ws://127.0.0.1:8808/echo?popup");
        assert!(socket.closed_at.is_some());
        // And a socket never seen before its close (a navigation of the
        // tab ended it) is listed closed with its lone handshake's URL
        let mut unseen = handshakes(&[("h1", "/echo?unseen")]);
        let closed = json!({"httpChannelId": 77, "wsMessageType": "webSocketClosed", "code": 1001});
        on_websocket(&mut unseen, &closed);
        let socket = &unseen.ws_connections["77"];
        assert_eq!(socket.url, "ws://127.0.0.1:8808/echo?unseen");
        assert_eq!(socket.close_code, Some(1001));
        // And with two handshakes unclaimed, a socket without a URL gets none
        let mut two = handshakes(&[("h1", "/a"), ("h2", "/b")]);
        let frame = json!({"httpChannelId": 50, "wsMessageType": "frameReceived",
            "data": {"opCode": 1, "payload": "x"}});
        on_websocket(&mut two, &frame);
        assert_eq!(two.ws_connections["50"].url, "");
        // And a second socket without a URL, the handshake taken, gets none
        let mut lone = handshakes(&[("h1", "/a")]);
        for id in [60, 61] {
            let frame = json!({"httpChannelId": id, "wsMessageType": "frameReceived",
                "data": {"opCode": 1, "payload": "x"}});
            on_websocket(&mut lone, &frame);
        }
        assert_eq!(lone.ws_connections["60"].url, "ws://127.0.0.1:8808/a");
        assert_eq!(lone.ws_connections["61"].url, "");
    }

    /// The sockets of the tab's document are closed when its target goes
    /// (a navigation to another site moves the tab to another process):
    /// Firefox may report only the close frame it sent; a socket that
    /// closed already keeps its close, and a target that is neither the
    /// tab's nor an announced frame closes nothing.
    #[test]
    fn sockets_of_a_document_whose_target_goes_are_closed() {
        let mut network = NetworkState::default();
        let mut owners = SocketOwners::default();
        for (actor, id) in [("top1", "41"), ("top1", "42"), ("other9", "43")] {
            let opened = json!({"httpChannelId": id, "wsMessageType": "webSocketOpened",
                "effectiveURI": format!("ws://127.0.0.1:8808/{id}")});
            on_websocket(&mut network, &opened);
            owners.own(actor, id.to_string());
        }
        let done = json!({"httpChannelId": 42, "wsMessageType": "webSocketClosed", "code": 1000});
        on_websocket(&mut network, &done);
        // When the document's target goes, then another unknown target
        let gone = |target: Value| json!({"type": "target-destroyed-form", "target": target});
        owners.on_target_form(
            &gone(json!({"actor": "top1", "isTopLevelTarget": true})),
            &mut network,
        );
        owners.on_target_form(&gone(json!({"actor": "other9"})), &mut network);
        // Then its open socket is closed, saying why
        let closed = &network.ws_connections["41"];
        assert_eq!(closed.close_reason.as_deref(), Some(NAVIGATED_AWAY));
        assert_eq!(network.ws_connections["42"].close_code, Some(1000));
        assert!(network.ws_connections["43"].closed_at.is_none());
    }

    /// A popup navigated to another site: Firefox announces its new
    /// top-level target and never says the old one went, and reported only
    /// the close frame of a socket that opened before the popup was
    /// watched. The new document's target closes the old one's sockets,
    /// the one without a URL taking its lone handshake's; a frame's
    /// sockets, and the new document's, stay open.
    #[test]
    fn a_new_document_closes_the_sockets_of_the_old_one() {
        // Given a popup's socket known by its close frame only, and a frame's
        let mut network = handshakes(&[("h1", "/echo?popup")]);
        let mut owners = SocketOwners::default();
        let available = |actor: &str, top: bool| {
            json!({"type": "target-available-form",
            "target": {"actor": actor, "isTopLevelTarget": top}})
        };
        owners.on_target_form(&available("w.process8//top", true), &mut network);
        owners.on_target_form(&available("w.process8//frame", false), &mut network);
        on_websocket(
            &mut network,
            &close_frame_of(42, "frameSent", "\u{3}\u{e9}"),
        );
        owners.own("w.process8//top", "42".into());
        let opened = json!({"httpChannelId": 43, "wsMessageType": "webSocketOpened",
            "effectiveURI": "ws://127.0.0.1:8808/frame"});
        on_websocket(&mut network, &opened);
        owners.own("w.process8//frame", "43".into());
        // When the same top-level target is announced again, nothing closes
        owners.on_target_form(&available("w.process8//top", true), &mut network);
        assert!(network.ws_connections["42"].closed_at.is_none());
        // And when a new top-level target is announced
        owners.on_target_form(&available("w.process9//top", true), &mut network);
        // Then the old document's socket is closed, with its handshake's URL
        let closed = &network.ws_connections["42"];
        assert_eq!(closed.url, "ws://127.0.0.1:8808/echo?popup");
        assert_eq!(closed.close_reason.as_deref(), Some(NAVIGATED_AWAY));
        // And the frame's socket is left to its frame
        assert!(network.ws_connections["43"].closed_at.is_none());
        // And a socket of the new document stays open when it is announced again
        owners.own("w.process9//top", "44".into());
        on_websocket(
            &mut network,
            &json!({"httpChannelId": 44, "wsMessageType": "webSocketOpened",
            "effectiveURI": "ws://127.0.0.1:8808/new"}),
        );
        owners.on_target_form(&available("w.process9//top", true), &mut network);
        assert!(network.ws_connections["44"].closed_at.is_none());
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

    /// #172: the sockets of many frames (more than 64) are all closed when
    /// their frames go, and many frames without sockets do not push out
    /// the frames that have one.
    #[test]
    fn sockets_of_many_frames_are_closed() {
        // Given a frame with a socket, 200 frames without, 70 frames with one
        let mut network = NetworkState::default();
        let mut owners = SocketOwners::default();
        let announce = |owners: &mut SocketOwners, network: &mut NetworkState, actor: &str| {
            let form = json!({"type": "target-available-form",
                "target": {"actor": actor, "isTopLevelTarget": false}});
            owners.on_target_form(&form, network);
        };
        let open = |owners: &mut SocketOwners, network: &mut NetworkState, actor: &str| {
            let id = format!("{actor}-ws");
            network.insert_ws_connection(id.clone(), format!("ws://a/{actor}"));
            owners.own(actor, id);
        };
        announce(&mut owners, &mut network, "early");
        open(&mut owners, &mut network, "early");
        for n in 0..200 {
            announce(&mut owners, &mut network, &format!("empty{n}"));
        }
        let frames: Vec<String> = (0..70).map(|n| format!("frame{n}")).collect();
        for actor in &frames {
            announce(&mut owners, &mut network, actor);
            open(&mut owners, &mut network, actor);
        }
        // When every frame goes (its destroyed form says no more)
        for actor in frames.iter().chain(std::iter::once(&"early".to_string())) {
            let destroyed = json!({"type": "target-destroyed-form", "target": {"actor": actor}});
            owners.on_target_form(&destroyed, &mut network);
        }
        // Then all their sockets are closed, saying why
        assert_eq!(network.ws_connections.len(), 71);
        for connection in network.ws_connections.values() {
            assert_eq!(connection.close_reason.as_deref(), Some(FRAME_GONE));
        }
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
        assert!(owners.targets.values().all(|c| c.len() == OWNED));
        for n in 0..(FRAMES * 2) {
            let form = json!({"type": "target-available-form",
                "target": {"actor": format!("f{n}"), "isTopLevelTarget": false}});
            owners.on_target_form(&form, &mut network);
        }
        assert_eq!(owners.frames.len(), FRAMES);
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
    /// `Uncaught (in promise)`, as on Chrome, and replaces BiDi's copy;
    /// #204: with its stack, which BiDi's copy has not.
    #[test]
    fn promise_rejections_say_so() {
        let mut errors = resources(fixtures::RDP_RESOURCES_ERROR_MESSAGE);
        errors[0]["pageError"]["isPromiseRejection"] = json!(true);
        errors[0]["pageError"]["errorMessage"] = json!("Error: rej");
        let rejected = console_message("error-message", &errors[0]).unwrap();
        // #210: uncaught, as the watcher says it; a rejected cookie is not
        assert!(rejected.uncaught);
        assert!(
            !console_message("error-message", &errors[1])
                .unwrap()
                .uncaught
        );
        let expected =
            "Uncaught (in promise) Error: rej\n    at https://example.com/test.html:43:7";
        assert_eq!(rejected.text, expected);
        let mut log = crate::handler::cdp_domains::log::LogState::default();
        let mut bidi = rejected.clone();
        (bidi.text, bidi.source, bidi.channel) =
            ("Error: rej".into(), "exception".into(), Channel::Primary);
        log.push_deduplicated(bidi);
        log.push_deduplicated(rejected);
        let texts: Vec<&str> = log.messages.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(texts, [expected]);
    }

    /// #204 (Firefox): an uncaught error carries its stack, one line per
    /// frame in Chrome's form (`at function (url:line:column)`, or
    /// `at url:line:column` for a frame without a name), at most
    /// [`MAX_STACK_FRAMES`], Firefox's own frames left out; a message
    /// without an exception (a cookie warning) has none.
    #[test]
    fn uncaught_errors_carry_their_stack() {
        let mut errors = resources(fixtures::RDP_RESOURCES_ERROR_MESSAGE);
        let frame = |name: Value, line: u64| json!({"functionName": name, "filename": "http://a/index.html", "lineNumber": line, "columnNumber": 25, "sourceId": null});
        let frames: Vec<Value> = std::iter::once(frame(json!("boom"), 3))
            .chain((0..MAX_STACK_FRAMES + 5).map(|_| frame(Value::Null, 9)))
            .collect();
        let mut frames = frames;
        let internal = json!({"functionName": "executeInRealm", "filename": "chrome://remote/content/shared/Realm.sys.mjs", "lineNumber": 306, "columnNumber": 40});
        frames.insert(1, internal);
        errors[0]["pageError"]["stacktrace"] = Value::Array(frames);
        let thrown = console_message("error-message", &errors[0]).unwrap();
        let lines: Vec<&str> = thrown.text.lines().collect();
        assert_eq!(lines.len(), 1 + MAX_STACK_FRAMES, "{}", thrown.text);
        assert_eq!(
            lines[0],
            "ReferenceError: nonExistentFunctionCallHere is not defined"
        );
        assert_eq!(lines[1], "    at boom (http://a/index.html:3:25)");
        assert_eq!(lines[2], "    at http://a/index.html:9:25");
        let cookie = console_message("error-message", &errors[1]).unwrap();
        assert!(!cookie.text.contains('\n'));
    }

    /// #204 (Firefox): a worker's uncaught error (category `Web Worker`,
    /// without `hasException`) is a script error with its stack, as the
    /// copy BiDi reports, which it replaces.
    #[test]
    fn worker_errors_are_script_errors_with_their_stack() {
        let mut errors = resources(fixtures::RDP_RESOURCES_ERROR_MESSAGE);
        let error = &mut errors[0]["pageError"];
        error["category"] = json!("Web Worker");
        error["hasException"] = Value::Null;
        error["errorMessage"] = json!("Error: probe-throw WORKER");
        error["stacktrace"] = json!([{"columnNumber": 48, "filename": "http://a/probe-worker.js",
            "functionName": "probeThrower", "lineNumber": 7, "sourceId": null}]);
        let thrown = console_message("error-message", &errors[0]).unwrap();
        assert_eq!(thrown.source, "javascript");
        assert_eq!(
            thrown.text,
            "Error: probe-throw WORKER\n    at probeThrower (http://a/probe-worker.js:7:48)"
        );
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
            "ReferenceError: nonExistentFunctionCallHere is not defined\n    at https://example.com/test.html:43:7"
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
