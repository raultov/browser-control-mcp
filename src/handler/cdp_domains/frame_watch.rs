//! Console and network of a tab's frames of another origin on Chrome
//! (#36). Such a frame runs in another process, with a target of its own:
//! the tab's session sees neither its console messages nor its requests.
//! Each tab auto-attaches to its frames' targets (`Target.setAutoAttach`,
//! each frame paused until released, so its first messages are kept). For
//! each iframe the server attaches a session of its own, enables
//! `Runtime`, `Log` and `Network` there, feeds the tab's buffers from it
//! and auto-attaches to its own frames, then releases the frame. A frame is
//! released on every path, errors included: one left waiting would never
//! run.

use crate::handler::NetworkState;
use crate::handler::cdp_domains::cdp_target::CdpTarget;
use crate::handler::cdp_domains::event_pump::ListenerHandles;
use crate::handler::cdp_domains::log::LogState;
use cdp_browser_lite::{BrowserClient, NoParams, Tab, WsResponse};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::Mutex;
use tokio_stream::StreamExt;

/// Most frames of another origin watched per tab (ad pages rotate dozens);
/// past it a new frame is released without being watched.
pub(crate) const MAX_WATCHED: usize = 50;

/// The domains enabled on a watched frame's session.
const FRAME_DOMAINS: [&str; 3] = ["Runtime", "Log", "Network"];

/// The auto-attach every watched session asks for.
fn auto_attach() -> Value {
    json!({"autoAttach": true, "waitForDebuggerOnStart": true, "flatten": true})
}

/// Where a tab's frames report, and how many of them are watched.
#[derive(Clone)]
pub(crate) struct FrameSinks {
    pub(crate) log: Arc<Mutex<LogState>>,
    pub(crate) network: Arc<Mutex<NetworkState>>,
    watched: Arc<AtomicUsize>,
}

impl FrameSinks {
    /// The sinks of a tab whose buffers are `log` and `network`.
    pub(crate) fn new(log: Arc<Mutex<LogState>>, network: Arc<Mutex<NetworkState>>) -> Self {
        Self {
            log,
            network,
            watched: Arc::default(),
        }
    }

    /// Takes a place among the [`MAX_WATCHED`] frames, if one is free.
    fn take_place(&self) -> bool {
        let free = |watched: usize| (watched < MAX_WATCHED).then_some(watched + 1);
        self.watched
            .try_update(Ordering::Relaxed, Ordering::Relaxed, free)
            .is_ok()
    }
}

/// Watches the frames of another origin of `session` (a tab, or a watched
/// frame for the frames nested in it), feeding `sinks`. The events are
/// subscribed before the auto-attach is asked for: it announces the frames
/// already there at once.
pub(crate) fn start_frame_watcher(session: &Tab, sinks: FrameSinks) -> tokio::task::JoinHandle<()> {
    let events = session.on_domain("Target");
    let session = session.clone();
    tokio::spawn(async move {
        let _ = session
            .send_raw_command("Target.setAutoAttach", auto_attach())
            .await;
        watch(session, events, sinks).await;
    })
}

/// Handles `session`'s target events: each attached target is set up in a
/// task of its own (the loop never waits, so no event is missed), kept
/// until it detaches, with whether it counts as watched (an iframe within
/// [`MAX_WATCHED`]): only those give their place back.
async fn watch(session: Tab, mut events: cdp_browser_lite::EventFilter, sinks: FrameSinks) {
    let mut frames: HashMap<String, (ListenerHandles, bool)> = HashMap::new();
    while let Some(event) = events.next().await {
        let Ok(event) = event else { continue };
        let params = event.params.unwrap_or_default();
        let Some(auto) = params["sessionId"].as_str().map(str::to_string) else {
            continue;
        };
        match event.method.as_deref() {
            Some("Target.attachedToTarget") => {
                let watched = params["targetInfo"]["type"] == "iframe" && sinks.take_place();
                let mut handles = ListenerHandles::default();
                let setup = frame_setup(session.clone(), params, watched, sinks.clone());
                handles.push(tokio::spawn(setup));
                frames.insert(auto, (handles, watched));
            }
            Some("Target.detachedFromTarget") if frames.remove(&auto).is_some_and(|f| f.1) => {
                sinks.watched.fetch_sub(1, Ordering::Relaxed);
            }
            _ => {}
        }
    }
}

/// Sets up the frame `attached` (a `Target.attachedToTarget` of `session`)
/// describes when `watched`, releases it, then keeps its listeners until
/// aborted. Other targets (workers, frames past [`MAX_WATCHED`]) are only
/// released.
async fn frame_setup(session: Tab, attached: Value, watched: bool, sinks: FrameSinks) {
    let auto = attached["sessionId"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let mut listeners = ListenerHandles::default();
    if watched && let Some(target_id) = attached["targetInfo"]["targetId"].as_str() {
        listeners = watch_frame(&session, target_id, sinks).await;
    }
    release(&session, &auto).await;
    // Held until the frame detaches (the task is aborted then).
    let _listeners = listeners;
    std::future::pending::<()>().await;
}

/// Attaches a session of its own to frame `target_id`, enables its
/// domains and starts its listeners (console, network, nested frames).
async fn watch_frame(session: &Tab, target_id: &str, sinks: FrameSinks) -> ListenerHandles {
    let mut listeners = ListenerHandles::default();
    let browser = BrowserClient::from_client(session.client().clone());
    let Ok(frame) = browser.attach(target_id).await else {
        return listeners;
    };
    let target = CdpTarget::tab(frame.clone());
    // Subscribed before the domains are enabled: nothing is missed.
    listeners.absorb(crate::handler::cdp_domains::log::start_log_listener(
        &target,
        sinks.log.clone(),
    ));
    listeners.push(
        crate::handler::cdp_domains::network::start_frame_network_listener(
            &target,
            sinks.network.clone(),
            frame.session_id().to_string(),
        ),
    );
    for domain in FRAME_DOMAINS {
        let _ = frame
            .send_raw_command(&format!("{domain}.enable"), NoParams)
            .await;
    }
    listeners.push(start_frame_watcher(&frame, sinks));
    listeners
}

/// Lets the frame of the auto-attached session `auto` run.
async fn release(session: &Tab, auto: &str) -> Option<WsResponse> {
    session
        .client()
        .send_raw_command_to_session(auto, "Runtime.runIfWaitingForDebugger", json!({}))
        .await
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::cdp_domains::tests::spawn_recording_mock;

    /// The session id of the last command `method` the mock received.
    fn session_of(
        mock: &cdp_browser_lite::test_support::mock_devtools::MockDevTools,
        method: &str,
    ) -> Vec<Option<String>> {
        mock.commands_named(method)
            .into_iter()
            .map(|c| c.session_id)
            .collect()
    }

    /// Waits (at most 2 s) until `done` holds.
    async fn eventually(done: impl Fn() -> bool) {
        for _ in 0..200 {
            if done() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    /// A watched tab of the mock, its sinks and the attached tab session.
    struct Watched {
        mock: &'static cdp_browser_lite::test_support::mock_devtools::MockDevTools,
        tab: Tab,
        sinks: FrameSinks,
        _watcher: tokio::task::JoinHandle<()>,
    }

    /// A tab watching its frames, once it asked for the auto-attach.
    async fn watched_tab() -> Watched {
        let mock = spawn_recording_mock().await;
        let address = format!("127.0.0.1:{}", mock.http_port);
        let browser = BrowserClient::connect(&address, std::time::Duration::from_secs(2))
            .await
            .unwrap();
        let tab = browser.attach("T-tab-1").await.unwrap();
        let sinks = FrameSinks::new(Arc::default(), Arc::default());
        let _watcher = start_frame_watcher(&tab, sinks.clone());
        eventually(|| !mock.commands_named("Target.setAutoAttach").is_empty()).await;
        Watched {
            mock,
            tab,
            sinks,
            _watcher,
        }
    }

    /// Attaches an auto-attached target of `kind` to the watched tab.
    fn attach(watched: &Watched, auto: &str, kind: &str) {
        let info = json!({"targetId": format!("T-{kind}"), "type": kind, "url": "http://b/"});
        let params = json!({"sessionId": auto, "targetInfo": info, "waitingForDebugger": true});
        let tab = Some(watched.tab.session_id());
        watched.mock.emit("Target.attachedToTarget", params, tab);
    }

    /// The session the server attached to a watched iframe, once enabled.
    async fn frame_session(watched: &Watched) -> String {
        eventually(|| !watched.mock.commands_named("Network.enable").is_empty()).await;
        session_of(watched.mock, "Network.enable")
            .pop()
            .flatten()
            .unwrap()
    }

    /// ISSUE-36: a frame of another origin is attached, its domains are
    /// enabled, it is released, and its console messages reach the tab's
    /// buffer; a worker is only released.
    #[tokio::test]
    async fn frames_of_another_origin_feed_the_tab() {
        // Given a tab watching its frames
        let watched = watched_tab().await;
        let (mock, sinks) = (watched.mock, &watched.sinks);
        let tab = watched.tab.session_id().to_string();
        assert_eq!(
            session_of(mock, "Target.setAutoAttach"),
            [Some(tab.clone())]
        );
        // When an iframe and a worker attach
        attach(&watched, "AUTO-F", "iframe");
        attach(&watched, "AUTO-W", "worker");
        eventually(|| mock.commands_named("Runtime.runIfWaitingForDebugger").len() == 2).await;
        // Then both are released, and only the iframe is attached and enabled
        let released = session_of(mock, "Runtime.runIfWaitingForDebugger");
        assert!(released.contains(&Some("AUTO-F".into())), "{released:?}");
        assert!(released.contains(&Some("AUTO-W".into())), "{released:?}");
        let attached = mock.commands_named("Target.attachToTarget");
        assert_eq!(attached.len(), 2, "the tab and the iframe: {attached:?}");
        assert_eq!(attached[1].params["targetId"], "T-iframe");
        let frame = frame_session(&watched).await;
        assert_ne!(frame, tab);
        // And the frame's console messages land in the tab's buffer
        let logged = json!({"type": "log", "args": [{"type": "string", "value": "inner log"}], "timestamp": 1.0});
        mock.emit("Runtime.consoleAPICalled", logged, Some(&frame));
        eventually(|| {
            sinks
                .log
                .try_lock()
                .is_ok_and(|log| log.messages.len() == 1)
        })
        .await;
        assert_eq!(sinks.log.lock().await.messages[0].text, "inner log");
        assert_eq!(sinks.watched.load(Ordering::Relaxed), 1);
        // When the worker detaches, then another iframe attaches
        let worker_gone = json!({"sessionId": "AUTO-W", "targetId": "T-worker"});
        mock.emit("Target.detachedFromTarget", worker_gone, Some(&tab));
        attach(&watched, "AUTO-G", "iframe");
        eventually(|| mock.commands_named("Runtime.runIfWaitingForDebugger").len() == 3).await;
        // Then both iframes are counted (the worker never was)
        assert_eq!(sinks.watched.load(Ordering::Relaxed), 2);
        // When a frame detaches, it is no longer counted
        let gone = json!({"sessionId": "AUTO-F", "targetId": "T-iframe"});
        mock.emit("Target.detachedFromTarget", gone, Some(&tab));
        eventually(|| sinks.watched.load(Ordering::Relaxed) == 1).await;
        assert_eq!(sinks.watched.load(Ordering::Relaxed), 1);
    }

    /// ISSUE-36: a frame's requests reach the tab's buffer noting the
    /// frame's session, where their bodies are; so does the frame's own
    /// document, requested by the tab, once its response reaches the frame.
    #[tokio::test]
    async fn frame_requests_note_the_frame_session() {
        // Given a watched frame, and its document requested by the tab
        let watched = watched_tab().await;
        attach(&watched, "AUTO-F", "iframe");
        let frame = frame_session(&watched).await;
        let network = &watched.sinks.network;
        let sent = |id: &str| json!({"requestId": id, "request": {"url": "http://b/", "method": "GET", "headers": {}}, "type": "Document"});
        let document = cdp_browser_lite::WsResponse {
            method: Some("Network.requestWillBeSent".into()),
            params: Some(sent("DOC")),
            ..Default::default()
        };
        crate::handler::cdp_domains::network::process_network_event(&document, network).await;
        // When the frame sends a request, and gets its document's response
        watched
            .mock
            .emit("Network.requestWillBeSent", sent("F.1"), Some(&frame));
        let response = json!({"requestId": "DOC", "response": {"url": "http://b/", "status": 200, "statusText": "OK", "headers": {}}, "type": "Document"});
        watched
            .mock
            .emit("Network.responseReceived", response, Some(&frame));
        // Then both note the frame's session
        let session_of_request = |id: &str| {
            let mut network = network.try_lock().ok()?;
            network.find_request_mut(id)?.session.clone()
        };
        eventually(|| session_of_request("F.1").is_some() && session_of_request("DOC").is_some())
            .await;
        assert_eq!(session_of_request("F.1").as_deref(), Some(frame.as_str()));
        assert_eq!(session_of_request("DOC").as_deref(), Some(frame.as_str()));
    }
}
