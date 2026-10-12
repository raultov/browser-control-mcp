//! One Firefox instance: its `Browser`, its tabs and their buffered events.

use super::launch::FirefoxSpec;
use super::{lifecycle_error, protocol_error};
use crate::backend::error::BackendError;
use crate::handler::cdp_domains::event_pump::ListenerHandles;
use crate::handler::cdp_domains::log::LogState;
use crate::handler::{CustomState, NetworkState};
use crate::util::sync::{read, write};
use bidi_browser_lite::{Browser, Tab};
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use tokio::sync::Mutex;

/// Largest response body Firefox keeps for `get_network_logs` (bytes).
const BODY_LIMIT: u64 = 2_000_000;

/// Most tabs per Firefox instance (as for Chrome).
const MAX_TABS: usize = 16;

/// Buffered state of one tab (a top-level browsing context).
#[derive(Clone)]
pub(crate) struct FirefoxTab {
    /// BiDi handle of the context.
    pub(crate) tab: Tab,
    /// Label chosen by the agent.
    pub(crate) label: Option<String>,
    /// Last URL seen (navigation and load events keep it current).
    pub(crate) url: String,
    /// Console messages.
    pub(crate) log: Arc<Mutex<LogState>>,
    /// Requests, by navigation.
    pub(crate) network: Arc<Mutex<NetworkState>>,
    /// Events of modules the agent subscribed to.
    pub(crate) custom: Arc<Mutex<CustomState>>,
}

/// Tabs by MCP id, plus the context tree used to route iframe events.
#[derive(Default)]
pub(crate) struct FirefoxTabs {
    pub(crate) tabs: BTreeMap<String, FirefoxTab>,
    pub(crate) active: Option<String>,
    counter: usize,
    /// Child context → parent context (iframes).
    pub(crate) parents: HashMap<String, String>,
}

impl FirefoxTabs {
    /// The MCP id of the tab showing `context` (or one of its iframes).
    pub(crate) fn tab_of(&self, context: &str) -> Option<String> {
        let mut current = context;
        // Bounded walk: a malformed cycle cannot hang the pump.
        for _ in 0..32 {
            if let Some((id, _)) = self
                .tabs
                .iter()
                .find(|(_, t)| t.tab.context_id() == current)
            {
                return Some(id.clone());
            }
            current = self.parents.get(current)?;
        }
        None
    }

    /// Registers a tab (or returns the id it already has, updating its label).
    pub(crate) fn register(
        &mut self,
        tab: Tab,
        label: Option<String>,
        url: &str,
    ) -> Result<String, String> {
        if let Some(id) = self.tab_of_top(tab.context_id()) {
            if let (Some(label), Some(entry)) = (label, self.tabs.get_mut(&id)) {
                entry.label = Some(label);
            }
            return Ok(id);
        }
        if self.tabs.len() >= MAX_TABS {
            return Err(format!("Tab limit reached ({MAX_TABS})"));
        }
        if label.is_some() && self.tabs.values().any(|t| t.label == label) {
            return Err(format!(
                "Label '{}' is already in use by another tab",
                label.unwrap_or_default()
            ));
        }
        self.counter += 1;
        let id = format!("tab-{}", self.counter);
        let entry = FirefoxTab {
            tab,
            label,
            url: url.to_string(),
            log: Default::default(),
            network: Default::default(),
            custom: Default::default(),
        };
        self.tabs.insert(id.clone(), entry);
        self.active.get_or_insert_with(|| id.clone());
        Ok(id)
    }

    fn tab_of_top(&self, context: &str) -> Option<String> {
        self.tabs
            .iter()
            .find(|(_, t)| t.tab.context_id() == context)
            .map(|(id, _)| id.clone())
    }

    /// Forgets a tab; the active tab falls back to another one.
    pub(crate) fn remove(&mut self, id: &str) -> Option<FirefoxTab> {
        let removed = self.tabs.remove(id);
        if self.active.as_deref() == Some(id) {
            self.active = self.tabs.keys().next_back().cloned();
        }
        removed
    }

    /// Forgets the tab showing `context`, if it is a top-level one.
    pub(crate) fn remove_context(&mut self, context: &str) {
        self.parents.remove(context);
        if let Some(id) = self.tab_of_top(context) {
            self.remove(&id);
        }
    }

    /// Looks a tab up by MCP id (`None` = the active tab).
    pub(crate) fn get(&self, id: Option<&str>) -> Result<(String, FirefoxTab), String> {
        let id = match id {
            Some(id) => id.to_string(),
            None => self
                .active
                .clone()
                .ok_or("No tab is open in this Firefox instance")?,
        };
        match self.tabs.get(&id) {
            Some(tab) => Ok((id, tab.clone())),
            None => {
                let ids: Vec<_> = self.tabs.keys().cloned().collect();
                Err(format!(
                    "Tab '{id}' not found. Available tabs: {}. Use 'list_tabs' to discover current tab IDs.",
                    if ids.is_empty() {
                        "none".to_string()
                    } else {
                        ids.join(", ")
                    }
                ))
            }
        }
    }
}

/// One Firefox instance, launched (or attached) lazily on first use.
pub(crate) struct FirefoxSession {
    pub(crate) spec: Mutex<FirefoxSpec>,
    browser: Mutex<Option<Arc<Browser>>>,
    pub(crate) tabs: Arc<RwLock<FirefoxTabs>>,
    pump: Mutex<ListenerHandles>,
    /// Events lost because the pump fell behind (reported once, then reset).
    pub(crate) lagged: Arc<AtomicU64>,
    /// Debuggers attached per tab (RDP), see `debugger.rs`.
    pub(crate) debuggers: Mutex<HashMap<String, super::debugger::TabDebugger>>,
    /// Subscription id of the pump (`send_bidi_command` may not remove it).
    pub(crate) own_subscription: RwLock<Option<String>>,
    /// Modules the agent subscribed to through `send_bidi_command`.
    pub(crate) custom_modules: Arc<RwLock<Vec<String>>>,
}

impl FirefoxSession {
    /// A session that has not launched anything yet.
    pub(crate) fn new(spec: FirefoxSpec) -> Self {
        Self {
            spec: Mutex::new(spec),
            browser: Mutex::new(None),
            tabs: Default::default(),
            pump: Mutex::new(ListenerHandles::default()),
            lagged: Default::default(),
            debuggers: Default::default(),
            own_subscription: RwLock::new(None),
            custom_modules: Default::default(),
        }
    }

    /// The running `Browser`, launching or attaching it on first use.
    pub(crate) async fn browser(&self) -> Result<Arc<Browser>, BackendError> {
        let mut slot = self.browser.lock().await;
        if let Some(browser) = slot.as_ref() {
            return Ok(browser.clone());
        }
        let config = self
            .spec
            .lock()
            .await
            .config()
            .map_err(BackendError::InvalidArgument)?;
        let browser = Arc::new(Browser::ensure(config).await.map_err(lifecycle_error)?);
        self.start(&browser).await?;
        *slot = Some(browser.clone());
        Ok(browser)
    }

    /// Subscribes once, starts the pump, registers the tabs already open.
    async fn start(&self, browser: &Browser) -> Result<(), BackendError> {
        let client = browser.client().await.map_err(lifecycle_error)?;
        let events = client.subscribe();
        let id = client
            .subscribe_events(super::pump::EVENTS, None)
            .await
            .map_err(|e| protocol_error("session.subscribe", &e))?;
        *write(&self.own_subscription) = Some(id);
        let state = super::pump::PumpState {
            tabs: self.tabs.clone(),
            lagged: self.lagged.clone(),
            custom_modules: self.custom_modules.clone(),
        };
        let pump = super::pump::spawn(
            events,
            bidi_browser_lite::BrowserClient::from_client(client.clone()),
            state,
        );
        let mut handles = ListenerHandles::default();
        handles.push(pump);
        *self.pump.lock().await = handles;
        // Response bodies are only kept for requests made after a collector
        // exists (spike H11). Best effort: older Firefox versions lack it.
        let collector =
            serde_json::json!({"dataTypes": ["response"], "maxEncodedDataSize": BODY_LIMIT});
        let _ = client
            .send_raw_command("network.addDataCollector", collector)
            .await;
        let urls: HashMap<String, String> = browser
            .list_tabs()
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|info| (info.context, info.url))
            .collect();
        for tab in browser
            .attach_to_all_tabs()
            .await
            .map_err(lifecycle_error)?
        {
            let url = urls.get(tab.context_id()).cloned().unwrap_or_default();
            let _ = write(&self.tabs).register(tab, None, &url);
        }
        Ok(())
    }

    /// Detaches the debuggers, stops the pump, ends the session and stops (or
    /// releases) the browser. Safe to call when nothing runs.
    pub(crate) async fn stop(&self) -> Result<(), BackendError> {
        super::debugger::detach_all(self).await;
        *self.pump.lock().await = ListenerHandles::default();
        let browser = self.browser.lock().await.take();
        *write(&self.tabs) = FirefoxTabs::default();
        *write(&self.own_subscription) = None;
        self.lagged.store(0, Ordering::Relaxed);
        match browser {
            Some(browser) => browser.stop().await.map_err(lifecycle_error),
            None => Ok(()),
        }
    }

    /// `(host, BiDi port, profile dir)` of the running browser.
    pub(crate) async fn address(&self) -> Option<(String, u16, Option<std::path::PathBuf>)> {
        let browser = self.browser.lock().await.clone()?;
        let (host, port) = browser.debug_address().await;
        Some((host, port, browser.profile_dir().await))
    }

    /// The `browserVersion` of the session.
    pub(crate) async fn version(&self) -> Option<String> {
        let browser = self.browser.lock().await.clone()?;
        let client = browser.client().await.ok()?;
        let caps = client.session()?.capabilities;
        caps.get("browserVersion")
            .and_then(|v| v.as_str())
            .map(str::to_string)
    }

    /// The tab `id` (`None` = active), launching the browser first.
    pub(crate) async fn tab(&self, id: Option<&str>) -> Result<(String, FirefoxTab), BackendError> {
        self.browser().await?;
        read(&self.tabs)
            .get(id)
            .map_err(BackendError::InvalidArgument)
    }

    /// Events lost since the last call, as a note for the agent.
    pub(crate) fn lag_note(&self) -> Option<String> {
        let lost = self.lagged.swap(0, Ordering::Relaxed);
        (lost > 0).then(|| format!("{lost} browser events were lost (the server fell behind)"))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use bidi_browser_lite::test_support::{MockBidi, MockBidiBehavior};

    /// A session attached to a mock Firefox (no RDP).
    pub(crate) fn attached(mock: &MockBidi) -> FirefoxSession {
        let mut spec = FirefoxSpec::default();
        spec.firefox.firefox_port = Some(mock.port());
        spec.firefox.firefox_path = Some("/nonexistent/firefox".into());
        FirefoxSession::new(spec)
    }

    /// BID-03: A busy Firefox session is reported with what to do.
    #[tokio::test]
    async fn busy_session_error_is_actionable() {
        let mock = MockBidi::start(MockBidiBehavior::SessionBusy)
            .await
            .unwrap();
        let err = attached(&mock).browser().await.unwrap_err().to_string();
        assert!(err.contains("already has an automation session"), "{err}");
        assert!(err.contains(&mock.port().to_string()), "{err}");
    }

    /// BID-04 / BID-06: one subscription; stop ends the session and the pump.
    #[tokio::test]
    async fn stop_ends_session_and_pump() {
        // Given a running session over the mock, used twice
        let mock = MockBidi::start(MockBidiBehavior::Ready).await.unwrap();
        let session = attached(&mock);
        session.browser().await.unwrap();
        session.browser().await.unwrap();
        // Then it subscribed exactly once and registered the existing tab
        assert_eq!(mock.count("session.subscribe"), 1);
        assert_eq!(read(&session.tabs).tabs.len(), 1);
        let pump = session.pump.lock().await.abort_handles();
        // When it is stopped
        session.stop().await.unwrap();
        // Then the session was ended, the pump finished and the tabs cleared
        assert_eq!(mock.count("session.end"), 1);
        assert!(!mock.session_active());
        tokio::task::yield_now().await;
        assert!(pump.iter().all(|h| h.is_finished()));
        assert!(read(&session.tabs).tabs.is_empty());
    }

    /// BID-05: list_instances shows the Firefox version.
    #[tokio::test]
    async fn info_reports_firefox_version() {
        let mock = MockBidi::start(MockBidiBehavior::Ready).await.unwrap();
        let session = attached(&mock);
        session.browser().await.unwrap();
        assert_eq!(session.version().await.as_deref(), Some("157.0.1"));
        session.stop().await.unwrap();
    }
}
