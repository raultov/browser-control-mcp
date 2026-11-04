//! Console, network and debugger of a tab's frames of another origin on
//! Chrome (#36). Such a frame runs in another process, with a target of its
//! own: the tab's session sees neither its console messages, its requests
//! nor its scripts. Each tab auto-attaches to its frames' targets
//! (`Target.setAutoAttach`, each frame paused until released, so its first
//! messages and scripts are kept). For each iframe the server attaches a
//! session of its own, enables `Runtime`, `Log`, `Network` and `Debugger`
//! there, feeds the tab's buffers and debugger state from it (see
//! `debugger::frames`: its breakpoints set before it runs) and
//! auto-attaches to its own frames, then releases the frame. A frame is
//! released on every path, errors included: one left waiting would never
//! run.

use crate::handler::cdp_domains::cdp_target::CdpTarget;
use crate::handler::cdp_domains::debugger;
use crate::handler::cdp_domains::event_pump::ListenerHandles;
use crate::handler::cdp_domains::log::LogState;
use crate::handler::{DebuggerState, NetworkState};
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
const FRAME_DOMAINS: [&str; 4] = ["Runtime", "Log", "Network", "Debugger"];

/// The domains enabled on a watched worker's session: not `Log`, whose
/// worker messages the tab already reports (as `info` for `console.log`).
const WORKER_DOMAINS: [&str; 2] = ["Runtime", "Network"];

/// What an auto-attached target is watched as.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Watch {
    /// A frame of another origin: console, network, debugger, its frames.
    Frame,
    /// A dedicated worker: its console and requests (ISSUE-75).
    Worker,
    /// Not watched (another kind, or past [`MAX_WATCHED`]): only released.
    Released,
}

/// The auto-attach every watched session asks for.
fn auto_attach() -> Value {
    json!({"autoAttach": true, "waitForDebuggerOnStart": true, "flatten": true})
}

/// Where a tab's frames report, and how many of them are watched.
#[derive(Clone)]
pub(crate) struct FrameSinks {
    pub(crate) log: Arc<Mutex<LogState>>,
    pub(crate) network: Arc<Mutex<NetworkState>>,
    pub(crate) debugger: Arc<Mutex<DebuggerState>>,
    watched: Arc<AtomicUsize>,
}

impl FrameSinks {
    /// The sinks of a tab whose buffers are `log` and `network`, and whose
    /// debugger state is `debugger`.
    pub(crate) fn new(
        log: Arc<Mutex<LogState>>,
        network: Arc<Mutex<NetworkState>>,
        debugger: Arc<Mutex<DebuggerState>>,
    ) -> Self {
        Self {
            log,
            network,
            debugger,
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
    let mut frames: HashMap<String, Attached> = HashMap::new();
    while let Some(event) = events.next().await {
        let Ok(event) = event else { continue };
        let params = event.params.unwrap_or_default();
        let Some(auto) = params["sessionId"].as_str().map(str::to_string) else {
            continue;
        };
        match event.method.as_deref() {
            Some("Target.attachedToTarget") => {
                let attached = attach(&session, params, &sinks).await;
                frames.insert(auto, attached);
            }
            Some("Target.detachedFromTarget") => {
                if let Some(attached) = frames.remove(&auto) {
                    attached.detach(&sinks).await;
                }
                if let Some(target_id) = params["targetId"].as_str() {
                    sinks.debugger.lock().await.forget_frame_target(target_id);
                }
            }
            _ => {}
        }
    }
}

/// An auto-attached target: its setup task (aborted when it detaches),
/// what it is watched as, and its URL.
struct Attached {
    _setup: ListenerHandles,
    kind: Watch,
    url: String,
}

impl Attached {
    /// Gives back its place and, for a worker, its console to the tab.
    async fn detach(self, sinks: &FrameSinks) {
        if self.kind == Watch::Released {
            return;
        }
        sinks.watched.fetch_sub(1, Ordering::Relaxed);
        if self.kind == Watch::Worker {
            sinks.log.lock().await.forget_worker(&self.url);
        }
    }
}

/// What the target `attached` (a `Target.attachedToTarget`) is watched as:
/// an iframe or a dedicated worker within [`MAX_WATCHED`], else released.
fn watch_kind(attached: &Value, sinks: &FrameSinks) -> Watch {
    let kind = match attached["targetInfo"]["type"].as_str() {
        Some("iframe") => Watch::Frame,
        Some("worker") => Watch::Worker,
        _ => return Watch::Released,
    };
    match sinks.take_place() {
        true => kind,
        false => Watch::Released,
    }
}

/// Starts the setup of the target `attached` announces.
async fn attach(session: &Tab, attached: Value, sinks: &FrameSinks) -> Attached {
    let kind = watch_kind(&attached, sinks);
    let url = attached["targetInfo"]["url"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    if kind == Watch::Worker {
        // Before it runs: its first messages come in its session.
        sinks.log.lock().await.watch_worker(&url);
    }
    let mut setup = ListenerHandles::default();
    let task = frame_setup(session.clone(), attached, kind, sinks.clone());
    setup.push(tokio::spawn(task));
    Attached {
        _setup: setup,
        kind,
        url,
    }
}

/// Sets up the frame `attached` (a `Target.attachedToTarget` of `session`)
/// describes as `kind` says, releases it, then keeps its listeners until
/// aborted. Other targets (other kinds, past [`MAX_WATCHED`]) are only
/// released.
async fn frame_setup(session: Tab, attached: Value, kind: Watch, sinks: FrameSinks) {
    let auto = attached["sessionId"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let mut listeners = ListenerHandles::default();
    if let Some(target_id) = attached["targetInfo"]["targetId"].as_str() {
        listeners = match kind {
            Watch::Frame => watch_frame(&session, target_id, sinks).await,
            Watch::Worker => watch_worker(&session, target_id, sinks).await,
            Watch::Released => listeners,
        };
    }
    release(&session, &auto).await;
    // Held until the frame detaches (the task is aborted then).
    let _listeners = listeners;
    std::future::pending::<()>().await;
}

/// Attaches a session of its own to frame `target_id`, enables its
/// domains and starts its listeners (console, network, debugger, nested
/// frames); its debugger is ready (pauses, breakpoints) before it runs.
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
    let session = frame.session_id().to_string();
    sinks
        .debugger
        .lock()
        .await
        .register_frame(target_id, frame.clone());
    let state = sinks.debugger.clone();
    let watch = debugger::start_debugger_listener_in(&target, state, Some(session));
    listeners.push(watch);
    for domain in FRAME_DOMAINS {
        let _ = frame
            .send_raw_command(&format!("{domain}.enable"), NoParams)
            .await;
    }
    debugger::frames::prepare_frame(&frame, &sinks.debugger).await;
    listeners.push(start_frame_watcher(&frame, sinks));
    listeners
}

/// Attaches a session of its own to worker `target_id`, starts its console
/// and network listeners, then enables its domains (ISSUE-75: Chrome's tab
/// session sees neither its requests nor its console levels).
async fn watch_worker(session: &Tab, target_id: &str, sinks: FrameSinks) -> ListenerHandles {
    let mut listeners = ListenerHandles::default();
    let browser = BrowserClient::from_client(session.client().clone());
    let Ok(worker) = browser.attach(target_id).await else {
        return listeners;
    };
    let target = CdpTarget::tab(worker.clone());
    // Subscribed before the domains are enabled: nothing is missed.
    listeners.absorb(crate::handler::cdp_domains::log::start_log_listener(
        &target,
        sinks.log.clone(),
    ));
    listeners.push(
        crate::handler::cdp_domains::network::start_frame_network_listener(
            &target,
            sinks.network.clone(),
            worker.session_id().to_string(),
        ),
    );
    for domain in WORKER_DOMAINS {
        let _ = worker
            .send_raw_command(&format!("{domain}.enable"), NoParams)
            .await;
    }
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
    use cdp_browser_lite::test_support::mock_devtools::MockDevTools;

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
        let sinks = FrameSinks::new(Arc::default(), Arc::default(), Arc::default());
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
    /// buffer; a worker is watched too (ISSUE-75).
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
        // Then both are released and attached
        let released = session_of(mock, "Runtime.runIfWaitingForDebugger");
        assert!(released.contains(&Some("AUTO-F".into())), "{released:?}");
        assert!(released.contains(&Some("AUTO-W".into())), "{released:?}");
        let attached = mock.commands_named("Target.attachToTarget");
        assert_eq!(
            attached.len(),
            3,
            "the tab, the iframe and the worker: {attached:?}"
        );
        let frame = mock
            .commands_named("Debugger.enable")
            .into_iter()
            .find_map(|c| c.session_id)
            .unwrap();
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
        assert_eq!(sinks.watched.load(Ordering::Relaxed), 2);
        // When the worker detaches, then another iframe attaches
        let worker_gone = json!({"sessionId": "AUTO-W", "targetId": "T-worker"});
        mock.emit("Target.detachedFromTarget", worker_gone, Some(&tab));
        attach(&watched, "AUTO-G", "iframe");
        eventually(|| mock.commands_named("Runtime.runIfWaitingForDebugger").len() == 3).await;
        // Then both iframes are counted (the worker gave its place back)
        eventually(|| sinks.watched.load(Ordering::Relaxed) == 2).await;
        assert_eq!(sinks.watched.load(Ordering::Relaxed), 2);
        // When a frame detaches, it is no longer counted
        let gone = json!({"sessionId": "AUTO-F", "targetId": "T-iframe"});
        mock.emit("Target.detachedFromTarget", gone, Some(&tab));
        eventually(|| sinks.watched.load(Ordering::Relaxed) == 1).await;
        assert_eq!(sinks.watched.load(Ordering::Relaxed), 1);
    }

    /// The debugger commands the mock got in `frame`'s session, and the
    /// releases, in order.
    fn readied_in(mock: &MockDevTools, frame: &str) -> Vec<String> {
        let release = "Runtime.runIfWaitingForDebugger";
        mock.commands()
            .into_iter()
            .filter(|c| c.session_id.as_deref() == Some(frame) || c.method == release)
            .map(|c| c.method)
            .filter(|m| m.starts_with("Debugger.") || m == release)
            .collect()
    }

    /// A worker attached to the watched tab, once released: its session.
    async fn attached_worker(watched: &Watched) -> String {
        attach(watched, "AUTO-W", "worker");
        let mock = watched.mock;
        eventually(|| {
            !mock
                .commands_named("Runtime.runIfWaitingForDebugger")
                .is_empty()
        })
        .await;
        frame_session(watched).await
    }

    /// ISSUE-75: a dedicated worker is attached too: its `Runtime` and
    /// `Network` are enabled in its own session (not `Log`: the tab already
    /// reports its console there), it is released, and its console messages
    /// and requests reach the tab's buffers; the tab's log skips its copies.
    #[tokio::test]
    async fn workers_feed_the_tab() {
        // Given a tab watching its frames, When a worker attaches
        let watched = watched_tab().await;
        let worker = attached_worker(&watched).await;
        let (mock, sinks) = (watched.mock, &watched.sinks);
        // Then it was readied in its own session
        let readied: Vec<String> = mock
            .commands()
            .into_iter()
            .filter(|c| c.session_id.as_deref() == Some(worker.as_str()))
            .map(|c| c.method)
            .collect();
        assert_eq!(readied, ["Runtime.enable", "Network.enable"]);
        assert!(sinks.log.lock().await.watches_worker("http://b/"));
        // And its console messages and requests reach the tab's buffers
        let logged = json!({"type": "log", "args": [{"type": "string", "value": "worker log"}], "timestamp": 1.0});
        mock.emit("Runtime.consoleAPICalled", logged, Some(&worker));
        let sent = json!({"requestId": "W1", "request": {"url": "http://b/api", "method": "GET", "headers": {}}, "type": "Fetch"});
        mock.emit("Network.requestWillBeSent", sent, Some(&worker));
        eventually(|| {
            sinks
                .log
                .try_lock()
                .is_ok_and(|log| log.messages.len() == 1)
        })
        .await;
        let message = sinks.log.lock().await.messages[0].clone();
        assert_eq!(
            (message.level.as_str(), message.text.as_str()),
            ("log", "worker log")
        );
        eventually(|| {
            sinks
                .network
                .try_lock()
                .is_ok_and(|n| n.known_ids().contains("W1"))
        })
        .await;
        assert!(sinks.network.lock().await.known_ids().contains("W1"));
    }

    /// ISSUE-75: once a worker detaches, the tab's log reports its URL's
    /// messages again, and its place is given back.
    #[tokio::test]
    async fn detached_workers_log_through_the_tab() {
        let watched = watched_tab().await;
        attached_worker(&watched).await;
        let sinks = &watched.sinks;
        let gone = json!({"sessionId": "AUTO-W", "targetId": "T-worker"});
        let tab = Some(watched.tab.session_id());
        watched.mock.emit("Target.detachedFromTarget", gone, tab);
        eventually(|| {
            sinks
                .log
                .try_lock()
                .is_ok_and(|log| !log.watches_worker("http://b/"))
        })
        .await;
        assert!(!sinks.log.lock().await.watches_worker("http://b/"));
        assert_eq!(sinks.watched.load(Ordering::Relaxed), 0);
    }

    /// ISSUE-36: a frame's debugger is ready before the frame runs: it is
    /// registered, enabled, told whether to skip pauses, and gets the tab's
    /// breakpoints by URL, all before its release.
    #[tokio::test]
    async fn frame_debuggers_are_ready_before_the_frame_runs() {
        use crate::handler::cdp_domains::debugger::frames::ChromeBreakpoint;
        // Given a tab with a breakpoint by URL
        let watched = watched_tab().await;
        let by_url = json!({"url": "http://b/inner.js", "lineNumber": 1});
        let breakpoint = ChromeBreakpoint {
            natives: vec![(None, "B0".into())],
            by_url: Some(by_url.clone()),
        };
        watched
            .sinks
            .debugger
            .lock()
            .await
            .breakpoints
            .add(breakpoint);
        let answer = |_: &Value| json!({"breakpointId": "B1", "locations": []});
        watched
            .mock
            .respond_with("Debugger.setBreakpointByUrl", answer);
        // When an iframe attaches and is released
        attach(&watched, "AUTO-F", "iframe");
        let released = || {
            !watched
                .mock
                .commands_named("Runtime.runIfWaitingForDebugger")
                .is_empty()
        };
        eventually(released).await;
        // Then its debugger was readied in its session before the release
        let frame = frame_session(&watched).await;
        let expected = [
            "Debugger.enable",
            "Debugger.setSkipAllPauses",
            "Debugger.setPauseOnExceptions",
            "Debugger.setAsyncCallStackDepth",
            "Debugger.setBreakpointByUrl",
            "Runtime.runIfWaitingForDebugger",
        ];
        assert_eq!(readied_in(watched.mock, &frame), expected);
        let sent = &watched.mock.commands_named("Debugger.setBreakpointByUrl")[0].params;
        assert_eq!(sent, &by_url);
        // And the breakpoint knows its id there, and the frame is debugged
        let mut state = watched.sinks.debugger.lock().await;
        assert_eq!(state.frame_number(&frame), Some(1));
        let (_, breakpoint) = state.breakpoints.iter_mut().next().unwrap();
        assert_eq!(
            breakpoint.natives[1],
            (Some(frame.clone()), "B1".to_string())
        );
    }

    /// ISSUE-36: a detached frame leaves its tab's debugger.
    #[tokio::test]
    async fn detached_frames_leave_the_debugger() {
        let watched = watched_tab().await;
        attach(&watched, "AUTO-F", "iframe");
        let frame = frame_session(&watched).await;
        let debugged = || {
            watched
                .sinks
                .debugger
                .try_lock()
                .is_ok_and(|s| s.frame_number(&frame).is_some())
        };
        eventually(debugged).await;
        let gone = json!({"sessionId": "AUTO-F", "targetId": "T-iframe"});
        watched.mock.emit(
            "Target.detachedFromTarget",
            gone,
            Some(watched.tab.session_id()),
        );
        let forgotten = || {
            watched
                .sinks
                .debugger
                .try_lock()
                .is_ok_and(|s| s.frames.is_empty())
        };
        eventually(forgotten).await;
        assert!(watched.sinks.debugger.lock().await.frames.is_empty());
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
