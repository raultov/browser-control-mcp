//! The first requests of a page another page opened, on Chrome (#141).
//!
//! A popup (`window.open`, a `target=_blank` link) starts loading before
//! the server can attach to it from `Target.targetCreated`: its document
//! and first scripts went unrecorded. The browser connection auto-attaches
//! to every new page instead (`Target.setAutoAttach` with
//! `waitForDebuggerOnStart`), each page paused until released. For a page
//! with an opener, `Network` is enabled on that auto-attached session and
//! the page released in the same breath (Chrome runs a session's commands
//! in order, and answers `Network.enable` of a popup in a new process only
//! once it runs, so the reply is not awaited); its events feed the tab's
//! network log until the tab's own session records them too, then a short
//! grace. The session stays attached: the bodies of those first requests
//! are only kept there. Any other page is released and its auto-attached
//! session detached at once. Every page is released on every path: one
//! left waiting would never load.

use crate::handler::NetworkState;
use crate::handler::cdp_domains::event_pump::ListenerHandles;
use crate::handler::chrome_instance::tab_registry::TabRegistry;
use cdp_browser_lite::{CdpClient, EventFilter, Tab, WsResponse};
use serde_json::{Value, json};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::Mutex;
use tokio_stream::StreamExt;

/// How long a popup's auto-attached session waits for its tab to be
/// registered (by the target-discovery listener) before it is detached.
const REGISTRATION_WAIT: Duration = Duration::from_secs(5);

/// How often the registry is looked at while waiting.
const REGISTRATION_POLL: Duration = Duration::from_millis(20);

/// How long the auto-attached session's events still feed the tab once its
/// own session records requests: the events of requests started before
/// reach both, and the duplicates are recognised (see `on_request`).
const GRACE: Duration = Duration::from_secs(1);

/// The auto-attach asked of the browser connection: pages only, each paused
/// until released.
fn auto_attach() -> Value {
    json!({
        "autoAttach": true,
        "waitForDebuggerOnStart": true,
        "flatten": true,
        "filter": [{"type": "page"}],
    })
}

/// Buffers of the auto-attached session (it stays attached for its first
/// requests' bodies): smaller than Chrome's defaults.
fn network_buffers() -> Value {
    json!({"maxTotalBufferSize": 10_000_000, "maxResourceBufferSize": 5_000_000})
}

/// Watches the pages the browser of `client` opens from now on (see the
/// module docs), feeding the network log of their tabs in `tabs`. The
/// events are subscribed before the auto-attach is asked for.
pub(crate) fn start_page_watcher(
    client: CdpClient,
    tabs: Arc<RwLock<TabRegistry>>,
) -> tokio::task::JoinHandle<()> {
    let events = client.on_domain("Target");
    tokio::spawn(async move {
        let _off = AutoAttachOff(client.clone());
        if let Err(e) = client
            .send_raw_command("Target.setAutoAttach", auto_attach())
            .await
        {
            let error = crate::handler::cdp_domains::event_pump::error_kind(&e);
            tracing::debug!(error, "Chrome refused to auto-attach to new pages");
        }
        watch(client, events, tabs).await;
    })
}

/// Turns the auto-attach off when the watcher ends (aborted with the
/// listeners of a closed connection, ISSUE-101): the browser connection
/// may outlive it, and a page it opened then would wait for a release that
/// never comes. Turning it off detaches the auto-attached sessions, which
/// releases their pages. Written without waiting: a drop cannot.
struct AutoAttachOff(CdpClient);

impl Drop for AutoAttachOff {
    fn drop(&mut self) {
        // Chrome refuses a browser-level auto-attach without `flatten`.
        let off = json!({"autoAttach": false, "waitForDebuggerOnStart": false, "flatten": true});
        let _ = self
            .0
            .send_raw_command_no_reply("Target.setAutoAttach", off);
    }
}

/// Handles the browser connection's `Target.attachedToTarget` events of
/// pages waiting to run, each in a task of its own (the loop never waits,
/// so no page waits for another), owned here.
async fn watch(client: CdpClient, mut events: EventFilter, tabs: Arc<RwLock<TabRegistry>>) {
    let mut pages = ListenerHandles::default();
    while let Some(event) = events.next().await {
        let Ok(event) = event else { continue };
        let Some(waiting) = waiting_page(&event) else {
            continue;
        };
        pages.retain_running();
        let setup = set_up(client.clone(), tabs.clone(), waiting);
        pages.push(tokio::spawn(setup));
    }
}

/// A page auto-attached and waiting to run, as `Target.attachedToTarget`
/// on the browser connection announces it.
#[derive(Debug, PartialEq)]
struct Waiting {
    /// The auto-attached session.
    session: String,
    target_id: String,
    /// Whether another page opened it: only then are its first requests
    /// recorded through the auto-attached session.
    opened: bool,
}

/// The waiting page `event` announces, if it is a `Target.attachedToTarget`
/// of the browser connection (not of a tab's session, whose frames are
/// handled in `frame_watch`) waiting to run. Explicit attaches never wait.
fn waiting_page(event: &WsResponse) -> Option<Waiting> {
    if event.method.as_deref() != Some("Target.attachedToTarget") || event.session_id.is_some() {
        return None;
    }
    let params = event.params.as_ref()?;
    if params["waitingForDebugger"] != json!(true) {
        return None;
    }
    let info = &params["targetInfo"];
    Some(Waiting {
        session: params["sessionId"].as_str()?.to_string(),
        target_id: info["targetId"].as_str()?.to_string(),
        opened: info["type"] == "page" && info["openerId"].as_str().is_some(),
    })
}

/// Releases the page `waiting` describes; a page another page opened has
/// `Network` enabled first and its first requests recorded, any other is
/// released and its auto-attached session detached.
async fn set_up(client: CdpClient, tabs: Arc<RwLock<TabRegistry>>, waiting: Waiting) {
    let session = waiting.session.as_str();
    tracing::debug!(
        target_id = waiting.target_id,
        opened = waiting.opened,
        "releasing a new page"
    );
    if !waiting.opened {
        release(&client, session).await;
        detach(&client, session).await;
        return;
    }
    // Subscribed first: the page's first events follow the release.
    let mut events = client.on_domain_for_session("Network", session);
    let _ =
        client.send_raw_command_to_session_no_reply(session, "Network.enable", network_buffers());
    release(&client, session).await;
    let Some((tab, network)) = registered(&tabs, &waiting.target_id).await else {
        // Not tracked (at the instance's cap, or closed at once).
        detach(&client, session).await;
        return;
    };
    let handoff = async move {
        let _ = tab.send_raw_command("Network.enable", json!({})).await;
    };
    feed(&mut events, &network, session, handoff).await;
}

/// The tab of target `target_id` and its network log, once registered
/// (at most [`REGISTRATION_WAIT`]).
async fn registered(
    tabs: &RwLock<TabRegistry>,
    target_id: &str,
) -> Option<(Tab, Arc<Mutex<NetworkState>>)> {
    let deadline = tokio::time::Instant::now() + REGISTRATION_WAIT;
    loop {
        let found = crate::util::sync::read(tabs)
            .tabs
            .values()
            .find(|entry| entry.tab.target_id() == target_id)
            .map(|entry| (entry.tab.clone(), entry.network_state.clone()));
        if found.is_some() || tokio::time::Instant::now() >= deadline {
            return found;
        }
        tokio::time::sleep(REGISTRATION_POLL).await;
    }
}

/// Feeds `network` with the events of the auto-attached `session` until
/// `handoff` (the tab's own session enabling `Network`) is done, then for
/// [`GRACE`], without WebSocket frames then (those are not recognised as
/// duplicates). The requests note `session`, where their bodies are kept.
async fn feed(
    events: &mut EventFilter,
    network: &Arc<Mutex<NetworkState>>,
    session: &str,
    handoff: impl std::future::Future<Output = ()>,
) {
    let mut handoff = std::pin::pin!(handoff);
    let mut until: Option<tokio::time::Instant> = None;
    loop {
        let event = tokio::select! {
            () = &mut handoff, if until.is_none() => {
                until = Some(tokio::time::Instant::now() + GRACE);
                continue;
            }
            () = sleep_until(until) => return,
            event = events.next() => event,
        };
        match event {
            Some(Ok(event)) if until.is_none() || !is_frame(&event) => {
                let session = session.to_string();
                crate::handler::cdp_domains::network::process_frame_event(
                    event,
                    network.clone(),
                    session,
                )
                .await;
            }
            Some(_) => {}
            None => return,
        }
    }
}

/// Sleeps until `until`, or forever when there is none yet.
async fn sleep_until(until: Option<tokio::time::Instant>) {
    match until {
        Some(until) => tokio::time::sleep_until(until).await,
        None => std::future::pending().await,
    }
}

/// Whether `event` is a WebSocket frame.
fn is_frame(event: &WsResponse) -> bool {
    event
        .method
        .as_deref()
        .is_some_and(|m| m.starts_with("Network.webSocketFrame"))
}

/// Lets the page of the auto-attached `session` run.
async fn release(client: &CdpClient, session: &str) {
    let _ = client
        .send_raw_command_to_session(session, "Runtime.runIfWaitingForDebugger", json!({}))
        .await;
}

/// Ends the auto-attached `session` (not the page).
async fn detach(client: &CdpClient, session: &str) {
    let _ = client
        .send_raw_command("Target.detachFromTarget", json!({"sessionId": session}))
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::cdp_domains::tests::spawn_recording_mock;
    use crate::handler::chrome_instance::tab_registry::NewTab;
    use cdp_browser_lite::BrowserClient;
    use cdp_browser_lite::test_support::mock_devtools::MockDevTools;

    /// Waits (at most 2 s) until `done` holds.
    async fn eventually(done: impl Fn() -> bool) {
        for _ in 0..200 {
            if done() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// The `Target.attachedToTarget` params of a page waiting to run.
    fn attached(session: &str, target: &str, opener: Option<&str>) -> Value {
        let mut info = json!({"targetId": target, "type": "page", "url": ""});
        if let Some(opener) = opener {
            info["openerId"] = json!(opener);
        }
        json!({"sessionId": session, "targetInfo": info, "waitingForDebugger": true})
    }

    /// The methods the mock received on `session`, in order.
    fn sent_to(mock: &MockDevTools, session: &str) -> Vec<String> {
        mock.commands()
            .into_iter()
            .filter(|c| c.session_id.as_deref() == Some(session))
            .map(|c| c.method)
            .collect()
    }

    /// The browser connection to the mock, a registry with tab `T-pop`
    /// registered, and the page watcher, once it asked for the auto-attach.
    async fn watching() -> (
        &'static MockDevTools,
        Arc<RwLock<TabRegistry>>,
        tokio::task::JoinHandle<()>,
    ) {
        let mock = spawn_recording_mock().await;
        let address = format!("127.0.0.1:{}", mock.http_port);
        let browser = BrowserClient::connect(&address, Duration::from_secs(2))
            .await
            .unwrap();
        let tabs = Arc::new(RwLock::new(TabRegistry::new(8)));
        let tab = browser.attach("T-pop").await.unwrap();
        let id = crate::util::sync::write(&tabs)
            .register_tab(tab, NewTab::new(None, ""), false)
            .unwrap();
        assert_eq!(id, "tab-1");
        let watcher = start_page_watcher(browser.client().clone(), tabs.clone());
        eventually(|| !mock.commands_named("Target.setAutoAttach").is_empty()).await;
        (mock, tabs, watcher)
    }

    /// #141: the browser connection auto-attaches to new pages, each paused
    /// until released.
    #[tokio::test]
    async fn new_pages_are_auto_attached_paused() {
        // Given the page watcher
        let (mock, _tabs, _watcher) = watching().await;
        // Then the browser connection asked to auto-attach to pages, paused
        let asked = mock.commands_named("Target.setAutoAttach");
        assert_eq!(asked[0].session_id, None);
        assert_eq!(asked[0].params, auto_attach());
    }

    /// #141 (ISSUE-101): when the watcher ends with the listeners of a
    /// closed connection, the auto-attach is turned off (flattened, as
    /// Chrome requires), so a page opened afterwards is not left waiting.
    #[tokio::test]
    async fn the_auto_attach_ends_with_the_watcher() {
        // Given the page watcher
        let (mock, _tabs, watcher) = watching().await;
        // When it is aborted
        watcher.abort();
        eventually(|| mock.commands_named("Target.setAutoAttach").len() == 2).await;
        // Then the auto-attach is turned off
        let off = &mock.commands_named("Target.setAutoAttach")[1];
        let expected =
            json!({"autoAttach": false, "waitForDebuggerOnStart": false, "flatten": true});
        assert_eq!((off.session_id.as_deref(), &off.params), (None, &expected));
    }

    /// #141: a popup's auto-attached session enables `Network` before the
    /// popup is released, and its first request (the document) reaches the
    /// tab's network log, noting the session where its body is kept.
    #[tokio::test]
    async fn popups_record_their_first_requests() {
        // Given the page watcher and a registered tab T-pop
        let (mock, tabs, _watcher) = watching().await;
        // When Chrome announces T-pop, opened by a page, waiting to run
        mock.emit(
            "Target.attachedToTarget",
            attached("AUTO-1", "T-pop", Some("T-opener")),
            None,
        );
        eventually(|| {
            !mock
                .commands_named("Runtime.runIfWaitingForDebugger")
                .is_empty()
        })
        .await;
        // Then Network was enabled on its session, then it was released
        let sent = sent_to(mock, "AUTO-1");
        assert_eq!(sent, ["Network.enable", "Runtime.runIfWaitingForDebugger"]);
        // And its document's request reaches the tab's log
        let request = json!({"requestId": "D1", "timestamp": 1.5,
            "request": {"url": "http://a/popup.html", "method": "GET", "headers": {}}});
        mock.emit("Network.requestWillBeSent", request, Some("AUTO-1"));
        let network = crate::util::sync::read(&tabs).tabs["tab-1"]
            .network_state
            .clone();
        for _ in 0..200 {
            if network.lock().await.find_request_mut("D1").is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let mut state = network.lock().await;
        let logged = state
            .find_request_mut("D1")
            .map(|r| (r.url.clone(), r.session.clone()));
        assert_eq!(
            logged,
            Some((
                "http://a/popup.html".to_string(),
                Some("AUTO-1".to_string())
            ))
        );
    }

    /// #141: a page no other page opened (open_tab's, the user's) is
    /// released and its auto-attached session detached at once; a waiting
    /// target of a tab's session (a frame) is left to `frame_watch`.
    #[tokio::test]
    async fn other_pages_are_released_and_detached() {
        // Given the page watcher
        let (mock, _tabs, _watcher) = watching().await;
        // When Chrome announces a page without opener, and a tab's frame
        mock.emit(
            "Target.attachedToTarget",
            attached("AUTO-F", "T-frame", None),
            Some("TAB-S"),
        );
        mock.emit(
            "Target.attachedToTarget",
            attached("AUTO-2", "T-new", None),
            None,
        );
        eventually(|| !mock.commands_named("Target.detachFromTarget").is_empty()).await;
        // Then the page is released and its session detached, Network untouched
        assert_eq!(sent_to(mock, "AUTO-2"), ["Runtime.runIfWaitingForDebugger"]);
        let detached = &mock.commands_named("Target.detachFromTarget")[0];
        assert_eq!(detached.params["sessionId"], "AUTO-2");
        // And the frame is not touched here
        assert!(sent_to(mock, "AUTO-F").is_empty());
    }

    /// #141: only the browser connection's announcements of pages waiting
    /// to run are handled: explicit attaches (not waiting) and tabs'
    /// frames are not.
    #[test]
    fn only_waiting_pages_of_the_browser_are_handled() {
        let event = |params: Value, session: Option<&str>| {
            let mut event: WsResponse = serde_json::from_value(
                json!({"method": "Target.attachedToTarget", "params": params}),
            )
            .unwrap();
            event.session_id = session.map(str::to_string);
            event
        };
        let popup = attached("S1", "T1", Some("T0"));
        assert_eq!(
            waiting_page(&event(popup.clone(), None)),
            Some(Waiting {
                session: "S1".into(),
                target_id: "T1".into(),
                opened: true
            })
        );
        assert_eq!(waiting_page(&event(popup.clone(), Some("TAB"))), None);
        let mut explicit = popup;
        explicit["waitingForDebugger"] = json!(false);
        assert_eq!(waiting_page(&event(explicit, None)), None);
        let fresh = waiting_page(&event(attached("S2", "T2", None), None));
        assert_eq!(fresh.map(|w| w.opened), Some(false));
    }
}
