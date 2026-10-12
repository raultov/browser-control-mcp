use crate::handler::cdp_domains::event_pump::ListenerHandles;
use crate::handler::cdp_domains::log::LogState;
use crate::handler::cdp_domains::tracing::TracingState;
use crate::handler::cdp_domains::webmcp::{WebmcpAvailability, WebmcpState};
use crate::handler::{CustomState, DebuggerState, NetworkState};
use cdp_browser_lite::Tab;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

pub type TabId = String;

/// Per-tab domain state handles needed to wire the CDP listeners.
pub(crate) type TabDomainStates = (
    Arc<Mutex<DebuggerState>>,
    Arc<Mutex<NetworkState>>,
    Arc<Mutex<LogState>>,
    Arc<Mutex<TracingState>>,
    Arc<Mutex<WebmcpState>>,
);

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
    pub(crate) listeners: Option<ListenerHandles>,
}

impl TabEntry {
    pub(crate) fn domain_states(&self) -> TabDomainStates {
        (
            self.debugger_state.clone(),
            self.network_state.clone(),
            self.log_state.clone(),
            self.tracing_state.clone(),
            self.webmcp_state.clone(),
        )
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
            .iter_mut()
            .find(|(_, entry)| entry.tab.target_id() == tab.target_id());
        if let Some((id, entry)) = existing {
            if label.is_some() {
                entry.label = label;
            }
            return Ok(id.clone());
        }
        if self.tabs.len() >= self.max_tabs {
            return Err(format!("Tab limit reached ({})", self.max_tabs));
        }

        if let Some(ref lbl) = label {
            for entry in self.tabs.values() {
                if entry.label.as_ref() == Some(lbl) {
                    return Err(format!("Label '{}' is already in use by another tab", lbl));
                }
            }
        }

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
            listeners: None,
        };

        self.tabs.insert(tab_id.clone(), entry);
        if self.active_tab_id.is_none() {
            self.activate(&tab_id);
        }

        Ok(tab_id)
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
}
