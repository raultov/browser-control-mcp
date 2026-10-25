use crate::handler::cdp_domains;
use crate::handler::chrome_instance::tab_registry::TabRegistry;
use cdp_browser_lite::{BrowserClient, CdpResult, WsResponse};
use std::sync::{Arc, RwLock};
use tokio_stream::StreamExt;

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
                on_page_created(browser_client, tabs, (target_id, url), enable_webmcp).await;
            }
        }
        "Target.targetDestroyed" => {
            if let Some(target_id) = params.get("targetId").and_then(|v| v.as_str()) {
                let mut registry = crate::util::sync::write(tabs);
                let tab_id = registry
                    .tabs
                    .iter()
                    .find(|(_, entry)| entry.tab.target_id() == target_id)
                    .map(|(id, _)| id.clone());
                if let Some(tab_id) = tab_id {
                    registry.remove_tab(&tab_id);
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
}
