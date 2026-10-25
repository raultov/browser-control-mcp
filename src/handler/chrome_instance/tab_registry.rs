use crate::handler::cdp_domains::event_pump::ListenerHandles;
use crate::handler::cdp_domains::log::LogState;
use crate::handler::cdp_domains::tracing::TracingState;
use crate::handler::cdp_domains::webmcp::{WebmcpAvailability, WebmcpState};
use crate::handler::dialogs::DialogState;
use crate::handler::{CustomState, DebuggerState, NetworkState};
use cdp_browser_lite::Tab;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

pub type TabId = String;

/// Per-tab domain state handles needed to wire the CDP listeners.
pub(crate) struct TabDomainStates {
    pub(crate) debugger: Arc<Mutex<DebuggerState>>,
    pub(crate) network: Arc<Mutex<NetworkState>>,
    pub(crate) log: Arc<Mutex<LogState>>,
    pub(crate) tracing: Arc<Mutex<TracingState>>,
    pub(crate) webmcp: Arc<Mutex<WebmcpState>>,
    pub(crate) dialog: Arc<Mutex<DialogState>>,
}

pub(crate) struct TabEntry {
    pub(crate) tab: Tab,
    pub(crate) label: Option<String>,
    pub(crate) url: String,
    pub(crate) debugger_state: Arc<Mutex<DebuggerState>>,
    pub(crate) network_state: Arc<Mutex<NetworkState>>,
    pub(crate) log_state: Arc<Mutex<LogState>>,
    pub(crate) tracing_state: Arc<Mutex<TracingState>>,
    pub(crate) custom_state: Arc<Mutex<CustomState>>,
    pub(crate) webmcp_state: Arc<Mutex<WebmcpState>>,
    pub(crate) dialog_state: Arc<Mutex<DialogState>>,
    pub(crate) listeners: Option<ListenerHandles>,
}

impl TabEntry {
    pub(crate) fn domain_states(&self) -> TabDomainStates {
        TabDomainStates {
            debugger: self.debugger_state.clone(),
            network: self.network_state.clone(),
            log: self.log_state.clone(),
            tracing: self.tracing_state.clone(),
            webmcp: self.webmcp_state.clone(),
            dialog: self.dialog_state.clone(),
        }
    }
}

/// What the caller knows about a tab it registers.
pub(crate) struct NewTab {
    /// Optional unique label chosen by the agent.
    pub(crate) label: Option<String>,
    /// URL at registration time.
    pub(crate) url: String,
}

impl NewTab {
    pub(crate) fn new(label: Option<String>, url: impl Into<String>) -> Self {
        Self {
            label,
            url: url.into(),
        }
    }
}

pub(crate) struct TabRegistry {
    pub(crate) tabs: HashMap<TabId, TabEntry>,
    pub(crate) active_tab_id: Option<TabId>,
    pub(crate) max_tabs: usize,
    pub(crate) counter: usize,
    /// Tabs in the order they were last made active (most recent last), so
    /// closing the active tab falls back to the previous one.
    recent: Vec<TabId>,
}

impl TabRegistry {
    pub(crate) fn new(max_tabs: usize) -> Self {
        Self {
            tabs: HashMap::new(),
            active_tab_id: None,
            max_tabs,
            counter: 1,
            recent: Vec::new(),
        }
    }

    pub(crate) fn register_tab(
        &mut self,
        tab: Tab,
        info: NewTab,
        enable_webmcp: bool,
    ) -> Result<TabId, String> {
        let NewTab { label, url } = info;
        // `open_tab` and the `Target.targetCreated` listener race to register
        // the same target: the second one gets the existing id.
        let existing = self
            .tabs
            .iter()
            .find(|(_, entry)| entry.tab.target_id() == tab.target_id())
            .map(|(id, _)| id.clone());
        if let Some(id) = existing {
            if label.is_some() {
                self.check_label(label.as_deref(), Some(&id))?;
                if let Some(entry) = self.tabs.get_mut(&id) {
                    entry.label = label;
                }
            }
            return Ok(id);
        }
        self.check_new(label.as_deref())?;

        let tab_id = format!("tab-{}", self.counter);
        self.counter += 1;

        let webmcp_st = WebmcpState {
            availability: WebmcpAvailability::initial(enable_webmcp),
            ..Default::default()
        };

        let entry = TabEntry {
            tab,
            label,
            url,
            debugger_state: Arc::new(Mutex::new(DebuggerState::default())),
            network_state: Arc::new(Mutex::new(NetworkState::default())),
            log_state: Arc::new(Mutex::new(LogState::default())),
            tracing_state: Arc::new(Mutex::new(TracingState::default())),
            custom_state: Arc::new(Mutex::new(CustomState::default())),
            webmcp_state: Arc::new(Mutex::new(webmcp_st)),
            dialog_state: Default::default(),
            listeners: None,
        };

        self.tabs.insert(tab_id.clone(), entry);
        if self.active_tab_id.is_none() {
            self.activate(&tab_id);
        }

        Ok(tab_id)
    }

    /// Whether a new tab labelled `label` may be registered (under the cap,
    /// label unused). `open_tab` checks before creating the page.
    pub(crate) fn check_new(&self, label: Option<&str>) -> Result<(), String> {
        if self.tabs.len() >= self.max_tabs {
            return Err(format!("Tab limit reached ({})", self.max_tabs));
        }
        self.check_label(label, None)
    }

    /// Refuses `label` if a tab other than `except` uses it.
    fn check_label(&self, label: Option<&str>, except: Option<&str>) -> Result<(), String> {
        let labels = self
            .tabs
            .iter()
            .map(|(id, e)| (id.as_str(), e.label.as_deref()));
        crate::handler::chrome_instance::tab_registry::check_label(labels, label, except)
    }

    pub(crate) fn attach_listeners(&mut self, tab_id: &str, handles: ListenerHandles) {
        if let Some(entry) = self.tabs.get_mut(tab_id) {
            entry.listeners = Some(handles);
        }
    }

    pub(crate) fn remove_tab(&mut self, tab_id: &str) -> Option<TabEntry> {
        let removed = self.tabs.remove(tab_id);
        self.recent.retain(|id| id != tab_id);
        if removed.is_some() && self.active_tab_id.as_deref() == Some(tab_id) {
            // The most recently used remaining tab, else any remaining one.
            self.active_tab_id = self
                .recent
                .last()
                .cloned()
                .or_else(|| self.tabs.keys().next().cloned());
        }
        removed
    }

    pub(crate) fn switch_tab(&mut self, tab_id: &str) -> Result<(), String> {
        if !self.tabs.contains_key(tab_id) {
            return Err(format!("Tab with ID '{}' not found", tab_id));
        }
        self.activate(tab_id);
        Ok(())
    }

    fn activate(&mut self, tab_id: &str) {
        self.recent.retain(|id| id != tab_id);
        self.recent.push(tab_id.to_string());
        self.active_tab_id = Some(tab_id.to_string());
    }

    pub(crate) fn clear(&mut self) {
        self.tabs.clear();
        self.recent.clear();
        self.active_tab_id = None;
    }
}

/// Refuses `label` if a tab other than `except` uses it; `labels` yields
/// `(tab id, label)` (shared by the Chrome and Firefox registries).
pub(crate) fn check_label<'a>(
    mut labels: impl Iterator<Item = (&'a str, Option<&'a str>)>,
    label: Option<&str>,
    except: Option<&str>,
) -> Result<(), String> {
    let Some(label) = label else {
        return Ok(());
    };
    if labels.any(|(id, l)| Some(id) != except && l == Some(label)) {
        return Err(format!("Label '{label}' is already in use by another tab"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FIX-08: Closing the active tab activates the most recently used one.
    #[tokio::test]
    async fn closing_active_tab_activates_previous() {
        // Given tabs tab-1, tab-2, tab-3 used in that order and tab-3 active
        let port = crate::handler::cdp_domains::tests::spawn_mock_chrome_server().await;
        let address = format!("127.0.0.1:{port}");
        let timeout = std::time::Duration::from_secs(5);
        let browser = cdp_browser_lite::BrowserClient::connect(&address, timeout)
            .await
            .unwrap();
        let mut registry = TabRegistry::new(16);
        for _ in 0..3 {
            let tab = browser.new_tab("about:blank").await.unwrap();
            registry
                .register_tab(tab, NewTab::new(None, "about:blank"), false)
                .unwrap();
        }
        for id in ["tab-1", "tab-2", "tab-3"] {
            registry.switch_tab(id).unwrap();
        }
        // When tab-3 is closed
        registry.remove_tab("tab-3");
        // Then tab-2 becomes active
        assert_eq!(registry.active_tab_id.as_deref(), Some("tab-2"));
    }

    /// FIX-10: A tab opened with open_tab is registered once.
    #[tokio::test]
    async fn open_tab_registers_once() {
        let port = crate::handler::cdp_domains::tests::spawn_mock_chrome_server().await;
        let address = format!("127.0.0.1:{port}");
        let timeout = std::time::Duration::from_secs(5);
        let browser = cdp_browser_lite::BrowserClient::connect(&address, timeout)
            .await
            .unwrap();
        let mut registry = TabRegistry::new(16);
        // Given the lifecycle listener registered the target first
        let tab = browser.attach("T-page-1").await.unwrap();
        let first = registry
            .register_tab(tab, NewTab::new(None, "about:blank"), false)
            .unwrap();
        // When open_tab registers the same target with a label
        let tab = browser.attach("T-page-1").await.unwrap();
        let info = NewTab::new(Some("mine".into()), "about:blank");
        let second = registry.register_tab(tab, info, false).unwrap();
        // Then there is one entry, with the label
        assert_eq!(first, second);
        assert_eq!(registry.tabs.len(), 1);
        assert_eq!(registry.tabs[&first].label.as_deref(), Some("mine"));
    }

    /// REV-06: a label taken by another tab is refused on the
    /// already-registered path too, and `check_new` refuses it up front.
    #[tokio::test]
    async fn labels_stay_unique_when_a_target_registers_twice() {
        // Given tab-1 labelled "x" and tab-2 registered by target discovery
        let port = crate::handler::cdp_domains::tests::spawn_mock_chrome_server().await;
        let browser = cdp_browser_lite::BrowserClient::connect(
            &format!("127.0.0.1:{port}"),
            std::time::Duration::from_secs(5),
        )
        .await
        .unwrap();
        let mut registry = TabRegistry::new(16);
        let first = browser.attach("T-page-1").await.unwrap();
        registry
            .register_tab(first, NewTab::new(Some("x".into()), "about:blank"), false)
            .unwrap();
        let second = browser.attach("T-page-2").await.unwrap();
        registry
            .register_tab(second, NewTab::new(None, "about:blank"), false)
            .unwrap();
        // When open_tab registers T-page-2 again with the label "x"
        let again = browser.attach("T-page-2").await.unwrap();
        let taken =
            registry.register_tab(again, NewTab::new(Some("x".into()), "about:blank"), false);
        // Then it is refused, and so is a new tab with that label
        assert!(taken.is_err());
        assert!(registry.check_new(Some("x")).is_err());
        assert!(registry.check_new(Some("y")).is_ok());
    }
}
