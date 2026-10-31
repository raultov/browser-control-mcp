pub mod cdp_domains;
pub mod chrome_instance;
pub(crate) mod debugging;
pub(crate) mod dialogs;
pub(crate) mod evaluation;
pub(crate) mod interruption;
pub(crate) mod navigation;
mod registry;
pub(crate) mod routes;
mod schema_compat;
pub(crate) mod settle;
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
    /// The script's last line in its resource (`endLine`).
    #[serde(default)]
    pub end_line: i32,
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
    /// Where the kept pause stopped (`Debugger.paused`'s top frame).
    pub paused_at: Option<debugging::PauseLocation>,
    /// Identifier of the `debugger;` script `pause_on_load` installed, removed
    /// on the next `resume` so later navigations do not pause again.
    pub pause_on_load_script: Option<String>,
    /// Breakpoints by MCP id (`bp-N`) → CDP breakpoint id.
    pub breakpoints: debugging::BreakpointIds<String>,
}

impl DebuggerState {
    /// Whether the page must not pause: the debugger is always attached, so
    /// without a breakpoint or a `pause_on_load` the only pauses are the
    /// page's own `debugger;` statements, which anti-debugging pages loop on
    /// to freeze every tool.
    pub(crate) fn skips_pauses(&self) -> bool {
        self.breakpoints.is_empty() && self.pause_on_load_script.is_none()
    }
}

#[derive(Clone, Debug, Default, ::serde::Serialize, ::serde::Deserialize)]
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
    /// `"base64"` when `response_body` is base64 (a binary body).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_body_encoding: Option<String>,
    /// Why the request failed (the browser's network error), when it did:
    /// without it a failed request looked like one still waiting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// URLs this request was redirected from, oldest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub redirects: Vec<String>,
    /// The response finished loading (or failed): only then is its body
    /// asked for (an open stream never finishes).
    #[serde(skip)]
    pub finished: bool,
}

#[derive(Clone, Debug, ::serde::Serialize, ::serde::Deserialize)]
pub struct WebSocketFrame {
    pub url: String,
    /// The payload, within [`WS_PAYLOAD_MAX`] characters.
    pub payload_data: String,
    /// Length of the whole payload (bytes).
    pub payload_len: usize,
    pub is_sent: bool,
}

/// Most characters kept of a WebSocket payload: 500 frames of a connection
/// streaming megabytes would fill memory and every reply.
pub(crate) const WS_PAYLOAD_MAX: usize = 64_000;

impl WebSocketFrame {
    /// A frame keeping at most [`WS_PAYLOAD_MAX`] characters of `payload`.
    pub(crate) fn new(url: String, payload: String, is_sent: bool) -> Self {
        Self {
            url,
            payload_len: payload.len(),
            payload_data: crate::util::text::capped(&payload, WS_PAYLOAD_MAX),
            is_sent,
        }
    }
}

pub(crate) const MAX_REQUESTS_PER_NAVIGATION: usize = 1000;
pub(crate) const MAX_NAVIGATIONS_RETAINED: usize = 3;
pub(crate) const MAX_WS_FRAMES_PER_CONNECTION: usize = 500;
/// Navigations whose document request id is remembered (Firefox).
pub(crate) const MAX_DOCUMENTS: usize = 16;

impl NetworkRequest {
    /// The value of the response header `name` (any case).
    pub(crate) fn response_header(&self, name: &str) -> Option<&str> {
        let headers = self.response_headers.as_ref()?.as_object()?;
        let (_, value) = headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))?;
        value.as_str()
    }

    /// Whether the response asks to be saved (`Content-Disposition:
    /// attachment`), so the browser downloads it instead of showing it.
    pub(crate) fn is_attachment(&self) -> bool {
        self.response_header("content-disposition")
            .is_some_and(|value| value.trim().to_ascii_lowercase().starts_with("attachment"))
    }
}
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
    /// Document request id per navigation id, where they differ (BiDi's
    /// `navigation`; Chrome's document request id is its loader id).
    pub documents: crate::util::bounded::BoundedMap<String, String, MAX_DOCUMENTS>,
    /// Headers Chrome reports only in its ExtraInfo events (Cookie,
    /// Set-Cookie...), per request id: they may come before or after the
    /// request and its response, so they are kept and merged each time.
    pub extra_headers: crate::util::bounded::BoundedMap<String, ExtraHeaders, MAX_EXTRA_HEADERS>,
}

/// URL schemes of the browser's own requests (the new-tab page, DevTools,
/// extensions) and of inline data: never the page traffic an agent asks
/// about, and the bulk of a fresh browser's log.
const INTERNAL_SCHEMES: &[&str] = &[
    "chrome:",
    "chrome-extension:",
    "chrome-untrusted:",
    "chrome-search:",
    "devtools:",
    "data:",
    "about:",
    "moz-extension:",
    "resource:",
];

/// Whether `url` is one of the browser's own requests ([`INTERNAL_SCHEMES`]).
fn is_internal_url(url: &str) -> bool {
    INTERNAL_SCHEMES.iter().any(|scheme| {
        url.as_bytes()
            .get(..scheme.len())
            .is_some_and(|start| start.eq_ignore_ascii_case(scheme.as_bytes()))
    })
}

/// Requests whose ExtraInfo headers are kept.
pub(crate) const MAX_EXTRA_HEADERS: usize = 256;

/// The ExtraInfo headers of one request (see [`NetworkState::extra_headers`]).
#[derive(Clone, Debug, Default)]
pub struct ExtraHeaders {
    /// From `Network.requestWillBeSentExtraInfo`.
    pub request: Option<serde_json::Value>,
    /// From `Network.responseReceivedExtraInfo`.
    pub response: Option<serde_json::Value>,
}

/// `extra` merged into `headers` (a name in any case replaces the same name).
fn merge_headers(headers: &mut Option<serde_json::Value>, extra: &serde_json::Value) {
    let Some(extra) = extra.as_object() else {
        return;
    };
    let merged = headers.get_or_insert_with(|| serde_json::json!({}));
    let Some(map) = merged.as_object_mut() else {
        return;
    };
    for (name, value) in extra {
        map.retain(|key, _| !key.eq_ignore_ascii_case(name));
        map.insert(name.clone(), value.clone());
    }
}

impl Default for NetworkState {
    fn default() -> Self {
        let mut navigations = std::collections::VecDeque::new();
        navigations.push_back(NavigationBucket::default());
        Self {
            navigations,
            ws_connections: Default::default(),
            ws_frames: Default::default(),
            documents: Default::default(),
            extra_headers: Default::default(),
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
        if is_internal_url(&req.url) {
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

    /// Keeps ExtraInfo headers of `id` (`response` false: the request's)
    /// and merges what is known into the logged request.
    pub(crate) fn note_extra_headers(
        &mut self,
        id: &str,
        response: bool,
        headers: serde_json::Value,
    ) {
        if self.extra_headers.get_mut(id).is_none() {
            self.extra_headers
                .insert(id.to_string(), ExtraHeaders::default());
        }
        if let Some(extra) = self.extra_headers.get_mut(id) {
            match response {
                true => extra.response = Some(headers),
                false => extra.request = Some(headers),
            }
        }
        self.apply_extra_headers(id);
    }

    /// Merges the kept ExtraInfo headers of `id` into its logged request.
    pub(crate) fn apply_extra_headers(&mut self, id: &str) {
        let Some(extra) = self.extra_headers.get(id).cloned() else {
            return;
        };
        let Some(request) = self.find_request_mut(id) else {
            return;
        };
        if let Some(headers) = &extra.request {
            merge_headers(&mut request.request_headers, headers);
        }
        if let Some(headers) = &extra.response {
            merge_headers(&mut request.response_headers, headers);
        }
    }

    /// Records that `request_id` is the document of navigation `navigation`.
    pub(crate) fn note_document(&mut self, navigation: String, request_id: String) {
        self.documents.insert(navigation, request_id);
    }

    /// The document request of navigation (or loader) `id`.
    pub(crate) fn document(&self, id: &str) -> Option<&NetworkRequest> {
        let request_id = self.documents.get(id).map_or(id, String::as_str);
        self.navigations
            .iter()
            .find_map(|bucket| bucket.requests.get(request_id))
    }

    pub(crate) fn find_request_mut(&mut self, id: &str) -> Option<&mut NetworkRequest> {
        for bucket in &mut self.navigations {
            if let Some(req) = bucket.requests.get_mut(id) {
                return Some(req);
            }
        }
        None
    }

    /// Starts a new navigation's bucket. `document` is the request id of
    /// the navigation's document when it was sent before the split (Chrome
    /// reports `frameNavigated` after the request): it moves to the new
    /// bucket, where `current_navigation_only` looks for it.
    pub(crate) fn split_after_navigation(&mut self, document: Option<&str>) {
        let moved = document.and_then(|id| {
            let bucket = self.navigations.front_mut()?;
            let request = bucket.requests.remove(id)?;
            bucket.order.retain(|other| other != id);
            Some((id.to_string(), request))
        });
        self.navigations.push_front(NavigationBucket::default());
        self.navigations.truncate(MAX_NAVIGATIONS_RETAINED);
        if let (Some((id, request)), Some(bucket)) = (moved, self.navigations.front_mut()) {
            bucket.order.push_back(id.clone());
            bucket.requests.insert(id, request);
        }
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

    /// The ids of the logged requests, oldest first (`current_only`: of
    /// the current navigation).
    pub(crate) fn ordered_ids(&self, current_only: bool) -> Vec<String> {
        // The front bucket is the current navigation.
        let buckets = self.navigations.iter().rev();
        let skip = if current_only {
            self.navigations.len().saturating_sub(1)
        } else {
            0
        };
        buckets
            .skip(skip)
            .flat_map(|bucket| bucket.order.iter().cloned())
            .collect()
    }

    /// The ids of every logged request.
    pub(crate) fn known_ids(&self) -> std::collections::HashSet<String> {
        self.navigations
            .iter()
            .flat_map(|bucket| bucket.requests.keys().cloned())
            .collect()
    }

    /// The requests not in `known`: `(how many, how many still loading)`.
    pub(crate) fn new_since(&self, known: &std::collections::HashSet<String>) -> (usize, usize) {
        let new = self
            .navigations
            .iter()
            .flat_map(|bucket| bucket.requests.iter())
            .filter(|(id, _)| !known.contains(*id));
        new.fold((0, 0), |(seen, loading), (_, request)| {
            let open = !request.finished && request.error.is_none();
            (seen + 1, loading + usize::from(open))
        })
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

    /// Forgets the requests `ids` (a `clear` that returned them).
    pub(crate) fn remove_requests<'a>(&mut self, ids: impl Iterator<Item = &'a String>) {
        let ids: std::collections::HashSet<&String> = ids.collect();
        for bucket in &mut self.navigations {
            bucket.requests.retain(|id, _| !ids.contains(id));
            bucket.order.retain(|id| !ids.contains(id));
        }
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

/// Sends one command; a failure becomes `"<failure>: <error>"` (a command a
/// dialog interrupted gives the dialog's message alone).
pub(crate) async fn send_or(
    target: &cdp_domains::cdp_target::CdpTarget,
    method: &str,
    params: serde_json::Value,
    failure: &str,
) -> std::result::Result<cdp_browser_lite::WsResponse, CallToolError> {
    target
        .send_raw_command(method, params)
        .await
        .map_err(|e| CallToolError::from_message(cdp_failure(failure, &e)))
}

/// A CDP error as the agent reads it: `"<failure>: <error text>"`, or the
/// message alone for a command a JavaScript dialog interrupted.
pub(crate) fn cdp_failure(failure: &str, error: &cdp_browser_lite::CdpError) -> String {
    match error {
        cdp_browser_lite::CdpError::ProtocolError { message, .. }
            if dialogs::interrupted(error) =>
        {
            message.clone()
        }
        cdp_browser_lite::CdpError::Disconnected => format!(
            "{failure}: the DevTools connection to Chrome dropped. A reply over 256 MB (a huge evaluate_js result, page, script or response body) closes it: keep results small (slice strings, query the DOM). The next call reconnects; tab ids are re-assigned (call list_tabs)."
        ),
        other => format!("{failure}: {other}"),
    }
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
    snippet_outcome(reply.result.unwrap_or_default())
}

/// A tool's arguments and the Chrome target they address (`ids` reads its
/// `instance_id` and `tab_id`).
pub(crate) async fn args_and_target<T: serde::de::DeserializeOwned>(
    params: CallToolRequestParams,
    handler: &McpHandler,
    ids: impl Fn(&T) -> (Option<String>, Option<String>),
) -> std::result::Result<(T, cdp_domains::cdp_target::CdpTarget), CallToolError> {
    let args: T = parse_args(params)?;
    let (instance_id, tab_id) = ids(&args);
    let target = resolve_target(handler, instance_id, tab_id).await?;
    Ok((args, target))
}

/// A tool that evaluates one shared `util::js` snippet in the page and
/// answers from its value, the same on every browser (`scroll`,
/// `snapshot`): on Chrome its whole handler is [`handle_snippet_tool`].
pub(crate) trait SnippetTool: serde::de::DeserializeOwned {
    /// The instance and tab it addresses.
    fn target_args(&self) -> TargetArgs;
    /// Why the arguments make no sense, checked before any browser work.
    fn check(&self) -> std::result::Result<(), CallToolError> {
        Ok(())
    }
    /// The snippet.
    fn snippet(&self) -> String;
    /// The reply from the snippet's value.
    fn answer(
        &self,
        outcome: &serde_json::Value,
    ) -> std::result::Result<CallToolResult, CallToolError>;
}

/// The Chrome handler of a [`SnippetTool`].
pub(crate) async fn handle_snippet_tool<T: SnippetTool>(
    params: CallToolRequestParams,
    handler: &McpHandler,
) -> std::result::Result<CallToolResult, CallToolError> {
    let args: T = parse_args(params)?;
    args.check()?;
    let ids = args.target_args();
    let target = resolve_target(handler, ids.instance_id, ids.tab_id).await?;
    let outcome = snippet_value(&target, &args.snippet()).await?;
    args.answer(&outcome)
}

/// The value of a snippet's `Runtime.evaluate` result, or the exception it
/// threw as an error (its first line).
pub(crate) fn snippet_outcome(
    result: serde_json::Value,
) -> std::result::Result<serde_json::Value, CallToolError> {
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

/// The error of a call on an instance whose browser died (crashed, killed
/// or closed from outside): its tabs are gone and nothing is relaunched
/// behind the agent's back.
pub(crate) const BROWSER_GONE: &str = "The browser of this instance closed or crashed, and its tabs are gone. Call 'restart_browser' for this instance to start a new one (or 'close_instance' to drop the instance).";

/// How long the first call on a fresh connection waits for the browser's
/// page to register as a tab (target discovery runs in the background).
const FIRST_TAB_WAIT: std::time::Duration = std::time::Duration::from_secs(3);

/// How long the first observation call waits for replayed events.
const REPLAY_SETTLE: std::time::Duration = std::time::Duration::from_millis(200);

/// The active tab of `tabs` as a target, once one is registered, polling
/// until `limit`.
async fn wait_for_active_tab(
    tabs: &std::sync::RwLock<chrome_instance::tab_registry::TabRegistry>,
    limit: std::time::Duration,
) -> Option<cdp_domains::cdp_target::CdpTarget> {
    let deadline = tokio::time::Instant::now() + limit;
    loop {
        {
            let registry = crate::util::sync::read(tabs);
            let active = registry.active_tab_id.as_ref();
            if let Some(entry) = active.and_then(|id| registry.tabs.get(id)) {
                let target = cdp_domains::cdp_target::CdpTarget::tab(entry.tab.clone());
                return Some(target.with_debugger(entry.debugger_state.clone()));
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
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
    pub(crate) dialog_state: Arc<Mutex<dialogs::DialogState>>,
    pub(crate) route_state: Arc<Mutex<routes::RouteState>>,
    pub(crate) chrome_manager: Arc<Mutex<dyn chrome_instance::ChromeManager>>,
    pub(crate) tabs: Arc<std::sync::RwLock<chrome_instance::tab_registry::TabRegistry>>,
    pub(crate) session_listeners: Arc<Mutex<cdp_domains::event_pump::ListenerHandles>>,
    pub(crate) browser_version: Arc<Mutex<Option<BrowserVersion>>>,
    pub(crate) enable_webmcp: bool,
    /// Proxy-auth handler task per target (`None` = the session-level page),
    /// so enabling it again replaces the previous handler instead of stacking.
    pub(crate) proxy_auth_tasks: cdp_domains::fetch::ProxyAuthTasks,
    /// Set when a fresh connection started target discovery and no call has
    /// waited for the browser's page to register as a tab yet.
    first_tab_pending: std::sync::atomic::AtomicBool,
    /// Set while the session-level connection has its debugger enabled;
    /// turned off once a tab is registered (see `retire_fallback_debugger`).
    fallback_debugger: std::sync::atomic::AtomicBool,
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
            dialog_state: Default::default(),
            route_state: Default::default(),
            chrome_manager,
            tabs: Arc::new(std::sync::RwLock::new(
                chrome_instance::tab_registry::TabRegistry::new(16),
            )),
            session_listeners: Default::default(),
            browser_version: Default::default(),
            proxy_auth_tasks: Default::default(),
            enable_webmcp,
            first_tab_pending: Default::default(),
            fallback_debugger: Default::default(),
        }
    }

    /// Resets the cached CDP client and clears the tab registry so that the
    /// next tool call reconnects to a freshly launched Chrome instance.
    pub(crate) async fn reset_connection_state(&self) {
        *self.client.lock().await = None;
        *self.session_listeners.lock().await = cdp_domains::event_pump::ListenerHandles::default();
        *self.browser_version.lock().await = None;
        self.first_tab_pending
            .store(false, std::sync::atomic::Ordering::SeqCst);
        self.fallback_debugger
            .store(false, std::sync::atomic::Ordering::SeqCst);
        self.proxy_auth_tasks.lock().await.clear();
        *self.route_state.lock().await = Default::default();
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

        let target = cdp_domains::cdp_target::CdpTarget::client(client.clone());
        let mut handles = self.start_session_listeners(&target);
        // WebMCP.enable replays the page's tools: its listener runs first.
        self.enable_session_webmcp(client).await;
        // Debugger.enable replays `scriptParsed`: the listener runs first.
        let _ = client
            .send_raw_command("Debugger.enable", cdp_browser_lite::NoParams)
            .await;
        cdp_domains::debugger::sync_pauses(&target, &self.debugger_state).await;
        self.fallback_debugger
            .store(true, std::sync::atomic::Ordering::SeqCst);
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
            self.first_tab_pending
                .store(true, std::sync::atomic::Ordering::SeqCst);
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
        handles.push(cdp_domains::page::start_dialog_listener(
            target,
            self.dialog_state.clone(),
        ));
        handles
    }

    /// The registered tab a call addresses: `tab_id` (an error when unknown),
    /// else the active tab; `None` when no tab is registered yet.
    fn registered_target(
        &self,
        tab_id: Option<String>,
    ) -> std::result::Result<Option<cdp_domains::cdp_target::CdpTarget>, CallToolError> {
        let registry = crate::util::sync::read(&self.tabs);
        let tab = |id: &String| {
            registry.tabs.get(id).map(|entry| {
                cdp_domains::cdp_target::CdpTarget::tab(entry.tab.clone())
                    .with_debugger(entry.debugger_state.clone())
            })
        };
        let Some(id) = tab_id else {
            return Ok(registry.active_tab_id.as_ref().and_then(tab));
        };
        if let Some(found) = tab(&id) {
            return Ok(Some(found));
        }
        let available: Vec<_> = registry.tabs.keys().cloned().collect();
        let avail_str = if available.is_empty() {
            "none (if Chrome was restarted, previous tab IDs are no longer valid)".to_string()
        } else {
            available.join(", ")
        };
        Err(CallToolError::from_message(format!(
            "Tab with ID '{}' not found in this session. Available tabs: {}. Use 'list_tabs' to discover current tab IDs.",
            id, avail_str
        )))
    }

    /// Fails with [`BROWSER_GONE`] when the browser died, forgetting its
    /// tabs and connections (they belong to the dead process).
    /// Whether a DevTools connection is open (a tab registered, or the
    /// session-level client).
    pub(crate) async fn is_connected(&self) -> bool {
        let has_tab = !crate::util::sync::read(&self.tabs).tabs.is_empty();
        has_tab || self.client.lock().await.is_some()
    }

    /// Lets the events Chrome replays on a fresh connection (console
    /// messages logged before it) reach the tab's buffers: they arrive with
    /// the domain's `enable` reply, but listener tasks apply them.
    pub(crate) async fn settle_replayed_events(&self) {
        tokio::time::sleep(REPLAY_SETTLE).await;
    }

    /// Refuses a restart of an instance attached to a Chrome this server
    /// did not launch (see [`chrome_instance::attached_restart_refused`]).
    pub(crate) async fn check_not_attached(&self) -> std::result::Result<(), CallToolError> {
        let manager = self.chrome_manager.lock().await;
        if manager.is_attached().await {
            return Err(CallToolError::from_message(
                chrome_instance::attached_restart_refused(manager.get_port()),
            ));
        }
        Ok(())
    }

    /// Also replaces a DevTools connection that dropped while the browser
    /// lives (see [`Self::reconnect_if_dropped`]).
    pub(crate) async fn check_browser(&self) -> std::result::Result<(), CallToolError> {
        let crashed = self.chrome_manager.lock().await.crashed().await;
        if crashed {
            self.reset_connection_state().await;
            return Err(CallToolError::from_message(BROWSER_GONE));
        }
        self.reconnect_if_dropped().await
    }

    /// The DevTools WebSocket closes when one of Chrome's messages exceeds
    /// the client's 256 MB limit (cdp-lite's `MAX_MESSAGE_SIZE`; a huge
    /// `evaluate_js` result, page or script): the tabs registered over it
    /// would answer `Connection lost` forever. A dead connection is
    /// forgotten with its tabs, and the session reconnects and waits for the
    /// pages to register again (with new tab ids).
    async fn reconnect_if_dropped(&self) -> std::result::Result<(), CallToolError> {
        let tab_client = {
            let registry = crate::util::sync::read(&self.tabs);
            registry
                .tabs
                .values()
                .next()
                .map(|e| e.tab.client().clone())
        };
        let session_client = self.client.lock().await.clone();
        let Some(client) = tab_client.or(session_client) else {
            return Ok(());
        };
        let probe = client
            .send_raw_command("Browser.getVersion", cdp_browser_lite::NoParams)
            .await;
        if !matches!(probe, Err(cdp_browser_lite::CdpError::Disconnected)) {
            return Ok(());
        }
        tracing::warn!("DevTools connection dropped; reconnecting");
        self.reset_connection_state().await;
        drop(self.get_or_connect().await?);
        if self
            .first_tab_pending
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            let _ = wait_for_active_tab(&self.tabs, FIRST_TAB_WAIT).await;
        }
        Ok(())
    }

    /// Turns off the session-level connection's debugger once the page is
    /// a registered tab: tools then only read the tab's state, and that
    /// debugger, with no breakpoint of its own, would resume the tab's
    /// pauses (every debugger on a page can resume it).
    async fn retire_fallback_debugger(&self) {
        if !self
            .fallback_debugger
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return;
        }
        let client = self.client.lock().await.clone();
        if let Some(client) = client {
            let _ = client
                .send_raw_command("Debugger.disable", cdp_browser_lite::NoParams)
                .await;
        }
    }

    /// The target a call addresses, refused while a JavaScript dialog is
    /// open on it: every command that runs page code would wait for it.
    pub(crate) async fn target(
        &self,
        tab_id: Option<String>,
    ) -> std::result::Result<cdp_domains::cdp_target::CdpTarget, CallToolError> {
        let found = self.target_ignoring_dialogs(tab_id.clone()).await?;
        // Bound first: the error type is not `Send` across an await.
        let dialog = self.dialog_state(tab_id)?;
        let open = dialog.lock().await.open.clone();
        match open {
            Some(dialog) => Err(CallToolError::from_message(dialog.blocking())),
            None => Ok(found),
        }
    }

    /// The target a call addresses even while a dialog is open (for
    /// `handle_dialog` and raw protocol commands).
    pub(crate) async fn target_ignoring_dialogs(
        &self,
        tab_id: Option<String>,
    ) -> std::result::Result<cdp_domains::cdp_target::CdpTarget, CallToolError> {
        self.check_browser().await?;
        // Bound first: the lookup's error type is not `Send` across an await.
        let registered = self.registered_target(tab_id)?;
        if let Some(found) = registered {
            self.retire_fallback_debugger().await;
            return Ok(found);
        }
        let client = self.get_or_connect().await?.clone();
        // A fresh connection discovers the browser's page in the background:
        // the first call waits for it, so its traffic lands in the tab's
        // state, which every later call reads (the session-level copy is
        // only read while no tab is registered).
        if self
            .first_tab_pending
            .swap(false, std::sync::atomic::Ordering::SeqCst)
            && let Some(tab) = wait_for_active_tab(&self.tabs, FIRST_TAB_WAIT).await
        {
            self.retire_fallback_debugger().await;
            return Ok(tab);
        }
        client
            .map(|client| {
                cdp_domains::cdp_target::CdpTarget::client(client)
                    .with_debugger(self.debugger_state.clone())
            })
            .ok_or_else(|| {
                CallToolError::from_message(
                    "Failed to retrieve target: no active tabs or default connection available",
                )
            })
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
    state_accessor!(dialog_state, dialogs::DialogState);
    state_accessor!(route_state, routes::RouteState);

    /// Whether an `enable_proxy_auth` handler still runs: it owns Chrome's
    /// Fetch domain, which mocked routes need too.
    pub(crate) async fn proxy_auth_running(&self) -> bool {
        let tasks = self.proxy_auth_tasks.lock().await;
        tasks
            .values()
            .any(cdp_domains::event_pump::ListenerHandles::is_running)
    }
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
    pub upload: crate::cli::UploadArgs,
    pub tools: crate::cli::ToolArgs,
    /// `--secrets`, already read.
    pub secrets: crate::secrets::Secrets,
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
            upload: Default::default(),
            tools: Default::default(),
            secrets: Default::default(),
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
    /// `--upload-root` directories (empty: the defaults of `upload_file`).
    pub(crate) upload_roots: Vec<std::path::PathBuf>,
    /// `--tool-groups`: the opt-in groups listed and callable.
    pub(crate) tool_groups: Vec<crate::cli::ToolGroup>,
    /// `--secrets`: masked in every tool outcome, typed by `<secret:NAME>`.
    pub(crate) secrets: crate::secrets::Secrets,
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
            upload_roots: options.upload.upload_roots,
            tool_groups: options.tools.tool_groups,
            secrets: options.secrets,
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
            upload_roots: Vec::new(),
            tool_groups: Vec::new(),
            secrets: Default::default(),
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

/// The 0-based line and column of the first `pattern` in `source`, in the
/// units the browsers' debuggers use: lines end at `\n`, columns count
/// UTF-16 code units.
pub(crate) fn find_line_column(source: &str, pattern: &str) -> Option<(u32, u32)> {
    let byte_index = source.find(pattern)?;
    let prefix = &source[..byte_index];
    let line_start = prefix.rfind('\n').map_or(0, |i| i + 1);
    let line = prefix.matches('\n').count();
    let column = prefix[line_start..].encode_utf16().count();
    Some((
        u32::try_from(line).unwrap_or(u32::MAX),
        u32::try_from(column).unwrap_or(u32::MAX),
    ))
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
                end_line: 0,
                url: String::new(),
            };
            debugger.scripts.insert(i.to_string(), info);
        }
        let mut network = NetworkState::default();
        for i in 0..500 {
            network.insert_ws_connection(i.to_string(), "wss://x".into());
            let frame = WebSocketFrame::new(String::new(), String::new(), true);
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

    /// AUD-LC-04: once the browser died, every call that needs it says so
    /// and what to do (it answered `CDP Error: Disconnected` forever), and
    /// the dead tabs are forgotten.
    #[tokio::test]
    async fn calls_on_a_dead_browser_say_to_restart() {
        // Given a session whose browser crashed, with a tab still registered
        let mut manager = chrome_instance::MockChromeManager::new(0);
        manager.set_crashed(true);
        let session = BrowserSession::new(
            Arc::new(Mutex::new(manager)),
            cdp_domains::webmcp::WebmcpAvailability::NotRequested,
            false,
        );
        let port = cdp_domains::tests::spawn_mock_chrome_server().await;
        let browser = cdp_browser_lite::BrowserClient::connect(
            &format!("127.0.0.1:{port}"),
            std::time::Duration::from_secs(2),
        )
        .await
        .unwrap();
        let tab = browser.attach("T-dead").await.unwrap();
        let info = chrome_instance::tab_registry::NewTab::new(None, "about:blank");
        crate::util::sync::write(&session.tabs)
            .register_tab(tab, info, false)
            .unwrap();
        // When a tool looks for its target
        let error = session.target(None).await.unwrap_err().to_string();
        // Then it is told to restart, and the dead tab is gone
        assert_eq!(error, BROWSER_GONE);
        assert!(crate::util::sync::read(&session.tabs).tabs.is_empty());
    }

    /// AUD-EV-07: once the page is registered as a tab, the session-level
    /// connection's debugger is turned off: with no breakpoints of its own
    /// it would resume the tab's pauses (`pause_on_load`, breakpoints).
    #[tokio::test]
    async fn fallback_debugger_is_disabled_once_a_tab_is_registered() {
        // Given a session that used its session-level connection
        let mock = cdp_domains::tests::spawn_recording_mock().await;
        let handler = cdp_domains::tests::mock_handler(mock.http_port);
        let session = handler.session(None).await.unwrap();
        session.target(None).await.unwrap();
        let fallback = |method: &str| {
            mock.commands_named(method)
                .into_iter()
                .filter(|c| c.session_id.is_none())
                .count()
        };
        assert_eq!(fallback("Debugger.enable"), 1);
        assert_eq!(fallback("Debugger.disable"), 0);
        // When a tab registers and calls use it, twice
        let browser = cdp_browser_lite::BrowserClient::connect(
            &format!("127.0.0.1:{}", mock.http_port),
            std::time::Duration::from_secs(2),
        )
        .await
        .unwrap();
        let tab = browser.attach("T-page").await.unwrap();
        let info = chrome_instance::tab_registry::NewTab::new(None, "about:blank");
        crate::util::sync::write(&session.tabs)
            .register_tab(tab, info, false)
            .unwrap();
        session.target(None).await.unwrap();
        session.target(None).await.unwrap();
        // Then the session-level debugger was disabled once
        assert_eq!(fallback("Debugger.disable"), 1);
    }

    /// ISSUE-18: the browser's own requests (the new-tab page's chrome://
    /// resources, data: URLs, DevTools) are not logged; the page's are.
    #[test]
    fn internal_requests_are_not_logged() {
        let mut network = NetworkState::default();
        for (id, url) in [
            ("a", "chrome://resources/js/x.js"),
            ("b", "data:image/gif;base64,R0lGOD"),
            ("c", "devtools://devtools/bundled/x.js"),
            ("d", "http://a/page"),
            ("e", "about:blank"),
        ] {
            let request = NetworkRequest {
                url: url.into(),
                ..Default::default()
            };
            network.insert_request(id.into(), request);
        }
        let logged: Vec<String> = network.ordered_ids(false);
        assert_eq!(logged, ["d"]);
    }

    /// AUD-EV-11: a navigation's document request is found by the
    /// navigation's id: Chrome's document request id is its loader id,
    /// Firefox maps navigation ids to request ids.
    #[test]
    fn document_request_is_found_by_navigation_id() {
        let request = |status| NetworkRequest {
            url: "http://a/".into(),
            method: "GET".into(),
            response_status: status,
            ..Default::default()
        };
        let mut network = NetworkState::default();
        network.insert_request("L1".into(), request(Some(404)));
        network.insert_request("r7".into(), request(Some(500)));
        network.note_document("nav-1".into(), "r7".into());
        let status = |id: &str| network.document(id).and_then(|r| r.response_status);
        assert_eq!(status("L1"), Some(404));
        assert_eq!(status("nav-1"), Some(500));
        assert_eq!(status("nav-2"), None);
    }

    /// AUD-OB-13: a huge WebSocket payload is kept cut, with its length.
    #[test]
    fn large_ws_payload_is_capped_with_its_length() {
        let frame = WebSocketFrame::new("wss://x".into(), "y".repeat(1_000_000), false);
        assert_eq!(frame.payload_len, 1_000_000);
        assert!(frame.payload_data.starts_with(&"y".repeat(WS_PAYLOAD_MAX)));
        assert!(
            frame
                .payload_data
                .ends_with("[cut: the first 64000 of 1000000 characters]")
        );
        let small = WebSocketFrame::new("wss://x".into(), "ping".into(), true);
        assert_eq!(
            (small.payload_data.as_str(), small.payload_len),
            ("ping", 4)
        );
    }

    /// AUD-EV-23: a tab whose DevTools connection dropped (Chrome closes it
    /// on a message over 256 MB) while the browser lives is forgotten and the
    /// session reconnects, instead of answering "Connection lost" forever.
    #[tokio::test]
    async fn dropped_connection_is_reconnected() {
        use cdp_browser_lite::test_support::mock_devtools::{MockDevTools, MockWsBehavior};
        // Given a live browser and a registered tab whose connection closed
        let live = cdp_domains::tests::spawn_recording_mock().await;
        let handler = cdp_domains::tests::mock_handler(live.http_port);
        let session = handler.session(None).await.unwrap();
        let dying = MockDevTools::start(MockWsBehavior::CloseAfterOneCommand).await;
        let browser = cdp_browser_lite::BrowserClient::connect(
            &format!("127.0.0.1:{}", dying.http_port),
            std::time::Duration::from_secs(2),
        )
        .await
        .unwrap();
        let tab = browser.attach("T-dropped").await.unwrap();
        let info = chrome_instance::tab_registry::NewTab::new(None, "about:blank");
        crate::util::sync::write(&session.tabs)
            .register_tab(tab, info, false)
            .unwrap();
        // When a tool looks for its target
        let target = session.target(None).await.unwrap();
        // Then the dead tab is gone and the call runs on a live connection
        assert!(crate::util::sync::read(&session.tabs).tabs.is_empty());
        let reply = target
            .send_raw_command("Runtime.evaluate", serde_json::json!({"expression": "1"}))
            .await;
        assert!(reply.is_ok(), "{:?}", reply.err());
    }

    /// AUD-EV-23: "Connection lost" says why and what happens next.
    #[test]
    fn lost_connection_error_is_explained() {
        let text = cdp_failure("CDP Error", &cdp_browser_lite::CdpError::Disconnected);
        assert!(text.contains("256 MB"), "{text}");
        assert!(text.contains("next call reconnects"), "{text}");
    }

    /// AUD-LC-29: the first call on a fresh Chrome waits for its page to
    /// register as a tab and uses it, so the tab's state (network, scripts)
    /// sees that call's traffic instead of a session-level copy that tools
    /// stop reading once the tab registers.
    #[tokio::test]
    async fn first_call_waits_for_the_first_tab() {
        use cdp_browser_lite::BrowserClient;
        // Given an empty registry where the page registers 50 ms later
        let tabs = Arc::new(std::sync::RwLock::new(
            chrome_instance::tab_registry::TabRegistry::new(4),
        ));
        let port = cdp_domains::tests::spawn_mock_chrome_server().await;
        let browser = BrowserClient::connect(
            &format!("127.0.0.1:{port}"),
            std::time::Duration::from_secs(2),
        )
        .await
        .unwrap();
        let tab = browser.attach("T-first").await.unwrap();
        let later = tabs.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            let info = chrome_instance::tab_registry::NewTab::new(None, "about:blank");
            crate::util::sync::write(&later)
                .register_tab(tab, info, false)
                .unwrap();
        });
        // When the first call looks for its target
        let found = wait_for_active_tab(&tabs, std::time::Duration::from_secs(2)).await;
        // Then it gets that tab
        assert!(found.as_ref().and_then(|t| t.tab_target_id()) == Some("T-first"));
        // And with no page at all it gives up after the limit
        let empty = Arc::new(std::sync::RwLock::new(
            chrome_instance::tab_registry::TabRegistry::new(4),
        ));
        assert!(
            wait_for_active_tab(&empty, std::time::Duration::from_millis(60))
                .await
                .is_none()
        );
    }

    /// AUD-DB-01: a match at the start of a line is on that line, column 0
    /// (it was reported at the end of the previous line).
    #[test]
    fn match_at_line_start_is_on_its_line() {
        assert_eq!(
            find_line_column("let a=1;\nfunction foo(){}", "function foo"),
            Some((1, 0))
        );
        assert_eq!(find_line_column("a\n\n\nfoo", "foo"), Some((3, 0)));
        assert_eq!(find_line_column("a\r\nfoo", "foo"), Some((1, 0)));
    }

    /// AUD-DB-14: columns count UTF-16 code units, as the browsers do.
    #[test]
    fn columns_count_utf16_units() {
        assert_eq!(
            find_line_column("const s = 'ñ€😀'; foo()", "foo"),
            Some((0, 18))
        );
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
            upload: Default::default(),
            tools: Default::default(),
            secrets: Default::default(),
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

        // Without proxy, webmcp and tool groups, 33 tools are listed
        assert_eq!(tools.len(), 33);
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
        assert!(!tool_names.contains(&"get_performance_metrics".to_string()));
        assert!(!tool_names.contains(&"profile_page_performance".to_string()));
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

        // With proxy configured (and webmcp disabled), 34 tools are listed
        assert_eq!(tools.len(), 34);
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

        // With webmcp configured (33 + 4 = 37 tools)
        assert_eq!(tools.len(), 37);
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
