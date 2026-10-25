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

/// Firefox's own start pages, where page scripts need system access.
fn is_privileged_start_page(url: &str) -> bool {
    matches!(url, "about:home" | "about:newtab" | "about:welcome")
}

/// Largest response body Firefox keeps for `get_network_logs` (bytes).
const BODY_LIMIT: u64 = 2_000_000;

/// How long `stop` waits for the debuggers to detach.
const DETACH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);

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
            if label.is_some() {
                self.check_label(label.as_deref(), Some(&id))?;
                if let Some(entry) = self.tabs.get_mut(&id) {
                    entry.label = label;
                }
            }
            return Ok(id);
        }
        self.check_new(label.as_deref())?;
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

    /// Whether a new tab labelled `label` may be registered (under the cap,
    /// label unused). `open_tab` checks before creating the page.
    pub(crate) fn check_new(&self, label: Option<&str>) -> Result<(), String> {
        if self.tabs.len() >= MAX_TABS {
            return Err(format!("Tab limit reached ({MAX_TABS})"));
        }
        self.check_label(label, None)
    }

    /// Refuses `label` if a tab other than `except` uses it.
    fn check_label(&self, label: Option<&str>, except: Option<&str>) -> Result<(), String> {
        let labels = self
            .tabs
            .iter()
            .map(|(id, t)| (id.as_str(), t.label.as_deref()));
        crate::handler::chrome_instance::tab_registry::check_label(labels, label, except)
    }

    /// Forgets every tab, keeping the id counter: an id from before a
    /// restart never names a different tab.
    pub(crate) fn clear(&mut self) {
        self.tabs.clear();
        self.parents.clear();
        self.active = None;
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
        // Its iframes go too (BiDi may report only the top one destroyed).
        let mut gone = vec![context.to_string()];
        while let Some(parent) = gone.pop() {
            let children: Vec<String> = self
                .parents
                .iter()
                .filter(|(_, p)| **p == parent)
                .map(|(child, _)| child.clone())
                .collect();
            for child in children {
                self.parents.remove(&child);
                gone.push(child);
            }
        }
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
    /// The `enable_proxy_auth` handler, replaced (aborted) on a new call.
    pub(crate) proxy_task: Mutex<ListenerHandles>,
    /// The connection the pump listens on (its liveness is the session's).
    pump_client: RwLock<Option<bidi_browser_lite::BidiClient>>,
    /// Set when Firefox died under the session: calls report it instead of
    /// launching a new one, until `stop` (restart_browser, stop_browser).
    pub(crate) crashed: std::sync::atomic::AtomicBool,
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
            proxy_task: Mutex::new(ListenerHandles::default()),
            pump_client: RwLock::new(None),
            crashed: Default::default(),
        }
    }

    /// The running `Browser`, launching or attaching it on first use. A
    /// session that (re)started watches its tabs once the slot is free.
    pub(crate) async fn browser(&self) -> Result<Arc<Browser>, BackendError> {
        let (browser, started) = self.browser_slot().await?;
        if started && self.spec.lock().await.has_debugger() {
            // Boxed for the same layout-depth reason as the restart below.
            Box::pin(self.watch_initial_tabs(&browser)).await;
        }
        Ok(browser)
    }

    /// The running `Browser` and whether this call started its session.
    async fn browser_slot(&self) -> Result<(Arc<Browser>, bool), BackendError> {
        let mut slot = self.browser.lock().await;
        if self.crashed.load(Ordering::SeqCst) {
            return Err(BackendError::Launch(crate::handler::BROWSER_GONE.into()));
        }
        if let Some(browser) = slot.clone() {
            if self.connected() {
                return Ok((browser, false));
            }
            if !browser.is_alive().await {
                // Firefox itself is gone (not only the WebSocket): its tabs
                // with it. Nothing is relaunched behind the agent's back.
                tracing::warn!("Firefox died; the instance needs restart_browser");
                self.reset().await;
                let _ = browser.stop().await;
                *slot = None;
                self.crashed.store(true, Ordering::SeqCst);
                return Err(BackendError::Launch(crate::handler::BROWSER_GONE.into()));
            }
            // The WebSocket was lost: `Browser` opens a new session on next
            // use, but the pump, subscription and tab handles belong to the
            // dead one.
            tracing::warn!("Firefox connection lost; starting a new session");
            // Boxed: inlined, this future nests every debugger and launch
            // future and overflows rustc's layout query depth.
            Box::pin(self.restart_session(&browser)).await?;
            return Ok((browser, true));
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
        Ok((browser, true))
    }

    /// Whether the pump's connection to Firefox is still open.
    pub(crate) fn connected(&self) -> bool {
        read(&self.pump_client)
            .as_ref()
            .is_some_and(bidi_browser_lite::BidiClient::is_connected)
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
        *write(&self.pump_client) = Some(client.clone());
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
        self.register_initial_tabs(browser).await
    }

    /// Attaches the debugger of every registered tab (WebSocket frames and
    /// page errors from the first page load on). Runs once a session
    /// started, after the browser slot is released: attaching takes the
    /// debuggers lock, which debugger tools take after the slot.
    async fn watch_initial_tabs(&self, browser: &Browser) {
        let tabs: Vec<(String, FirefoxTab)> = read(&self.tabs)
            .tabs
            .iter()
            .map(|(id, tab)| (id.clone(), tab.clone()))
            .collect();
        for (id, tab) in tabs {
            super::debugger::watch_tab(self, browser, &id, &tab).await;
        }
    }

    /// Registers the tabs already open (moving a launched Firefox off its
    /// privileged start page).
    async fn register_initial_tabs(&self, browser: &Browser) -> Result<(), BackendError> {
        let urls: HashMap<String, String> = browser
            .list_tabs()
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|info| (info.context, info.url))
            .collect();
        let launched = self.spec.lock().await.firefox.firefox_port.is_none();
        for tab in browser
            .attach_to_all_tabs()
            .await
            .map_err(lifecycle_error)?
        {
            let mut url = urls.get(tab.context_id()).cloned().unwrap_or_default();
            // A fresh Firefox opens about:home, a privileged page where BiDi
            // refuses scripts without -remote-allow-system-access: start
            // from about:blank instead (only in instances we launched).
            if launched && is_privileged_start_page(&url) {
                let blank = tab
                    .navigate("about:blank", bidi_browser_lite::ReadinessState::Complete)
                    .await;
                if blank.is_ok() {
                    url = "about:blank".to_string();
                }
            }
            let _ = write(&self.tabs).register(tab, None, &url);
        }
        Ok(())
    }

    /// Detaches the debuggers, stops the pump, ends the session and stops (or
    /// releases) the browser. Safe to call when nothing runs.
    pub(crate) async fn stop(&self) -> Result<(), BackendError> {
        self.crashed.store(false, Ordering::SeqCst);
        self.reset().await;
        let browser = self.browser.lock().await.take();
        match browser {
            Some(browser) => browser.stop().await.map_err(lifecycle_error),
            None => Ok(()),
        }
    }

    /// Replaces a dead session's pump, subscription and tabs with new ones.
    async fn restart_session(&self, browser: &Browser) -> Result<(), BackendError> {
        self.reset().await;
        self.start(browser).await
    }

    /// Forgets everything tied to the current session: debuggers, pump,
    /// proxy handler, subscriptions and tabs (never the browser itself).
    async fn reset(&self) {
        // Bounded: a debugger tool in flight holds the debuggers lock, and
        // the browser must still be stopped within the shutdown budget.
        let detach = super::debugger::detach_all(self);
        let _ = tokio::time::timeout(DETACH_TIMEOUT, detach).await;
        *self.pump.lock().await = ListenerHandles::default();
        *self.proxy_task.lock().await = ListenerHandles::default();
        write(&self.custom_modules).clear();
        write(&self.tabs).clear();
        *write(&self.own_subscription) = None;
        *write(&self.pump_client) = None;
        self.lagged.store(0, Ordering::Relaxed);
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

    /// REV-09: after the WebSocket is lost the next use starts a new
    /// session (subscription, pump, tabs) instead of reusing a dead pump.
    #[tokio::test]
    async fn lost_connection_starts_a_fresh_session() {
        // Given a running session whose WebSocket then drops
        let mock = MockBidi::start(MockBidiBehavior::Ready).await.unwrap();
        let session = attached(&mock);
        session.browser().await.unwrap();
        mock.drop_connections();
        // (and the old session is gone; an orphan one is reported instead)
        mock.release_session();
        for _ in 0..100 {
            if !session.connected() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(!session.connected());
        // When a tool uses the session again
        session.browser().await.unwrap();
        // Then the server subscribed again on a live connection
        assert_eq!(mock.count("session.subscribe"), 2);
        assert!(session.connected());
        session.stop().await.unwrap();
    }

    /// AUD-LC-07: once Firefox itself is gone (its port no longer answers),
    /// calls say to restart, without relaunching anything, until the
    /// instance is stopped (it answered "the browser was stopped" forever).
    #[tokio::test]
    async fn dead_firefox_says_to_restart() {
        // Given a running session whose Firefox then dies
        let mock = MockBidi::start(MockBidiBehavior::Ready).await.unwrap();
        let session = attached(&mock);
        session.browser().await.unwrap();
        drop(mock);
        for _ in 0..100 {
            if !session.connected() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        // When tools use the session, twice
        for _ in 0..2 {
            let error = session.browser().await.unwrap_err().to_string();
            // Then each says the browser is gone, and its tabs are forgotten
            assert_eq!(error, crate::handler::BROWSER_GONE);
        }
        assert!(read(&session.tabs).tabs.is_empty());
        // And once stopped (what restart_browser does first) it is not reported
        session.stop().await.unwrap();
        assert!(!session.crashed.load(Ordering::SeqCst));
    }

    /// Two tabs of a mock Firefox: `ctx-1` labelled "x", `ctx-2` unlabelled.
    async fn two_tabs(mock: &MockBidi) -> (bidi_browser_lite::BrowserClient, FirefoxTabs) {
        let client = bidi_browser_lite::BrowserClient::connect(
            &format!("127.0.0.1:{}", mock.port()),
            std::time::Duration::from_secs(5),
        )
        .await
        .unwrap();
        let mut tabs = FirefoxTabs::default();
        let first = client.attach("ctx-1").await.unwrap();
        tabs.register(first, Some("x".into()), "about:blank")
            .unwrap();
        let second = client.attach("ctx-2").await.unwrap();
        tabs.register(second, None, "about:blank").unwrap();
        (client, tabs)
    }

    /// REV-06: labels stay unique when a context registers twice, and
    /// `check_new` refuses a taken label before a tab is created.
    #[tokio::test]
    async fn labels_stay_unique_when_a_context_registers_twice() {
        // Given tab-1 labelled "x" and tab-2 registered by the pump
        let mock = MockBidi::start(MockBidiBehavior::Ready).await.unwrap();
        let (client, mut tabs) = two_tabs(&mock).await;
        // When open_tab registers ctx-2 again with the label "x"
        let again = client.attach("ctx-2").await.unwrap();
        let taken = tabs.register(again, Some("x".into()), "about:blank");
        // Then it is refused, and so is a new tab with that label
        assert!(taken.is_err());
        assert!(tabs.check_new(Some("x")).is_err());
        assert!(tabs.check_new(Some("y")).is_ok());
    }

    /// REV-13: tab ids are not reused after a restart.
    #[tokio::test]
    async fn tab_ids_are_not_reused_after_clear() {
        // Given tab-1 and tab-2
        let mock = MockBidi::start(MockBidiBehavior::Ready).await.unwrap();
        let (client, mut tabs) = two_tabs(&mock).await;
        // When the tabs are cleared (restart) and a tab registers
        tabs.clear();
        let tab = client.attach("ctx-3").await.unwrap();
        let id = tabs.register(tab, None, "about:blank").unwrap();
        // Then it gets a new id
        assert_eq!(id, "tab-3");
        assert!(tabs.tabs.len() == 1 && tabs.parents.is_empty());
    }

    /// REV-12: destroying a context forgets its whole iframe subtree.
    #[test]
    fn destroying_a_context_forgets_its_subtree() {
        // Given ctx-1 > frame-a > frame-b, and ctx-9 > frame-z
        let mut tabs = FirefoxTabs::default();
        for (child, parent) in [
            ("frame-a", "ctx-1"),
            ("frame-b", "frame-a"),
            ("frame-z", "ctx-9"),
        ] {
            tabs.parents.insert(child.into(), parent.into());
        }
        // When ctx-1 is destroyed
        tabs.remove_context("ctx-1");
        // Then only ctx-9's frame remains
        assert_eq!(tabs.parents.len(), 1);
        assert!(tabs.parents.contains_key("frame-z"));
    }
}
