pub mod cdp_domains;
pub mod chrome_instance;
pub(crate) mod debugging;
pub(crate) mod evaluation;
pub(crate) mod navigation;
mod registry;
mod schema_compat;
pub(crate) mod shutdown;

use async_trait::async_trait;
use cdp_browser_lite::CdpClient;
use rust_mcp_sdk::{McpServer, mcp_server::ServerHandler, schema::*};
use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Clone, Debug, ::serde::Serialize, ::serde::Deserialize)]
pub(crate) struct ScriptInfo {
    pub hash: String,
    pub start_line: i32,
    pub start_column: i32,
    /// The script's URL (empty for inline evals).
    #[serde(default)]
    pub url: String,
}

/// Most parsed scripts remembered per tab (oldest forgotten first).
pub(crate) const MAX_SCRIPTS: usize = 2000;
/// Most WebSocket connections remembered per tab, with their frames.
pub(crate) const MAX_WS_CONNECTIONS: usize = 100;

#[derive(Default)]
pub(crate) struct DebuggerState {
    pub scripts: crate::util::bounded::BoundedMap<String, ScriptInfo, MAX_SCRIPTS>,
    pub paused_call_frame_id: Option<String>,
    /// Identifier of the `debugger;` script `pause_on_load` installed, removed
    /// on the next `resume` so later navigations do not pause again.
    pub pause_on_load_script: Option<String>,
    /// Breakpoints by MCP id (`bp-N`) → CDP breakpoint id.
    pub breakpoints: debugging::BreakpointIds<String>,
}

#[derive(Clone, Debug, ::serde::Serialize, ::serde::Deserialize)]
pub struct NetworkRequest {
    pub url: String,
    pub method: String,
    pub resource_type: Option<String>,
    pub request_headers: Option<serde_json::Value>,
    pub request_post_data: Option<String>,
    pub response_status: Option<i64>,
    pub response_status_text: Option<String>,
    pub response_headers: Option<serde_json::Value>,
    pub response_body: Option<String>,
    /// URLs this request was redirected from, oldest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub redirects: Vec<String>,
}

#[derive(Clone, Debug, ::serde::Serialize, ::serde::Deserialize)]
pub struct WebSocketFrame {
    pub url: String,
    pub payload_data: String,
    pub is_sent: bool,
}

pub(crate) const MAX_REQUESTS_PER_NAVIGATION: usize = 1000;
pub(crate) const MAX_NAVIGATIONS_RETAINED: usize = 3;
pub(crate) const MAX_WS_FRAMES_PER_CONNECTION: usize = 500;
pub(crate) const MAX_CONSOLE_MESSAGES: usize = 1000;

#[derive(Default, Clone)]
pub(crate) struct NavigationBucket {
    pub requests: std::collections::HashMap<String, NetworkRequest>,
    pub order: std::collections::VecDeque<String>,
}

pub(crate) struct NetworkState {
    pub navigations: std::collections::VecDeque<NavigationBucket>,
    pub ws_connections: crate::util::bounded::BoundedMap<String, String, MAX_WS_CONNECTIONS>,
    pub ws_frames: crate::util::bounded::BoundedMap<
        String,
        std::collections::VecDeque<WebSocketFrame>,
        MAX_WS_CONNECTIONS,
    >,
}

impl Default for NetworkState {
    fn default() -> Self {
        let mut navigations = std::collections::VecDeque::new();
        navigations.push_back(NavigationBucket::default());
        Self {
            navigations,
            ws_connections: Default::default(),
            ws_frames: Default::default(),
        }
    }
}

impl NetworkState {
    pub(crate) fn insert_request(&mut self, id: String, req: NetworkRequest) {
        // A redirect reuses the request id: keep one entry, remember the hop.
        if let Some(existing) = self.find_request_mut(&id) {
            let previous = std::mem::replace(existing, req);
            existing.redirects = previous.redirects;
            existing.redirects.push(previous.url);
            return;
        }
        if self.navigations.is_empty() {
            self.navigations.push_back(NavigationBucket::default());
        }
        if let Some(bucket) = self.navigations.front_mut() {
            bucket.order.push_back(id.clone());
            bucket.requests.insert(id, req);
            if bucket.order.len() > MAX_REQUESTS_PER_NAVIGATION
                && let Some(oldest_id) = bucket.order.pop_front()
            {
                bucket.requests.remove(&oldest_id);
            }
        }
    }

    pub(crate) fn find_request_mut(&mut self, id: &str) -> Option<&mut NetworkRequest> {
        for bucket in &mut self.navigations {
            if let Some(req) = bucket.requests.get_mut(id) {
                return Some(req);
            }
        }
        None
    }

    pub(crate) fn split_after_navigation(&mut self) {
        self.navigations.push_front(NavigationBucket::default());
        self.navigations.truncate(MAX_NAVIGATIONS_RETAINED);
    }

    /// Remembers a WebSocket connection; the oldest one beyond the cap is
    /// forgotten together with its frames.
    pub(crate) fn insert_ws_connection(&mut self, id: String, url: String) {
        if let Some(evicted) = self.ws_connections.insert(id, url) {
            self.ws_frames.remove(&evicted);
        }
    }

    pub(crate) fn push_ws_frame(&mut self, id: String, frame: WebSocketFrame) {
        if self.ws_frames.get_mut(&id).is_none() {
            self.ws_frames
                .insert(id.clone(), std::collections::VecDeque::new());
        }
        if let Some(frames) = self.ws_frames.get_mut(&id) {
            frames.push_back(frame);
            if frames.len() > MAX_WS_FRAMES_PER_CONNECTION {
                frames.pop_front();
            }
        }
    }

    pub(crate) fn iter_all(&self) -> std::collections::HashMap<String, NetworkRequest> {
        let mut all = std::collections::HashMap::new();
        for bucket in self.navigations.iter().rev() {
            for (id, req) in &bucket.requests {
                all.insert(id.clone(), req.clone());
            }
        }
        all
    }

    pub(crate) fn iter_current(&self) -> std::collections::HashMap<String, NetworkRequest> {
        self.navigations
            .front()
            .map(|b| b.requests.clone())
            .unwrap_or_default()
    }

    pub(crate) fn current_ids(&self) -> std::collections::HashSet<String> {
        self.navigations
            .front()
            .map(|b| b.requests.keys().cloned().collect())
            .unwrap_or_default()
    }

    pub(crate) fn clear_all(&mut self) {
        self.navigations.clear();
        self.navigations.push_back(NavigationBucket::default());
        self.ws_connections.clear();
        self.ws_frames.clear();
    }
}

#[derive(Clone, Debug, ::serde::Serialize, ::serde::Deserialize)]
pub struct CustomEvent {
    pub method: String,
    pub params: serde_json::Value,
    pub timestamp: String,
}

#[derive(Default)]
pub(crate) struct CustomState {
    pub events: std::collections::VecDeque<CustomEvent>,
    pub active_domains: std::collections::HashSet<String>,
    pub handles: cdp_domains::event_pump::ListenerHandles,
}

pub(crate) const MIN_SUPPORTED_CHROME_MAJOR: u32 = 120;

#[derive(Clone, Debug, Default, ::serde::Serialize, ::serde::Deserialize)]
pub struct BrowserVersion {
    pub product: Option<String>,
    pub revision: Option<String>,
    pub protocol_version: Option<String>,
    pub js_version: Option<String>,
}

/// A tool's text for a protocol reply: `prefix` then the reply's `result` as
/// pretty JSON (chrome-debug-mcp printed the Rust `Debug` of the whole
/// message, unreadable for agents and tied to CDP; specs D24).
pub(crate) fn reply_text(prefix: &str, reply: &cdp_browser_lite::WsResponse) -> String {
    let empty = serde_json::json!({});
    let body =
        serde_json::to_string_pretty(reply.result.as_ref().unwrap_or(&empty)).unwrap_or_default();
    if prefix.is_empty() {
        body
    } else {
        format!("{prefix}\n{body}")
    }
}

/// Parses a tool's arguments into its argument struct.
pub(crate) fn parse_args<T: serde::de::DeserializeOwned>(
    params: CallToolRequestParams,
) -> std::result::Result<T, CallToolError> {
    let value = serde_json::Value::Object(params.arguments.unwrap_or_default());
    serde_json::from_value(value).map_err(|e| CallToolError::from_message(e.to_string()))
}

/// The CDP target of `instance_id` / `tab_id` (defaults: the default
/// instance, its active tab).
pub(crate) async fn resolve_target(
    handler: &McpHandler,
    instance_id: Option<String>,
    tab_id: Option<String>,
) -> std::result::Result<cdp_domains::cdp_target::CdpTarget, CallToolError> {
    // Two statements: the error type is not `Send` across an await.
    let session = handler.session(instance_id).await?;
    session.target(tab_id).await
}

/// The `instance_id` / `tab_id` pair every tool accepts (other fields of the
/// arguments are ignored when parsing into this).
#[derive(Debug, Default, serde::Deserialize)]
pub(crate) struct TargetArgs {
    pub(crate) instance_id: Option<String>,
    pub(crate) tab_id: Option<String>,
}

/// Sends one command; a failure becomes `"<failure>: <error>"`.
pub(crate) async fn send_or(
    target: &cdp_domains::cdp_target::CdpTarget,
    method: &str,
    params: serde_json::Value,
    failure: &str,
) -> std::result::Result<cdp_browser_lite::WsResponse, CallToolError> {
    target
        .send_raw_command(method, params)
        .await
        .map_err(|e| CallToolError::from_message(format!("{failure}: {e:?}")))
}

/// The value of a `util::js` snippet (`Runtime.evaluate` by value). A
/// thrown exception, such as the `SyntaxError` of an invalid selector, is an
/// error with its message rather than a missing value.
pub(crate) async fn snippet_value(
    target: &cdp_domains::cdp_target::CdpTarget,
    expression: &str,
) -> std::result::Result<serde_json::Value, CallToolError> {
    let params = serde_json::json!({"expression": expression, "returnByValue": true});
    let reply = send_or(target, "Runtime.evaluate", params, "CDP Error").await?;
    let result = reply.result.unwrap_or_default();
    if let Some(details) = result.get("exceptionDetails") {
        let thrown = details
            .pointer("/exception/description")
            .or_else(|| details.get("text"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("exception");
        let first_line = thrown.lines().next().unwrap_or(thrown);
        return Err(CallToolError::from_message(first_line.to_string()));
    }
    Ok(result.pointer("/result/value").cloned().unwrap_or_default())
}

/// `Browser.getVersion`, or `None` when it fails.
async fn read_browser_version(client: &CdpClient) -> Option<BrowserVersion> {
    let reply = client
        .send_raw_command("Browser.getVersion", cdp_browser_lite::NoParams)
        .await
        .ok()?;
    let result = reply.result?;
    let field = |name: &str| result.get(name).and_then(|v| v.as_str()).map(String::from);
    Some(BrowserVersion {
        product: field("product"),
        revision: field("revision"),
        protocol_version: field("protocolVersion"),
        js_version: field("jsVersion"),
    })
}

pub(crate) fn parse_chrome_major(product: &str) -> Option<u32> {
    let prefix = if product.starts_with("HeadlessChrome/") {
        "HeadlessChrome/"
    } else if product.starts_with("Chrome/") {
        "Chrome/"
    } else {
        return None;
    };

    let rest = &product[prefix.len()..];
    let major_str = rest.split('.').next()?;
    major_str.parse::<u32>().ok()
}

/// `fn <field>(&self, tab_id)`: the tab's copy of a per-domain state, or the
/// session-level one when no tab is registered (same field name in both).
macro_rules! state_accessor {
    ($field:ident, $ty:ty) => {
        pub(crate) fn $field(
            &self,
            tab_id: Option<String>,
        ) -> std::result::Result<Arc<Mutex<$ty>>, CallToolError> {
            self.tab_or_fallback_state(tab_id, |e| e.$field.clone(), |s| s.$field.clone())
        }
    };
}

pub(crate) struct BrowserSession {
    pub(crate) client: Arc<Mutex<Option<CdpClient>>>,
    pub(crate) debugger_state: Arc<Mutex<DebuggerState>>,
    pub(crate) network_state: Arc<Mutex<NetworkState>>,
    pub(crate) log_state: Arc<Mutex<cdp_domains::log::LogState>>,
    pub(crate) tracing_state: Arc<Mutex<cdp_domains::tracing::TracingState>>,
    pub(crate) custom_state: Arc<Mutex<CustomState>>,
    pub(crate) webmcp_state: Arc<Mutex<cdp_domains::webmcp::WebmcpState>>,
    pub(crate) chrome_manager: Arc<Mutex<dyn chrome_instance::ChromeManager>>,
    pub(crate) tabs: Arc<std::sync::RwLock<chrome_instance::tab_registry::TabRegistry>>,
    pub(crate) session_listeners: Arc<Mutex<cdp_domains::event_pump::ListenerHandles>>,
    pub(crate) browser_version: Arc<Mutex<Option<BrowserVersion>>>,
    pub(crate) enable_webmcp: bool,
    /// Proxy-auth handler task per target (`None` = the session-level page),
    /// so enabling it again replaces the previous handler instead of stacking.
    pub(crate) proxy_auth_tasks: cdp_domains::fetch::ProxyAuthTasks,
}

impl BrowserSession {
    /// The MCP id of the tab a call addresses: `tab_id`, else the active
    /// tab (`None` when no tab is registered yet).
    pub(crate) fn tab_id_of(&self, tab_id: Option<&str>) -> Option<String> {
        match tab_id {
            Some(id) => Some(id.to_string()),
            None => crate::util::sync::read(&self.tabs).active_tab_id.clone(),
        }
    }

    /// A session with empty per-domain states, launched lazily by `chrome_manager`.
    pub(crate) fn new(
        chrome_manager: Arc<Mutex<dyn chrome_instance::ChromeManager>>,
        webmcp: cdp_domains::webmcp::WebmcpAvailability,
        enable_webmcp: bool,
    ) -> Self {
        let webmcp_state = cdp_domains::webmcp::WebmcpState {
            availability: webmcp,
            ..Default::default()
        };
        Self {
            client: Arc::new(Mutex::new(None)),
            debugger_state: Default::default(),
            network_state: Default::default(),
            log_state: Default::default(),
            tracing_state: Default::default(),
            custom_state: Default::default(),
            webmcp_state: Arc::new(Mutex::new(webmcp_state)),
            chrome_manager,
            tabs: Arc::new(std::sync::RwLock::new(
                chrome_instance::tab_registry::TabRegistry::new(16),
            )),
            session_listeners: Default::default(),
            browser_version: Default::default(),
            proxy_auth_tasks: Default::default(),
            enable_webmcp,
        }
    }

    /// Resets the cached CDP client and clears the tab registry so that the
    /// next tool call reconnects to a freshly launched Chrome instance.
    pub(crate) async fn reset_connection_state(&self) {
        *self.client.lock().await = None;
        *self.session_listeners.lock().await = cdp_domains::event_pump::ListenerHandles::default();
        *self.browser_version.lock().await = None;
        self.proxy_auth_tasks.lock().await.clear();
        cdp_domains::custom::clear_custom_listeners(&self.custom_state).await;
        crate::util::sync::write(&self.tabs).clear();
    }

    /// Returns the browser-level CDP client used for tab management.
    pub(crate) async fn browser_client(
        &self,
    ) -> std::result::Result<cdp_browser_lite::BrowserClient, CallToolError> {
        let manager = self.chrome_manager.lock().await;
        manager.browser_client().await.map_err(|e| {
            CallToolError::from_message(format!("Failed to obtain browser client: {}", e))
        })
    }

    pub(crate) async fn get_or_connect(
        &self,
    ) -> std::result::Result<tokio::sync::MutexGuard<'_, Option<CdpClient>>, CallToolError> {
        // First ensure instance is running
        {
            let mut manager = self.chrome_manager.lock().await;
            manager.ensure_instance().await.map_err(|e| {
                CallToolError::from_message(format!(
                    "Failed to ensure Chrome instance: {}",
                    chrome_instance::launch::describe_launch_failure(&e.to_string())
                ))
            })?;
        }

        let mut client_lock = self.client.lock().await;
        if client_lock.is_none() {
            let client_res = {
                let manager = self.chrome_manager.lock().await;
                manager.client().await
            };
            let client = client_res.map_err(|e| {
                CallToolError::from_message(format!("Failed to connect to Chrome: {}", e))
            })?;
            self.prepare_client(&client).await;
            *client_lock = Some(client);
        }
        Ok(client_lock)
    }

    /// First use of a session-level client: enable the domains, read the
    /// version, start the listeners. The order matters (see each step).
    async fn prepare_client(&self, client: &CdpClient) {
        for domain in ["Runtime", "Page", "Network", "Log", "Performance"] {
            let _ = client
                .send_raw_command(&format!("{domain}.enable"), cdp_browser_lite::NoParams)
                .await;
        }
        *self.browser_version.lock().await = read_browser_version(client).await;

        let target = cdp_domains::cdp_target::CdpTarget::Client(client.clone());
        let mut handles = self.start_session_listeners(&target);
        // WebMCP.enable replays the page's tools: its listener runs first.
        self.enable_session_webmcp(client).await;
        // Debugger.enable replays `scriptParsed`: the listener runs first.
        let _ = client
            .send_raw_command("Debugger.enable", cdp_browser_lite::NoParams)
            .await;
        // Start the tab lifecycle listener (Target.*)
        if let Ok(browser_client) = {
            let manager = self.chrome_manager.lock().await;
            manager.browser_client().await
        } {
            handles.push(
                chrome_instance::tab_lifecycle::start_tab_lifecycle_listener(
                    browser_client,
                    self.tabs.clone(),
                    self.enable_webmcp,
                ),
            );
        }
        *self.session_listeners.lock().await = handles;
    }

    /// `WebMCP.enable` when the flag is on; records the resulting availability.
    async fn enable_session_webmcp(&self, client: &CdpClient) {
        use cdp_domains::webmcp::WebmcpAvailability;
        let availability = if self.enable_webmcp {
            match client
                .send_raw_command("WebMCP.enable", cdp_browser_lite::NoParams)
                .await
            {
                Ok(_) => WebmcpAvailability::Enabled,
                Err(_) => WebmcpAvailability::Unsupported,
            }
        } else {
            WebmcpAvailability::NotRequested
        };
        self.webmcp_state.lock().await.availability = availability;
    }

    /// The per-domain listeners of the session-level page client.
    fn start_session_listeners(
        &self,
        target: &cdp_domains::cdp_target::CdpTarget,
    ) -> cdp_domains::event_pump::ListenerHandles {
        let mut handles = cdp_domains::event_pump::ListenerHandles::default();
        handles.push(cdp_domains::debugger::start_debugger_listener(
            target,
            self.debugger_state.clone(),
        ));
        handles.push(cdp_domains::network::start_network_listener(
            target,
            self.network_state.clone(),
        ));
        handles.push(cdp_domains::page::start_page_listener(
            target,
            self.network_state.clone(),
            self.enable_webmcp
                .then(|| cdp_domains::webmcp::WebmcpNavSync {
                    state: self.webmcp_state.clone(),
                    target: target.clone(),
                }),
        ));
        handles.absorb(cdp_domains::log::start_log_listener(
            target,
            self.log_state.clone(),
        ));
        handles.push(cdp_domains::tracing::start_tracing_listener(
            target,
            self.tracing_state.clone(),
        ));
        if self.enable_webmcp {
            handles.push(cdp_domains::webmcp::start_webmcp_listener(
                target,
                self.webmcp_state.clone(),
            ));
        }
        handles
    }

    pub(crate) async fn target(
        &self,
        tab_id: Option<String>,
    ) -> std::result::Result<cdp_domains::cdp_target::CdpTarget, CallToolError> {
        enum Lookup {
            Found(cdp_domains::cdp_target::CdpTarget),
            NotFound(String),
            FallbackToDefault,
        }

        let lookup = {
            let registry = crate::util::sync::read(&self.tabs);
            if let Some(id) = tab_id {
                if let Some(entry) = registry.tabs.get(&id) {
                    Lookup::Found(cdp_domains::cdp_target::CdpTarget::Tab(entry.tab.clone()))
                } else {
                    Lookup::NotFound(id)
                }
            } else if let Some(ref active_id) = registry.active_tab_id
                && let Some(entry) = registry.tabs.get(active_id)
            {
                Lookup::Found(cdp_domains::cdp_target::CdpTarget::Tab(entry.tab.clone()))
            } else {
                Lookup::FallbackToDefault
            }
        };

        match lookup {
            Lookup::Found(target) => return Ok(target),
            Lookup::NotFound(id) => {
                let registry = crate::util::sync::read(&self.tabs);
                let available: Vec<_> = registry.tabs.keys().cloned().collect();
                let avail_str = if available.is_empty() {
                    "none (if Chrome was restarted, previous tab IDs are no longer valid)"
                        .to_string()
                } else {
                    available.join(", ")
                };
                return Err(CallToolError::from_message(format!(
                    "Tab with ID '{}' not found in this session. Available tabs: {}. Use 'list_tabs' to discover current tab IDs.",
                    id, avail_str
                )));
            }
            Lookup::FallbackToDefault => {}
        }

        let client_guard = self.get_or_connect().await?;
        if let Some(ref client) = *client_guard {
            Ok(cdp_domains::cdp_target::CdpTarget::Client(client.clone()))
        } else {
            Err(CallToolError::from_message(
                "Failed to retrieve target: no active tabs or default connection available"
                    .to_string(),
            ))
        }
    }

    fn tab_or_fallback_state<T, F, G>(
        &self,
        tab_id: Option<String>,
        tab_extractor: F,
        fallback_extractor: G,
    ) -> std::result::Result<Arc<Mutex<T>>, CallToolError>
    where
        F: FnOnce(&chrome_instance::tab_registry::TabEntry) -> Arc<Mutex<T>>,
        G: FnOnce(&Self) -> Arc<Mutex<T>>,
    {
        let registry = crate::util::sync::read(&self.tabs);
        if let Some(id) = tab_id {
            return if let Some(entry) = registry.tabs.get(&id) {
                Ok(tab_extractor(entry))
            } else {
                let available: Vec<_> = registry.tabs.keys().cloned().collect();
                let avail_str = if available.is_empty() {
                    "none (if Chrome was restarted, previous tab IDs are no longer valid)"
                        .to_string()
                } else {
                    available.join(", ")
                };
                Err(CallToolError::from_message(format!(
                    "Tab '{}' not found. Available tabs: {}. Use 'list_tabs' to discover current tab IDs.",
                    id, avail_str
                )))
            };
        }
        if let Some(ref active_id) = registry.active_tab_id
            && let Some(entry) = registry.tabs.get(active_id)
        {
            return Ok(tab_extractor(entry));
        }
        Ok(fallback_extractor(self))
    }

    state_accessor!(debugger_state, DebuggerState);
    state_accessor!(network_state, NetworkState);
    state_accessor!(log_state, cdp_domains::log::LogState);
    state_accessor!(tracing_state, cdp_domains::tracing::TracingState);
    state_accessor!(custom_state, CustomState);
    state_accessor!(webmcp_state, cdp_domains::webmcp::WebmcpState);
}

#[derive(Debug, Clone)]
pub struct HandlerParams {
    pub host: String,
    pub port: u16,
    pub default_browser: crate::backend::BrowserKind,
    pub browser: crate::cli::BrowserFlags,
    pub server: crate::cli::ServerFlags,
    pub proxy: crate::cli::ProxyArgs,
    pub firefox: crate::cli::FirefoxArgs,
}

impl Default for HandlerParams {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".into(),
            port: 9222,
            default_browser: Default::default(),
            browser: Default::default(),
            server: Default::default(),
            proxy: Default::default(),
            firefox: Default::default(),
        }
    }
}

pub struct McpHandler {
    pub(crate) default_session: Arc<BrowserSession>,
    pub(crate) registry: Arc<chrome_instance::registry::Registry>,
    pub(crate) pool: Arc<cdp_browser_lite::BrowserPool>,
    pub(crate) base_params: chrome_instance::launch::LaunchParams,
    pub(crate) flags: crate::cli::ServerFlags,
    pub(crate) proxy: crate::cli::ProxyArgs,
    pub(crate) is_test: bool,
    /// Browser of the default instance (`--browser`).
    pub(crate) default_browser: crate::backend::BrowserKind,
    /// How Firefox instances are launched (from the command line).
    pub(crate) firefox_spec: crate::backend::bidi::launch::FirefoxSpec,
}

impl McpHandler {
    /// The Firefox instance `id` (`None` = the default instance, created on
    /// first use when `--browser firefox`).
    pub(crate) async fn firefox(
        &self,
        id: Option<&str>,
    ) -> std::result::Result<Arc<crate::backend::bidi::session::FirefoxSession>, CallToolError>
    {
        use crate::backend::bidi::session::FirefoxSession;
        let id = id.unwrap_or("default");
        if let Some(session) = self.registry.get_firefox(id) {
            return Ok(session);
        }
        if id != "default" || self.default_browser != crate::backend::BrowserKind::Firefox {
            let ids: Vec<_> = self
                .registry
                .list_descriptors()
                .into_iter()
                .map(|d| d.id)
                .collect();
            return Err(CallToolError::from_message(format!(
                "Instance id '{id}' not found. Available instances: {}. Use 'list_instances' to discover instance IDs.",
                ids.join(", ")
            )));
        }
        let session = Arc::new(FirefoxSession::new(self.firefox_spec.clone()));
        let mut desc = chrome_instance::registry::InstanceDescriptor::default_instance(
            self.firefox_spec.firefox.firefox_port.unwrap_or(0),
            Vec::new(),
        );
        desc.browser = crate::backend::BrowserKind::Firefox;
        // Two first calls may race: the loser takes the winner's session.
        match self.registry.add_firefox(desc, session.clone()) {
            Ok(()) => Ok(session),
            Err(e) => self
                .registry
                .get_firefox(id)
                .ok_or_else(|| CallToolError::from_message(e)),
        }
    }

    /// What stops this handler's browsers once the handler is owned by the
    /// MCP runtime.
    pub(crate) fn shutdown_hook(&self) -> shutdown::ShutdownHook {
        shutdown::ShutdownHook {
            registry: self.registry.clone(),
        }
    }

    /// The browser instance `id` runs (`None` = the default instance, which
    /// may not be registered yet). An unknown id reports Chrome: the Chrome
    /// handlers then answer with the list of valid ids.
    pub(crate) fn browser_of(&self, id: Option<&str>) -> crate::backend::BrowserKind {
        let id = id.unwrap_or("default");
        match self.registry.browser_of(id) {
            Some(browser) => browser,
            None if id == "default" => self.default_browser,
            None => crate::backend::BrowserKind::Chrome,
        }
    }

    /// What an instance of `browser` can do under this server's flags.
    pub(crate) fn capabilities(
        &self,
        browser: crate::backend::BrowserKind,
    ) -> crate::backend::Capabilities {
        let flags = crate::backend::capabilities::CapabilityFlags {
            server: self.flags,
            proxy: self.proxy.proxy_server.is_some(),
            firefox_debugger: self.firefox_spec.has_debugger(),
        };
        crate::backend::Capabilities::new(browser, flags)
    }

    pub async fn session(
        &self,
        id: Option<String>,
    ) -> std::result::Result<Arc<BrowserSession>, CallToolError> {
        let id = id.unwrap_or_else(|| "default".to_string());
        if let Some(session) = self.registry.get_session(&id) {
            Ok(session)
        } else if id == "default" {
            // Lazily add the default session
            let default_features = {
                let mgr = self.default_session.chrome_manager.lock().await;
                mgr.features()
                    .iter()
                    .map(|f| f.as_name().to_string())
                    .collect::<Vec<_>>()
            };
            let desc = chrome_instance::registry::InstanceDescriptor::default_instance(
                self.base_params.configured_port(),
                default_features,
            );
            self.registry
                .add_session(desc, self.default_session.clone())
                .map_err(|e| {
                    CallToolError::from_message(format!(
                        "Failed to register default session: {}",
                        e
                    ))
                })?;
            Ok(self.default_session.clone())
        } else {
            let available: Vec<_> = self
                .registry
                .list_descriptors()
                .into_iter()
                .map(|d| d.id)
                .collect();
            Err(CallToolError::from_message(format!(
                "Instance id '{}' not found. Available instances: {}. Use 'list_instances' to discover instance IDs.",
                id,
                available.join(", ")
            )))
        }
    }

    pub fn new_with_params(options: HandlerParams) -> Self {
        let port = options.port;
        let mut params =
            chrome_instance::launch::LaunchParams::new(options.host, options.port, options.browser);
        params.set_enable_webmcp(options.server.enable_webmcp);
        if let Some(proxy) = &options.proxy.proxy_server {
            params.set_proxy(Some(proxy.clone()));
        }
        let pool = Arc::new(cdp_browser_lite::BrowserPool::new());
        let registry = Arc::new(chrome_instance::registry::Registry::new(8)); // max 8 instances

        let manager = chrome_instance::cdp_browser_manager::CdpBrowserManager::new(
            params.clone(),
            Box::new(chrome_instance::cdp_browser_manager::RealLauncher { pool: pool.clone() }),
        );
        let enable_webmcp = options.server.enable_webmcp;
        let session = Arc::new(BrowserSession::new(
            Arc::new(Mutex::new(manager)),
            cdp_domains::webmcp::WebmcpAvailability::initial(enable_webmcp),
            enable_webmcp,
        ));

        let features = params.features().iter().map(|f| f.as_name().to_string());
        let mut default = chrome_instance::registry::InstanceDescriptor::default_instance(
            port,
            features.collect(),
        );
        if options.default_browser == crate::backend::BrowserKind::Firefox {
            default.browser = crate::backend::BrowserKind::Firefox;
            default.port = options.firefox.firefox_port.unwrap_or(0);
            default.features = Vec::new();
        }
        registry.register_descriptor(default);
        let firefox_spec = crate::backend::bidi::launch::FirefoxSpec {
            browser: options.browser,
            proxy: options.proxy.proxy_server.clone(),
            firefox: options.firefox,
        };

        Self {
            default_session: session,
            registry,
            pool,
            base_params: params,
            flags: options.server,
            proxy: options.proxy,
            is_test: false,
            default_browser: options.default_browser,
            firefox_spec,
        }
    }

    #[cfg(test)]
    pub fn new_test() -> Self {
        Self::new_test_with_port(9999)
    }

    #[cfg(test)]
    pub fn new_test_with_port(port: u16) -> Self {
        let params = chrome_instance::launch::LaunchParams::new(
            "127.0.0.1".into(),
            port,
            crate::cli::BrowserFlags::default(),
        );
        let pool = Arc::new(cdp_browser_lite::BrowserPool::new());
        let registry = Arc::new(chrome_instance::registry::Registry::new(8));

        let session = Arc::new(BrowserSession::new(
            Arc::new(Mutex::new(chrome_instance::MockChromeManager::new(port))),
            cdp_domains::webmcp::WebmcpAvailability::initial(false),
            false,
        ));

        registry.register_descriptor(
            chrome_instance::registry::InstanceDescriptor::default_instance(port, vec![]),
        );

        Self {
            default_session: session,
            registry,
            pool,
            base_params: params,
            flags: Default::default(),
            proxy: Default::default(),
            is_test: true,
            default_browser: Default::default(),
            firefox_spec: Default::default(),
        }
    }
}

impl Default for McpHandler {
    fn default() -> Self {
        Self::new_with_params(HandlerParams::default())
    }
}

pub(crate) fn is_local_address(url_str: &str) -> bool {
    // A blank page loads nothing (the default page of `open_tab`).
    if url_str == "about:blank" {
        return true;
    }
    let Ok(url) = url::Url::parse(url_str) else {
        return false;
    };
    // localhost, 127.0.0.1, [::1], 192.168.x.x (as an IP address, not a
    // domain name that starts like one), or .local
    match url.host() {
        Some(url::Host::Domain(host)) => host == "localhost" || host.ends_with(".local"),
        Some(url::Host::Ipv4(ip)) => {
            ip.octets() == [127, 0, 0, 1] || ip.octets()[..2] == [192, 168]
        }
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

pub(crate) fn extract_from_value<'a>(
    value: &'a Option<serde_json::Value>,
    param_name: &str,
) -> Option<&'a str> {
    value
        .as_ref()
        .and_then(|p| p.get(param_name))
        .and_then(|v| v.as_str())
}

pub(crate) fn find_line_column(source: &str, pattern: &str) -> Option<(u32, u32)> {
    let byte_index = source.find(pattern)?;
    let prefix = &source[..byte_index];
    let line_number = prefix.lines().count().saturating_sub(1) as u32;
    let column_number = prefix.lines().last().map(|line| line.len()).unwrap_or(0) as u32;

    Some((line_number, column_number))
}

#[async_trait]
impl ServerHandler for McpHandler {
    async fn handle_list_tools_request(
        &self,
        _request: Option<PaginatedRequestParams>,
        _runtime: Arc<dyn McpServer>,
    ) -> std::result::Result<ListToolsResult, RpcError> {
        let allow_cookie_import = self.flags.allow_cookie_import;
        Ok(ListToolsResult {
            tools: registry::listed_tools(self)
                .into_iter()
                .map(|t| schema_compat::normalize_tool_with_options(t, allow_cookie_import))
                .collect(),
            meta: None,
            next_cursor: None,
        })
    }

    async fn handle_call_tool_request(
        &self,
        params: CallToolRequestParams,
        _runtime: Arc<dyn McpServer>,
    ) -> std::result::Result<CallToolResult, CallToolError> {
        registry::call(self, params).await
    }
}

/// A handler on mock browsers, for tests of other modules.
#[cfg(test)]
pub(crate) fn test_handler() -> McpHandler {
    McpHandler::new_test()
}

/// The names of the tools `handler` lists.
#[cfg(test)]
pub(crate) fn listed_tool_names(handler: &McpHandler) -> Vec<String> {
    registry::listed_tools(handler)
        .into_iter()
        .map(|t| t.name)
        .collect()
}

#[cfg(test)]
mod tests {
    /// SEC-18: Observation buffers are bounded.
    #[test]
    fn observation_buffers_are_bounded() {
        // Given empty per-tab states, When far more items than the caps are pushed
        let mut debugger = DebuggerState::default();
        for i in 0..5000 {
            let info = ScriptInfo {
                hash: String::new(),
                start_line: 0,
                start_column: 0,
                url: String::new(),
            };
            debugger.scripts.insert(i.to_string(), info);
        }
        let mut network = NetworkState::default();
        for i in 0..500 {
            network.insert_ws_connection(i.to_string(), "wss://x".into());
            let frame = WebSocketFrame {
                url: String::new(),
                payload_data: String::new(),
                is_sent: true,
            };
            network.push_ws_frame(i.to_string(), frame);
        }
        let mut webmcp = cdp_domains::webmcp::WebmcpState::default();
        for i in 0..2000 {
            webmcp.invocations.insert(i.to_string(), Default::default());
        }
        // Then each keeps at most its cap, dropping the oldest
        assert_eq!(debugger.scripts.len(), MAX_SCRIPTS);
        assert!(!debugger.scripts.contains_key("0") && debugger.scripts.contains_key("4999"));
        assert_eq!(network.ws_connections.len(), MAX_WS_CONNECTIONS);
        assert_eq!(network.ws_frames.len(), MAX_WS_CONNECTIONS);
        assert!(
            !network.ws_frames.contains_key("0"),
            "evicted connection keeps frames"
        );
        assert_eq!(
            webmcp.invocations.len(),
            cdp_domains::webmcp::MAX_INVOCATIONS
        );
    }

    use super::*;
    use serde_json::json;
    struct DummyMcpServer {}

    // A dummy implementation of McpServer for testing
    #[async_trait]
    impl McpServer for DummyMcpServer {
        async fn start(self: Arc<Self>) -> rust_mcp_sdk::error::SdkResult<()> {
            Ok(())
        }
        async fn set_client_details(
            &self,
            _client_details: InitializeRequestParams,
        ) -> rust_mcp_sdk::error::SdkResult<()> {
            Ok(())
        }
        fn server_info(&self) -> &InitializeResult {
            unimplemented!()
        }
        fn client_info(&self) -> Option<InitializeRequestParams> {
            None
        }
        async fn auth_info(
            &self,
        ) -> tokio::sync::RwLockReadGuard<'_, Option<rust_mcp_sdk::auth::AuthInfo>> {
            unimplemented!()
        }
        async fn auth_info_cloned(&self) -> Option<rust_mcp_sdk::auth::AuthInfo> {
            None
        }
        async fn update_auth_info(&self, _auth_info: Option<rust_mcp_sdk::auth::AuthInfo>) {}
        async fn wait_for_initialization(&self) {}
        fn task_store(&self) -> Option<Arc<rust_mcp_sdk::task_store::ServerTaskStore>> {
            None
        }
        fn client_task_store(&self) -> Option<Arc<rust_mcp_sdk::task_store::ClientTaskStore>> {
            None
        }
        async fn stderr_message(&self, _message: String) -> rust_mcp_sdk::error::SdkResult<()> {
            Ok(())
        }
        fn session_id(&self) -> Option<String> {
            None
        }
        async fn send(
            &self,
            _message: MessageFromServer,
            _request_id: Option<RequestId>,
            _request_timeout: Option<std::time::Duration>,
        ) -> rust_mcp_sdk::error::SdkResult<Option<ClientMessage>> {
            Ok(None)
        }
        async fn send_batch(
            &self,
            _messages: Vec<ServerMessage>,
            _request_timeout: Option<std::time::Duration>,
        ) -> rust_mcp_sdk::error::SdkResult<Option<Vec<ClientMessage>>> {
            Ok(None)
        }
    }
    #[test]
    fn test_is_local_address() {
        let local = [
            "http://localhost",
            "https://localhost:8080",
            "http://127.0.0.1",
            "http://127.0.0.1:3000/path",
            "http://[::1]",
            "http://[::1]:5173",
            "http://192.168.1.1",
            "https://192.168.0.100/admin",
            "http://mydevice.local",
            "http://service.local:8000",
            // REV-03: the default page of open_tab loads nothing.
            "about:blank",
        ];
        let remote = [
            "http://google.com",
            "https://github.com/raultov",
            "http://8.8.8.8",
            "http://10.0.0.1",
            "not a url",
            "http://local.com",
            "http://192.167.1.1",
            // REV-02: a domain name that merely starts like a LAN address.
            "http://192.168.attacker.com/",
            "http://192.168.1.1.nip.io/",
        ];
        for url in local {
            assert!(is_local_address(url), "{url} must be local");
        }
        for url in remote {
            assert!(!is_local_address(url), "{url} must not be local");
        }
    }

    #[test]
    fn test_extract_from_value() {
        let val = Some(json!({"testKey": "testValue", "numKey": 42}));
        assert_eq!(extract_from_value(&val, "testKey"), Some("testValue"));
        assert_eq!(extract_from_value(&val, "numKey"), None); // As string fails
        assert_eq!(extract_from_value(&val, "missing"), None);
        assert_eq!(extract_from_value(&None, "testKey"), None);
    }

    #[test]
    fn test_find_line_column() {
        let source = "function test() {\n  let a = 1;\n  console.log(a);\n}";

        let (line, col) = find_line_column(source, "let a").unwrap();
        assert_eq!(line, 1);
        assert_eq!(col, 2);

        let (line, col) = find_line_column(source, "console.log").unwrap();
        assert_eq!(line, 2);
        assert_eq!(col, 2);

        let (line, col) = find_line_column(source, "function test").unwrap();
        assert_eq!(line, 0);
        assert_eq!(col, 0);

        assert_eq!(find_line_column(source, "not_found"), None);
    }

    #[test]
    fn test_handler_new_with_params() {
        let handler = McpHandler::new_with_params(HandlerParams {
            host: "host.docker.internal".into(),
            port: 9222,
            browser: crate::cli::BrowserFlags {
                headless: true,
                enable_automation: true,
                user_profile: false,
                ..Default::default()
            },
            server: crate::cli::ServerFlags {
                local: true,
                ..Default::default()
            },
            ..Default::default()
        });
        assert!(handler.flags.local);
        assert!(!handler.flags.allow_cookie_import);
        assert_eq!(handler.proxy.proxy_server, None);
    }

    #[test]
    fn test_handler_new_with_automation() {
        let handler = McpHandler::new_with_params(HandlerParams {
            host: "127.0.0.1".into(),
            port: 9444,
            default_browser: crate::backend::BrowserKind::Chrome,
            browser: crate::cli::BrowserFlags {
                headless: false,
                enable_automation: true,
                user_profile: true,
                ..Default::default()
            },
            server: crate::cli::ServerFlags {
                local: true,
                allow_cookie_import: false,
                enable_webmcp: true,
            },
            proxy: crate::cli::ProxyArgs {
                proxy_server: Some("http://proxy.example.com:8080".into()),
                proxy_username: Some("user".into()),
                proxy_password: Some("pass".into()),
            },
            firefox: Default::default(),
        });
        assert!(handler.flags.local);
        assert!(handler.flags.enable_webmcp);
        assert_eq!(
            handler.proxy.proxy_server,
            Some("http://proxy.example.com:8080".into())
        );
        assert_eq!(handler.proxy.proxy_username, Some("user".into()));
        assert_eq!(handler.proxy.proxy_password, Some("pass".into()));
    }

    #[tokio::test]
    async fn test_handle_list_tools_request() {
        let handler = McpHandler::new_test();
        let mock_server = Arc::new(DummyMcpServer {});

        let result = handler
            .handle_list_tools_request(None, mock_server.clone())
            .await;

        assert!(result.is_ok());
        let tools = result.unwrap().tools;

        // Without proxy and webmcp, 32 tools are listed
        assert_eq!(tools.len(), 32);
        let tool_names: Vec<String> = tools.into_iter().map(|t| t.name).collect();
        assert!(tool_names.contains(&"scroll".to_string()));
        assert!(tool_names.contains(&"capture_screenshot".to_string()));
        assert!(tool_names.contains(&"click_element".to_string()));
        assert!(tool_names.contains(&"fill_input".to_string()));
        assert!(tool_names.contains(&"evaluate_js".to_string()));
        assert!(tool_names.contains(&"navigate".to_string()));
        assert!(tool_names.contains(&"restart_browser".to_string()));
        assert!(tool_names.contains(&"stop_browser".to_string()));
        assert!(tool_names.contains(&"get_console_logs".to_string()));
        assert!(tool_names.contains(&"get_performance_metrics".to_string()));
        assert!(tool_names.contains(&"profile_page_performance".to_string()));
        assert!(!tool_names.contains(&"enable_proxy_auth".to_string()));
        assert!(!tool_names.contains(&"webmcp_list_tools".to_string()));
    }

    #[tokio::test]
    async fn test_handle_list_tools_request_with_proxy() {
        let mut handler = McpHandler::new_test();
        handler.proxy.proxy_server = Some("http://proxy.example.com:8080".to_string());
        let mock_server = Arc::new(DummyMcpServer {});

        let result = handler
            .handle_list_tools_request(None, mock_server.clone())
            .await;

        assert!(result.is_ok());
        let tools = result.unwrap().tools;

        // With proxy configured (and webmcp disabled), 31 tools are listed
        assert_eq!(tools.len(), 33);
        let tool_names: Vec<String> = tools.into_iter().map(|t| t.name).collect();
        assert!(tool_names.contains(&"enable_proxy_auth".to_string()));
    }

    #[tokio::test]
    async fn test_handle_list_tools_request_with_webmcp() {
        let mut handler = McpHandler::new_test();
        handler.flags.enable_webmcp = true;
        let mock_server = Arc::new(DummyMcpServer {});

        let result = handler
            .handle_list_tools_request(None, mock_server.clone())
            .await;

        assert!(result.is_ok());
        let tools = result.unwrap().tools;

        // With webmcp configured (30 + 4 = 34 tools)
        assert_eq!(tools.len(), 36);
        let tool_names: Vec<String> = tools.into_iter().map(|t| t.name).collect();
        assert!(tool_names.contains(&"webmcp_list_tools".to_string()));
        assert!(tool_names.contains(&"webmcp_invoke_tool".to_string()));
        assert!(tool_names.contains(&"webmcp_get_invocation".to_string()));
        assert!(tool_names.contains(&"webmcp_list_invocations".to_string()));
    }

    /// Gemini rejects a function declaration whose `items` sits next to a
    /// non-scalar `type`, so every published schema must use a single type
    /// name and give any array an `items` subschema.
    /// TOL-05: Every listed schema stays strict.
    #[tokio::test]
    async fn given_listed_tools_when_inspecting_schemas_then_types_are_scalar_and_arrays_are_typed()
    {
        let handler = McpHandler::new_test();
        let tools = handler
            .handle_list_tools_request(None, Arc::new(DummyMcpServer {}))
            .await
            .expect("listing tools must succeed")
            .tools;

        fn assert_strict(schema: &serde_json::Value, path: &str) {
            let serde_json::Value::Object(map) = schema else {
                return;
            };
            if let Some(type_) = map.get("type") {
                assert!(
                    type_.is_string(),
                    "{path}.type must be a single type name, got {type_}"
                );
            }
            if map.contains_key("items") {
                assert_eq!(
                    map.get("type").and_then(|t| t.as_str()),
                    Some("array"),
                    "{path} declares items so it must be typed as an array; got {schema}"
                );
            }
            for (key, child) in map {
                assert_strict(child, &format!("{path}.{key}"));
            }
        }

        for tool in tools {
            let schema = serde_json::to_value(&tool.input_schema).unwrap();
            assert_strict(&schema, &tool.name);
        }
    }

    #[tokio::test]
    async fn test_handle_call_tool_request_unknown_tool() {
        let handler = McpHandler::new_test();
        let mock_server = Arc::new(DummyMcpServer {});

        let params: CallToolRequestParams = serde_json::from_value(json!({
            "name": "non_existent_tool_123",
            "arguments": {}
        }))
        .unwrap();

        let result = handler.handle_call_tool_request(params, mock_server).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.to_string()
                .contains("Unknown tool: non_existent_tool_123")
        );
    }

    /// CDP-04: Lazy connection still enables the same domains, in order.
    #[tokio::test]
    async fn lazy_connect_enables_domains_in_order() {
        // Given a Chrome instance over the mock DevTools server
        let mock = cdp_domains::tests::spawn_recording_mock().await;
        let handler = cdp_domains::tests::mock_handler(mock.http_port);
        // When the first tool call needs the browser
        let session = handler.session(None).await.unwrap();
        drop(session.get_or_connect().await.unwrap());
        // Then the domains were enabled before the version was read, and the
        // debugger last (its listener must be running first)
        let sent: Vec<String> = mock
            .commands()
            .into_iter()
            .map(|c| c.method)
            .filter(|m| m.ends_with(".enable") || m == "Browser.getVersion")
            .collect();
        assert_eq!(
            sent,
            [
                "Runtime.enable",
                "Page.enable",
                "Network.enable",
                "Log.enable",
                "Performance.enable",
                "Browser.getVersion",
                "Debugger.enable"
            ]
        );
    }

    #[test]
    fn test_parse_chrome_major() {
        assert_eq!(parse_chrome_major("Chrome/152.0.6367.60"), Some(152));
        assert_eq!(parse_chrome_major("HeadlessChrome/151.0.1"), Some(151));
        assert_eq!(parse_chrome_major("Chrome/120.0"), Some(120));
        assert_eq!(parse_chrome_major("MockChrome/1.0"), None);
        assert_eq!(parse_chrome_major("Firefox/110.0"), None);
        assert_eq!(parse_chrome_major(""), None);
    }
}
