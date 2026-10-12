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
        let states = registry.tabs.get(&tab_id).map(|e| e.domain_states());
        (tab_id, states)
    };
    if let Some(states) = states {
        let webmcp_state = states.4.clone();
        let handles = cdp_domains::start_tab_listeners(tab, states);
        crate::util::sync::write(tabs).attach_listeners(&tab_id, handles);
        cdp_domains::enable_tab_domains(tab, &webmcp_state).await;
    }
    Ok(tab_id)
}
