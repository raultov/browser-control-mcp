use crate::handler::cdp_domains;
use crate::handler::chrome_instance::tab_registry::TabRegistry;
use cdp_browser_lite::{BrowserClient, CdpResult, WsResponse};
use std::sync::{Arc, RwLock};
use tokio_stream::StreamExt;

/// Brings the active tab to the browser's front. The browser shows one tab
/// per window: the others are hidden (`visibilityState`), run no
/// `requestAnimationFrame`, throttle their timers and Chrome draws no frame
/// of them (an element screenshot waits until it times out). The browser
/// picks its own front tab when a page opens or closes one, so it is set
/// back to the tab the tools act on. Best effort: a failure is logged.
pub(crate) async fn front_active(tabs: &RwLock<TabRegistry>) {
    let active = {
        let registry = crate::util::sync::read(tabs);
        registry.active_tab_id.as_ref().and_then(|id| {
            registry
                .tabs
                .get(id)
                .map(|entry| (id.clone(), entry.tab.clone()))
        })
    };
    if let Some((tab_id, tab)) = active
        && let Err(e) = tab.activate().await
    {
        let error = cdp_domains::event_pump::error_kind(&e);
        tracing::debug!(tab_id, error, "could not bring the active tab to the front");
    }
}

/// Waits (at most `limit`) until the pages Chrome announced are registered,
/// so an action that opened a tab can name it even when its reply comes
/// before the listener attached the page.
pub(crate) async fn registrations_done(tabs: &RwLock<TabRegistry>, limit: std::time::Duration) {
    let deadline = tokio::time::Instant::now() + limit;
    while crate::util::sync::read(tabs).opening > 0 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(REGISTRATION_POLL).await;
    }
}

/// How often [`registrations_done`] looks again.
const REGISTRATION_POLL: std::time::Duration = std::time::Duration::from_millis(20);

/// Makes tab `tab_id` the active one and brings it to the front.
pub(crate) async fn make_active(tabs: &RwLock<TabRegistry>, tab_id: &str) -> Result<(), String> {
    crate::util::sync::write(tabs).switch_tab(tab_id)?;
    front_active(tabs).await;
    Ok(())
}

/// Forgets tab `tab_id` (closed by close_tab, the page or the user) and
/// brings the active tab, maybe another one now, to the front. Returns the
/// active tab id.
pub(crate) async fn forget_tab(tabs: &RwLock<TabRegistry>, tab_id: &str) -> Option<String> {
    crate::util::sync::write(tabs).remove_tab(tab_id);
    front_active(tabs).await;
    crate::util::sync::read(tabs).active_tab_id.clone()
}

pub(crate) fn start_tab_lifecycle_listener(
    browser_client: BrowserClient,
    tabs: Arc<RwLock<TabRegistry>>,
    enable_webmcp: bool,
) -> tokio::task::JoinHandle<()> {
    let mut target_events = browser_client.client().on_domain("Target");
    let bc = browser_client.clone();
    let tabs_clone = tabs.clone();

    tokio::spawn(async move {
        // Enable target discovery to receive targetCreated/targetDestroyed events
        let _ = bc.set_discover_targets(true).await;

        while let Some(item) = target_events.next().await {
            match item {
                Ok(event) => {
                    let _ = process_target_event(&event, &bc, &tabs_clone, enable_webmcp).await;
                }
                Err(e) => {
                    tracing::warn!(
                        error = crate::handler::cdp_domains::event_pump::error_kind(&e),
                        "target lifecycle event stream error"
                    );
                }
            }
        }
    })
}

async fn process_target_event(
    event: &WsResponse,
    browser_client: &BrowserClient,
    tabs: &Arc<RwLock<TabRegistry>>,
    enable_webmcp: bool,
) -> CdpResult<()> {
    let (Some(method), Some(params)) = (event.method.as_deref(), &event.params) else {
        return Ok(());
    };
    match method {
        "Target.targetCreated" => {
            if let Some((target_id, url)) = page_target(params) {
                crate::util::sync::write(tabs).opening += 1;
                on_page_created(browser_client, tabs, (target_id, url), enable_webmcp).await;
                let mut registry = crate::util::sync::write(tabs);
                registry.opening = registry.opening.saturating_sub(1);
            }
        }
        "Target.targetDestroyed" => {
            if let Some(target_id) = params.get("targetId").and_then(|v| v.as_str()) {
                let tab_id = crate::util::sync::read(tabs)
                    .tabs
                    .iter()
                    .find(|(_, entry)| entry.tab.target_id() == target_id)
                    .map(|(id, _)| id.clone());
                if let Some(tab_id) = tab_id {
                    forget_tab(tabs, &tab_id).await;
                }
            }
        }
        _ => {}
    }
    Ok(())
}

/// `(target id, url)` of a created page target, ignoring extension pages.
fn page_target(params: &serde_json::Value) -> Option<(&str, &str)> {
    let info = params.get("targetInfo")?;
    let target_id = info.get("targetId").and_then(|v| v.as_str())?;
    let url = info.get("url").and_then(|v| v.as_str())?;
    let is_page = info.get("type").and_then(|v| v.as_str()) == Some("page");
    (is_page && !url.starts_with("chrome-extension://")).then_some((target_id, url))
}

/// Registers a page the browser opened (unless already known) and starts
/// its listeners.
async fn on_page_created(
    browser_client: &BrowserClient,
    tabs: &Arc<RwLock<TabRegistry>>,
    (target_id, url): (&str, &str),
    enable_webmcp: bool,
) {
    let already_exists = crate::util::sync::read(tabs)
        .tabs
        .values()
        .any(|entry| entry.tab.target_id() == target_id);
    if already_exists {
        return;
    }
    let Ok(tab) = browser_client.attach(target_id).await else {
        return;
    };
    let info = super::tab_registry::NewTab::new(None, url);
    let _ = register_and_listen(tabs, &tab, info, enable_webmcp).await;
}

/// Registers `tab`, then starts its listeners **before** enabling its
/// domains: enabling is what makes Chrome emit its authoritative replay
/// (`WebMCP.enable` re-emits the whole tool list, `Debugger.enable` replays
/// `scriptParsed`, ...). A subscriber created afterwards never sees those
/// events — tokio's broadcast channel delivers nothing retroactively — and
/// nothing re-triggers them once the page is loaded. Attaching to an
/// already-loaded page is even more exposed: the replay is the only way to
/// learn about tools it registered before we arrived.
pub(crate) async fn register_and_listen(
    tabs: &Arc<RwLock<TabRegistry>>,
    tab: &cdp_browser_lite::Tab,
    info: super::tab_registry::NewTab,
    enable_webmcp: bool,
) -> Result<String, String> {
    let (tab_id, states) = {
        let mut registry = crate::util::sync::write(tabs);
        let tab_id = registry.register_tab(tab.clone(), info, enable_webmcp)?;
        // Only the registration whose session the entry kept wires it: the
        // other one (same target, another session) would replace the
        // listeners with ones deaf to the events of the session in use.
        let states = registry
            .tabs
            .get(&tab_id)
            .filter(|e| e.tab.session_id() == tab.session_id())
            .map(|e| e.domain_states());
        (tab_id, states)
    };
    if let Some(states) = states {
        let webmcp_state = states.webmcp.clone();
        let handles = cdp_domains::start_tab_listeners(tab, states);
        crate::util::sync::write(tabs).attach_listeners(&tab_id, handles);
        cdp_domains::enable_tab_domains(tab, &webmcp_state).await;
    }
    Ok(tab_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::chrome_instance::tab_registry::NewTab;
    use std::time::Duration;

    /// TAB-RACE1: `open_tab` and the `Target.targetCreated` listener attach
    /// the same page twice (two sessions). The losing registration must not
    /// replace the winner's listeners: commands go through the winner's
    /// session, so events answered there (`Tracing.tracingComplete`) would
    /// be lost.
    #[tokio::test]
    async fn second_registration_of_a_target_keeps_the_first_listeners() {
        // Given one page attached twice and the first attachment registered
        let port = crate::handler::cdp_domains::tests::spawn_mock_chrome_server().await;
        let browser = BrowserClient::connect(&format!("127.0.0.1:{port}"), Duration::from_secs(2))
            .await
            .expect("connect");
        let first = browser.attach("T-tab-1").await.expect("first attach");
        let second = browser.attach("T-tab-1").await.expect("second attach");
        assert_ne!(first.session_id(), second.session_id());
        let tabs = Arc::new(RwLock::new(TabRegistry::new(8)));
        let id = register_and_listen(&tabs, &first, NewTab::new(None, "about:blank"), false)
            .await
            .expect("first registration");
        let winners = crate::util::sync::read(&tabs).tabs[&id]
            .listeners
            .as_ref()
            .expect("listeners")
            .abort_handles();

        // When the second attachment registers the same target
        let again = register_and_listen(&tabs, &second, NewTab::new(None, "about:blank"), false)
            .await
            .expect("second registration");
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Then it gets the same id and the first listeners keep running
        assert_eq!(again, id);
        assert!(winners.iter().all(|h| !h.is_finished()));
        let registry = crate::util::sync::read(&tabs);
        assert_eq!(registry.tabs[&id].tab.session_id(), first.session_id());
    }

    type Mock = &'static cdp_browser_lite::test_support::mock_devtools::MockDevTools;

    /// A mock Chrome instance with pages `T-page-1` (tab-1) and `T-page-2`
    /// (tab-2) registered, tab-2 active.
    async fn two_tabs() -> (Mock, crate::handler::McpHandler, BrowserClient) {
        use crate::handler::cdp_domains::tests::{mock_handler, spawn_recording_mock};
        let mock = spawn_recording_mock().await;
        let handler = mock_handler(mock.http_port);
        let session = handler.session(None).await.unwrap();
        let addr = format!("127.0.0.1:{}", mock.http_port);
        let browser = BrowserClient::connect(&addr, Duration::from_secs(5))
            .await
            .unwrap();
        for target in ["T-page-1", "T-page-2"] {
            let tab = browser.attach(target).await.unwrap();
            let info = NewTab::new(None, "about:blank");
            crate::util::sync::write(&session.tabs)
                .register_tab(tab, info, false)
                .unwrap();
        }
        crate::util::sync::write(&session.tabs)
            .switch_tab("tab-2")
            .unwrap();
        (mock, handler, browser)
    }

    /// The targets `Target.activateTarget` was sent for, in order.
    fn activated(mock: Mock) -> Vec<String> {
        mock.commands_named("Target.activateTarget")
            .iter()
            .filter_map(|c| c.params["targetId"].as_str().map(str::to_string))
            .collect()
    }

    /// ISSUE-68 (Chrome): closing the active tab (close_tab forgets it
    /// this way) brings the next active one to the browser's front, so its
    /// page is not left hidden.
    #[tokio::test]
    async fn closing_the_active_tab_fronts_the_next_one() {
        // Given tab-1 and the active tab-2
        let (mock, handler, _browser) = two_tabs().await;
        let session = handler.session(None).await.unwrap();
        // When the active tab is forgotten after its close
        let active = forget_tab(&session.tabs, "tab-2").await;
        // Then tab-1 is active and in front
        assert_eq!(active.as_deref(), Some("tab-1"));
        assert_eq!(activated(mock).last().map(String::as_str), Some("T-page-1"));
    }

    /// ISSUE-68 (Chrome): a click that opens a tab waits for the listener
    /// to register the pages Chrome announced, so the reply names the tab
    /// (and the opener is brought back to the front), within a bound.
    #[tokio::test]
    async fn opened_pages_are_awaited_until_registered() {
        // Given a page Chrome announced, registered 60 ms later
        let tabs = Arc::new(RwLock::new(TabRegistry::new(8)));
        crate::util::sync::write(&tabs).opening = 1;
        let later = tabs.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(60)).await;
            crate::util::sync::write(&later).opening = 0;
        });
        // When the click waits for registrations
        let start = std::time::Instant::now();
        registrations_done(&tabs, Duration::from_secs(2)).await;
        // Then it waited for that one, not for the bound
        let waited = start.elapsed();
        assert!(waited >= Duration::from_millis(60), "{waited:?}");
        assert!(waited < Duration::from_secs(1), "{waited:?}");
        // And a registration that never ends costs only the bound
        crate::util::sync::write(&tabs).opening = 1;
        let start = std::time::Instant::now();
        registrations_done(&tabs, Duration::from_millis(100)).await;
        assert!(start.elapsed() < Duration::from_millis(500));
    }

    /// ISSUE-68 (Chrome): a tab open_tab created becomes the active one,
    /// in front, as the browser shows it.
    #[tokio::test]
    async fn an_opened_tab_becomes_active_in_front() {
        // Given tab-1 and the active tab-2
        let (mock, handler, _browser) = two_tabs().await;
        let session = handler.session(None).await.unwrap();
        // When tab-1 is made active
        make_active(&session.tabs, "tab-1").await.unwrap();
        // Then it is active and in front
        let active = crate::util::sync::read(&session.tabs).active_tab_id.clone();
        assert_eq!(active.as_deref(), Some("tab-1"));
        assert_eq!(activated(mock).last().map(String::as_str), Some("T-page-1"));
    }

    /// ISSUE-68 (Chrome): a tab the page or the user closed brings the
    /// active one back to the front.
    #[tokio::test]
    async fn a_tab_closed_by_the_page_fronts_the_active_one() {
        // Given tab-1 and the active tab-2
        let (mock, handler, browser) = two_tabs().await;
        let session = handler.session(None).await.unwrap();
        // When Chrome reports tab-2's page destroyed
        let event: WsResponse = serde_json::from_value(serde_json::json!({
            "method": "Target.targetDestroyed", "params": {"targetId": "T-page-2"}
        }))
        .unwrap();
        process_target_event(&event, &browser, &session.tabs, false)
            .await
            .unwrap();
        // Then tab-1 is active and in front
        let active = crate::util::sync::read(&session.tabs).active_tab_id.clone();
        assert_eq!(active.as_deref(), Some("tab-1"));
        assert_eq!(activated(mock).last().map(String::as_str), Some("T-page-1"));
    }
}
