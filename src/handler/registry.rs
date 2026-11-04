//! The tool table: one entry per tool with its schema, the flag that gates
//! it and its handler. Listing and calling both iterate this table, so a
//! flag-gated tool can never be listed without being callable or vice versa.

use super::McpHandler;
use super::TargetArgs;
use super::cdp_domains;
use super::chrome_instance;
use super::debugging::{self, Paused};
use super::downloads::{ACTION_WAIT, DownloadState};
use super::{NetworkState, settle};
use crate::backend::{BrowserKind, Capability};
#[cfg(test)]
use crate::backend::{Capabilities, capabilities::CapabilityFlags};
use crate::cli::ToolGroup;
use rust_mcp_sdk::schema::{CallToolError, CallToolRequestParams, CallToolResult, Tool};
use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::Mutex;

/// What a tool handler returns.
pub(crate) type ToolFuture<'a> =
    Pin<Box<dyn Future<Output = Result<CallToolResult, CallToolError>> + Send + 'a>>;

/// Server flag a tool needs before it is listed or callable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Gate {
    /// Always available.
    Always,
    /// Needs `--proxy-server`.
    ProxyServer,
    /// Needs `--enable-webmcp`.
    Webmcp,
    /// Needs `--tool-groups` naming the group (or `all`).
    Group(ToolGroup),
}

impl Gate {
    fn is_open(self, handler: &McpHandler) -> bool {
        match self {
            Self::Always => true,
            Self::ProxyServer => handler.proxy.proxy_server.is_some(),
            Self::Webmcp => handler.flags.enable_webmcp,
            Self::Group(group) => handler
                .tool_groups
                .iter()
                .any(|on| *on == group || *on == ToolGroup::All),
        }
    }

    fn disabled_message(self, name: &str) -> String {
        let (flag, what) = match self {
            Self::Always => (String::new(), String::new()),
            Self::ProxyServer => ("--proxy-server".into(), "proxy support".into()),
            Self::Webmcp => ("--enable-webmcp".into(), "WebMCP support".into()),
            Self::Group(group) => (
                format!("--tool-groups {}", group.name()),
                format!("the '{}' tool group", group.name()),
            ),
        };
        format!(
            "Tool '{name}' is disabled. Start browser-control-mcp with {flag} to enable {what}."
        )
    }
}

/// The opt-in groups' gates.
const STORAGE: Gate = Gate::Group(ToolGroup::Storage);
const FILES: Gate = Gate::Group(ToolGroup::Files);
const RAW: Gate = Gate::Group(ToolGroup::Raw);
const PERF: Gate = Gate::Group(ToolGroup::Perf);
const EMULATION: Gate = Gate::Group(ToolGroup::Emulation);
const NETWORK: Gate = Gate::Group(ToolGroup::Network);

/// A tool implementation for one browser.
pub(crate) type ToolHandler = for<'a> fn(CallToolRequestParams, &'a McpHandler) -> ToolFuture<'a>;

/// One row of the tool table.
pub(crate) struct ToolEntry {
    /// The tool's schema (name, description, input schema).
    pub(crate) tool: fn() -> Tool,
    /// The flag it needs.
    pub(crate) gate: Gate,
    /// What the target instance must be able to do (checked before dispatch).
    pub(crate) needs: &'static [Capability],
    /// The Chrome (CDP) implementation.
    pub(crate) chrome: ToolHandler,
    /// The Firefox (BiDi/RDP) implementation, once it exists.
    pub(crate) firefox: Option<ToolHandler>,
}

macro_rules! entry {
    ($tool:ty, $gate:expr, $needs:expr) => {
        ToolEntry {
            tool: <$tool>::tool,
            gate: $gate,
            needs: $needs,
            chrome: |params, handler| Box::pin(<$tool>::handle(params, handler)),
            firefox: None,
        }
    };
    ($tool:ty, $gate:expr, $needs:expr, $firefox:path) => {
        ToolEntry {
            firefox: Some(|params, handler| Box::pin($firefox(params, handler))),
            ..entry!($tool, $gate, $needs)
        }
    };
}

/// A Firefox-only tool: on Chrome the capability check refuses it first.
macro_rules! firefox_only {
    ($tool:ty, $gate:expr, $needs:expr, $firefox:path) => {
        ToolEntry {
            tool: <$tool>::tool,
            gate: $gate,
            needs: $needs,
            chrome: |params, _| {
                Box::pin(async move {
                    Err(CallToolError::from_message(format!(
                        "`{}` is Firefox only",
                        params.name
                    )))
                })
            },
            firefox: Some(|params, handler| Box::pin($firefox(params, handler))),
        }
    };
}

/// Tools that act through input events. A page paused in the debugger
/// handles those only once it resumes, so these refuse to run then rather
/// than report a click or a text that has not happened.
const INPUT_TOOLS: &[&str] = &[
    "click_element",
    "hover",
    "drag",
    "fill_input",
    "fill_form",
    "press_key",
];

/// Tools renamed from chrome-debug-mcp, with their new names.
const RENAMED: &[(&str, &str)] = &[
    ("restart_chrome", "restart_browser"),
    ("stop_chrome", "stop_browser"),
];

/// Every tool, in listing order.
pub(crate) fn table() -> Vec<ToolEntry> {
    let mut tools = page_tools();
    tools.extend(debugger_tools());
    tools.extend(instance_tools());
    tools.extend(observation_tools());
    tools.extend(console_tools());
    tools.extend(firefox_tools());
    tools.extend(network_tools());
    tools.extend(gated_tools());
    tools
}

fn network_tools() -> Vec<ToolEntry> {
    use crate::backend::bidi::routes as ff;
    use cdp_domains::fetch::mock_route::{ListRoutesTool, MockRouteTool, RemoveRouteTool};
    use cdp_domains::network::set_offline::SetOfflineTool;
    const NEEDS: &[Capability] = &[Capability::NetworkControl];
    vec![
        entry!(MockRouteTool, NETWORK, NEEDS, ff::mock_route),
        entry!(ListRoutesTool, NETWORK, NEEDS, ff::list_routes),
        entry!(RemoveRouteTool, NETWORK, NEEDS, ff::remove_route),
        entry!(SetOfflineTool, NETWORK, NEEDS, ff::set_offline),
    ]
}

fn page_tools() -> Vec<ToolEntry> {
    use crate::backend::bidi::tools as ff;
    use Capability::*;
    use Gate::Always;
    use cdp_domains::input::drag::DragTool as Drag;
    use cdp_domains::input::fill_form::FillFormTool as Form;
    use cdp_domains::input::upload_file::UploadFileTool as Upload;
    use cdp_domains::input::{
        click_element::ClickElementTool as Click, fill_input::FillInputTool as Fill,
    };
    use cdp_domains::input::{hover::HoverTool as Hover, press_key::PressKeyTool as Key};
    use cdp_domains::page::emulate::EmulateTool as Emulate;
    use cdp_domains::page::handle_dialog::HandleDialogTool as Dialog;
    use cdp_domains::page::save_pdf::SavePdfTool as Pdf;
    use cdp_domains::page::wait_for::WaitForTool as Wait;
    use cdp_domains::page::{
        capture_screenshot::CaptureScreenshotTool as Shot, navigate::NavigateTool as Nav,
    };
    use cdp_domains::page::{reload::ReloadTool as Reload, scroll::ScrollTool as Scroll};
    use cdp_domains::runtime::{
        evaluate_js::EvaluateJsTool as Eval, inspect_dom::InspectDomTool as Dom_,
    };
    use cdp_domains::runtime::{
        find_in_snapshot::FindInSnapshotTool as Find, snapshot::SnapshotTool as Snap,
    };
    vec![
        entry!(Shot, Always, &[Screenshot], ff::capture_screenshot),
        entry!(Pdf, FILES, &[Print], ff::save_pdf),
        entry!(Click, Always, &[Input], ff::click_element),
        entry!(Fill, Always, &[Input], ff::fill_input),
        entry!(Form, Always, &[Input], ff::fill_form),
        entry!(Upload, FILES, &[Input], ff::upload_file),
        entry!(Key, Always, &[Input], ff::press_key),
        entry!(Hover, Always, &[Input], ff::hover),
        entry!(Drag, Always, &[Input], ff::drag),
        entry!(Eval, Always, &[Evaluate], ff::evaluate_js),
        entry!(Nav, Always, &[Navigation], ff::navigate),
        entry!(Dom_, Always, &[Dom], ff::inspect_dom),
        entry!(Snap, Always, &[Dom], ff::snapshot),
        entry!(Find, Always, &[Dom], ff::find_in_snapshot),
        entry!(Reload, Always, &[Navigation], ff::reload),
        entry!(Scroll, Always, &[Dom], ff::scroll),
        entry!(Wait, Always, &[Dom], ff::wait_for),
        entry!(Emulate, EMULATION, &[Dom], ff::emulate),
        entry!(Dialog, Always, &[Input], ff::handle_dialog),
    ]
}

fn debugger_tools() -> Vec<ToolEntry> {
    use crate::backend::bidi::debugger as ff;
    use Capability::{JsDebugger, PauseOnLoad};
    use Gate::Always;
    use cdp_domains::debugger::evaluate_on_call_frame::EvaluateOnCallFrameTool as Frame;
    use cdp_domains::debugger::pause_on_load::PauseOnLoadTool as PauseLoad;
    use cdp_domains::debugger::remove_breakpoint::RemoveBreakpointTool as Remove;
    use cdp_domains::debugger::{call_stack as stack, step_over as steps};
    use cdp_domains::debugger::{resume::ResumeTool as Resume, step_over::StepOverTool as Step};
    use cdp_domains::debugger::{
        search_scripts::SearchScriptsTool as Search, set_breakpoint::SetBreakpointTool as Set,
    };
    vec![
        entry!(PauseLoad, Always, &[JsDebugger, PauseOnLoad]),
        entry!(Step, Always, &[JsDebugger], ff::step_over),
        entry!(Resume, Always, &[JsDebugger], ff::resume),
        entry!(Search, Always, &[JsDebugger], ff::search_scripts),
        entry!(Set, Always, &[JsDebugger], ff::set_breakpoint),
        entry!(Frame, Always, &[JsDebugger], ff::evaluate_on_call_frame),
        entry!(Remove, Always, &[JsDebugger], ff::remove_breakpoint),
        entry!(steps::StepIntoTool, Always, &[JsDebugger], ff::step_into),
        entry!(steps::StepOutTool, Always, &[JsDebugger], ff::step_out),
        entry!(stack::CallStackTool, Always, &[JsDebugger], ff::call_stack),
        entry!(
            stack::PauseOnExceptionsTool,
            Always,
            &[JsDebugger],
            ff::pause_on_exceptions
        ),
    ]
}

fn instance_tools() -> Vec<ToolEntry> {
    use crate::backend::bidi::tools as ff;
    use Capability::Tabs;
    use Gate::Always;
    use chrome_instance::{close_instance, close_tab, list_instances, list_tabs};
    use chrome_instance::{open_instance, open_tab, restart_chrome, stop_chrome, switch_tab};
    vec![
        entry!(
            restart_chrome::RestartChromeTool,
            Always,
            &[],
            ff::restart_browser
        ),
        entry!(stop_chrome::StopChromeTool, Always, &[], ff::stop_browser),
        entry!(
            open_instance::OpenInstanceTool,
            Always,
            &[],
            ff::open_instance
        ),
        entry!(
            list_instances::ListInstancesTool,
            Always,
            &[],
            list_instances::ListInstancesTool::handle
        ),
        entry!(
            close_instance::CloseInstanceTool,
            Always,
            &[],
            ff::close_instance
        ),
        entry!(open_tab::OpenTabTool, Always, &[Tabs], ff::open_tab),
        entry!(list_tabs::ListTabsTool, Always, &[Tabs], ff::list_tabs),
        entry!(close_tab::CloseTabTool, Always, &[Tabs], ff::close_tab),
        entry!(switch_tab::SwitchTabTool, Always, &[Tabs], ff::switch_tab),
        entry!(
            crate::handler::downloads::ListDownloadsTool,
            Always,
            &[Tabs],
            crate::handler::downloads::list_firefox
        ),
    ]
}

/// The console and what the browser diagnosed (`get_issues`, ISSUE-60).
fn console_tools() -> Vec<ToolEntry> {
    use crate::backend::bidi::tools as ff;
    use Capability::ConsoleLogs;
    use Gate::Always;
    use cdp_domains::log::get_console_logs::GetConsoleLogsTool as Console;
    vec![
        entry!(Console, Always, &[ConsoleLogs], ff::console_logs),
        entry!(
            crate::handler::issues::GetIssuesTool,
            Always,
            &[ConsoleLogs],
            ff::issues
        ),
    ]
}

fn observation_tools() -> Vec<ToolEntry> {
    use crate::backend::bidi::storage as store;
    use crate::backend::bidi::tools as ff;
    use Capability::*;
    use Gate::Always;
    use cdp_domains::performance::get_performance_metrics as metrics;
    use cdp_domains::storage::{cookies::CookiesTool, web_storage::WebStorageTool};
    use cdp_domains::tracing::profile_page_performance as profile;
    use cdp_domains::{custom, network};
    vec![
        entry!(
            network::get_network_logs::GetNetworkLogsTool,
            Always,
            &[NetworkLogs],
            ff::network_logs
        ),
        entry!(CookiesTool, STORAGE, &[Evaluate], store::cookies),
        entry!(WebStorageTool, STORAGE, &[Evaluate], store::web_storage),
        entry!(
            metrics::GetPerformanceMetricsTool,
            PERF,
            &[PerformanceMetrics],
            crate::backend::bidi::performance::metrics
        ),
        entry!(
            profile::ProfilePagePerformanceTool,
            PERF,
            &[Tracing],
            crate::backend::bidi::performance::profile
        ),
        entry!(
            crate::handler::audit::AuditPageTool,
            PERF,
            &[Evaluate, ConsoleLogs],
            ff::audit_page
        ),
        entry!(custom::send_cdp_command::SendCdpCommandTool, RAW, &[RawCdp]),
        entry!(
            custom::get_custom_events::GetCustomEventsTool,
            RAW,
            &[CustomEvents],
            ff::custom_events
        ),
    ]
}

fn firefox_tools() -> Vec<ToolEntry> {
    use crate::backend::bidi::raw::{self, SendBidiCommandTool, SendRdpRequestTool};
    vec![
        firefox_only!(
            SendBidiCommandTool,
            RAW,
            &[Capability::RawBidi],
            raw::send_bidi_command
        ),
        firefox_only!(
            SendRdpRequestTool,
            RAW,
            &[Capability::RawRdp],
            raw::send_rdp_request
        ),
    ]
}

fn gated_tools() -> Vec<ToolEntry> {
    use cdp_domains::fetch::enable_proxy_auth::EnableProxyAuthTool;
    use cdp_domains::webmcp::{
        CancelWebmcpInvocationTool, GetWebmcpInvocationTool, InvokeWebmcpToolTool,
    };
    use cdp_domains::webmcp::{ListWebmcpInvocationsTool, ListWebmcpToolsTool};
    const WEBMCP: &[Capability] = &[Capability::Webmcp];
    vec![
        entry!(
            EnableProxyAuthTool,
            Gate::ProxyServer,
            &[Capability::ProxyAuth],
            crate::backend::bidi::proxy::enable_proxy_auth
        ),
        entry!(ListWebmcpToolsTool, Gate::Webmcp, WEBMCP),
        entry!(InvokeWebmcpToolTool, Gate::Webmcp, WEBMCP),
        entry!(GetWebmcpInvocationTool, Gate::Webmcp, WEBMCP),
        entry!(ListWebmcpInvocationsTool, Gate::Webmcp, WEBMCP),
        entry!(CancelWebmcpInvocationTool, Gate::Webmcp, WEBMCP),
    ]
}

/// The schemas of the tools `handler`'s flags enable, in table order.
pub(crate) fn listed_tools(handler: &McpHandler) -> Vec<Tool> {
    table()
        .into_iter()
        .filter(|entry| entry.gate.is_open(handler))
        .map(|entry| (entry.tool)())
        .collect()
}

/// Runs the tool named in `params` with `--secrets` applied: a
/// `<secret:NAME>` the typing tools get becomes its value, and the secrets'
/// values are masked in the outcome.
pub(crate) async fn call(
    handler: &McpHandler,
    params: CallToolRequestParams,
) -> Result<CallToolResult, CallToolError> {
    let params = handler.secrets.reveal(params)?;
    let outcome = dispatch(handler, params).await;
    handler.secrets.mask_outcome(outcome)
}

/// Runs the tool named in `params`: flag gate, then the target instance's
/// capabilities (so a refused call sends nothing), then the implementation of
/// that instance's browser.
async fn dispatch(
    handler: &McpHandler,
    params: CallToolRequestParams,
) -> Result<CallToolResult, CallToolError> {
    if handler.registry.is_closing() {
        return Err(CallToolError::from_message(
            "The server is shutting down; no tool can run",
        ));
    }
    let entry = table()
        .into_iter()
        .find(|entry| (entry.tool)().name == params.name);
    let Some(entry) = entry else {
        return Err(unknown_tool(params.name));
    };
    if !entry.gate.is_open(handler) {
        return Err(CallToolError::from_message(
            entry.gate.disabled_message(&params.name),
        ));
    }
    check_arguments(&(entry.tool)(), &params)?;
    let browser = target_browser(handler, &params);
    let needs = argument_needs(&params).chain(entry.needs.iter().copied());
    handler
        .capabilities(browser)
        .require(&params.name, &needs.collect::<Vec<_>>())?;
    if !INPUT_TOOLS.contains(&params.name.as_str()) {
        return run(&entry, browser, params, handler).await;
    }
    if let Some(paused) = paused_target(handler, browser, &params).await {
        return Err(CallToolError::from_message(debugging::paused_input(
            &params.name,
            &paused,
        )));
    }
    let before = network_before(handler, browser, target_args(&params)).await;
    let result = run(&entry, browser, params, handler).await?;
    Ok(match before {
        Some(before) => settled(handler, before, result).await,
        None => result,
    })
}

/// Runs `entry`'s implementation for `browser`.
async fn run(
    entry: &ToolEntry,
    browser: BrowserKind,
    params: CallToolRequestParams,
    handler: &McpHandler,
) -> Result<CallToolResult, CallToolError> {
    match (browser, entry.firefox) {
        (BrowserKind::Chrome, _) => (entry.chrome)(params, handler).await,
        (BrowserKind::Firefox, Some(firefox)) => firefox(params, handler).await,
        (BrowserKind::Firefox, None) => Err(CallToolError::from_message(format!(
            "`{}` is not implemented for Firefox yet (see the README's TODO list)",
            params.name
        ))),
    }
}

/// The `instance_id` / `tab_id` a call addresses.
fn target_args(params: &CallToolRequestParams) -> TargetArgs {
    let arguments = params.arguments.clone().unwrap_or_default();
    serde_json::from_value(arguments.into()).unwrap_or_default()
}

/// The tab an input action runs on, and its request log before it with the
/// requests it already held (see `settle`).
struct NetworkBefore {
    browser: BrowserKind,
    ids: TargetArgs,
    network: Arc<Mutex<NetworkState>>,
    known: HashSet<String>,
    /// The ids of the instance's tabs, to tell the tabs the action opened.
    tabs: HashSet<String>,
    /// The instance's downloads and how many had begun, to tell the ones
    /// the action started (#77).
    downloads: (Arc<Mutex<DownloadState>>, u64),
    /// The tab's dialogs and log, with how many file choosers and
    /// same-document navigations they had seen (ISSUE-81).
    page: PageBefore,
}

/// What the tab's page had done before an input action (ISSUE-81).
struct PageBefore {
    dialog: Arc<Mutex<super::dialogs::DialogState>>,
    file_choosers: u64,
    log: Arc<Mutex<cdp_domains::log::LogState>>,
    same_document: u64,
}

impl PageBefore {
    async fn of(
        dialog: Arc<Mutex<super::dialogs::DialogState>>,
        log: Arc<Mutex<cdp_domains::log::LogState>>,
    ) -> Self {
        let file_choosers = dialog.lock().await.file_choosers;
        let same_document = log.lock().await.same_document;
        Self {
            dialog,
            file_choosers,
            log,
            same_document,
        }
    }

    /// The notes for what the page did since: a file chooser it opened, the
    /// URL it moved to without loading a page.
    async fn notes(&self) -> Vec<String> {
        let (opened, multiple) = {
            let dialog = self.dialog.lock().await;
            (
                dialog.file_choosers.saturating_sub(self.file_choosers),
                dialog.chooser_multiple,
            )
        };
        let (moved, url) = {
            let log = self.log.lock().await;
            (
                log.same_document.saturating_sub(self.same_document),
                log.same_document_url.clone(),
            )
        };
        [
            chooser_note(opened, multiple),
            moved_note(moved, url.as_deref()),
        ]
        .into_iter()
        .flatten()
        .collect()
    }
}

/// The sentence for the file choosers an action opened (`opened`).
fn chooser_note(opened: u64, multiple: bool) -> Option<String> {
    let several = if multiple {
        " (several files allowed)"
    } else {
        ""
    };
    (opened > 0).then(|| {
        format!("The action opened a file chooser{several}: set its files with upload_file on the file input.")
    })
}

/// The sentence for the same-document navigations an action made (`moved`).
fn moved_note(moved: u64, url: Option<&str>) -> Option<String> {
    let url = url.filter(|_| moved > 0)?;
    Some(format!(
        "The URL changed to {url} without loading a new page (history API or #fragment)."
    ))
}

/// The tab's request log, the instance's downloads and the tab's page
/// state, for [`network_before`].
type TabStates = (
    Arc<Mutex<NetworkState>>,
    Arc<Mutex<DownloadState>>,
    PageBefore,
);

/// The states of the tab `ids` address on `browser`.
async fn tab_states(
    handler: &McpHandler,
    browser: BrowserKind,
    ids: &TargetArgs,
) -> Option<TabStates> {
    Some(match browser {
        BrowserKind::Chrome => {
            let session = handler.session(ids.instance_id.clone()).await.ok()?;
            let network = session.network_state(ids.tab_id.clone()).ok()?;
            let dialog = session.dialog_state(ids.tab_id.clone()).ok()?;
            let log = session.log_state(ids.tab_id.clone()).ok()?;
            (
                network,
                session.downloads.clone(),
                PageBefore::of(dialog, log).await,
            )
        }
        BrowserKind::Firefox => {
            let session = handler.firefox(ids.instance_id.as_deref()).await.ok()?;
            let tab = session.tab(ids.tab_id.as_deref()).await.ok()?.1;
            let page = PageBefore::of(tab.dialog.clone(), tab.log.clone()).await;
            (tab.network, session.downloads.clone(), page)
        }
    })
}

/// The request log of the tab `ids` address, read before an input action;
/// `None` when the tab cannot be resolved (the tool reports why).
async fn network_before(
    handler: &McpHandler,
    browser: BrowserKind,
    ids: TargetArgs,
) -> Option<NetworkBefore> {
    let (network, downloads, page) = tab_states(handler, browser, &ids).await?;
    let known = network.lock().await.known_ids();
    let begun = downloads.lock().await.begun();
    let tabs = tabs_now(handler, browser, &ids)
        .await
        .into_iter()
        .map(|tab| tab.id)
        .collect();
    Some(NetworkBefore {
        browser,
        ids,
        network,
        known,
        tabs,
        downloads: (downloads, begun),
        page,
    })
}

/// A tab of an instance, as its registry knows it.
struct KnownTab {
    id: String,
    /// The Chrome target, to ask the browser for its current URL.
    target: Option<String>,
    /// For Chrome the URL the tab opened with (often blank); Firefox
    /// follows navigations.
    url: String,
}

/// The tabs of the instance `ids` addresses. The browser registers the
/// tabs a page opens (`target=_blank`, `window.open`) as they appear.
async fn tabs_now(handler: &McpHandler, browser: BrowserKind, ids: &TargetArgs) -> Vec<KnownTab> {
    match browser {
        BrowserKind::Chrome => {
            let Ok(session) = handler.session(ids.instance_id.clone()).await else {
                return Vec::new();
            };
            let registry = crate::util::sync::read(&session.tabs);
            let tabs = registry.tabs.iter().map(|(id, entry)| KnownTab {
                id: id.clone(),
                target: Some(entry.tab.target_id().to_string()),
                url: entry.url.clone(),
            });
            tabs.collect()
        }
        BrowserKind::Firefox => {
            let Ok(session) = handler.firefox(ids.instance_id.as_deref()).await else {
                return Vec::new();
            };
            let registry = crate::util::sync::read(&session.tabs);
            let tabs = registry.tabs.iter().map(|(id, tab)| KnownTab {
                id: id.clone(),
                target: None,
                url: tab.url.clone(),
            });
            tabs.collect()
        }
    }
}

/// Waits for the pages Chrome announced to the instance `ids` addresses
/// to be registered (see [`REGISTRATION_WAIT`]).
async fn chrome_registrations(handler: &McpHandler, ids: &TargetArgs) {
    let Ok(session) = handler.session(ids.instance_id.clone()).await else {
        return;
    };
    let tabs = &session.tabs;
    crate::handler::chrome_instance::tab_lifecycle::registrations_done(tabs, REGISTRATION_WAIT)
        .await;
}

/// How long an action waits for Chrome's announced pages to be registered
/// before naming the tabs it opened.
const REGISTRATION_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

/// The tabs the action opened (those not in `before`): `(id, URL)`, the
/// URL of a Chrome tab asked of the browser (one round trip, only when a
/// tab opened).
async fn tabs_opened(
    handler: &McpHandler,
    browser: BrowserKind,
    ids: &TargetArgs,
    before: &HashSet<String>,
) -> Vec<(String, String)> {
    if browser == BrowserKind::Chrome {
        chrome_registrations(handler, ids).await;
    }
    let opened = opened_tabs(before, tabs_now(handler, browser, ids).await);
    let current = match opened.iter().any(|tab| tab.target.is_some()) {
        true => chrome_targets(handler, ids).await,
        false => Vec::new(),
    };
    opened
        .into_iter()
        .map(|tab| {
            let now = current
                .iter()
                .find(|t| Some(&t.target_id) == tab.target.as_ref());
            (tab.id, now.map_or(tab.url, |t| t.url.clone()))
        })
        .collect()
}

/// The targets of the Chrome instance `ids` addresses (their current URLs).
async fn chrome_targets(
    handler: &McpHandler,
    ids: &TargetArgs,
) -> Vec<cdp_browser_lite::TargetInfo> {
    let Ok(session) = handler.session(ids.instance_id.clone()).await else {
        return Vec::new();
    };
    match session.browser_client().await.ok() {
        Some(client) => client.list_tabs().await.unwrap_or_default(),
        None => Vec::new(),
    }
}

/// The tabs of `now` that were not in `before`.
fn opened_tabs(before: &HashSet<String>, now: Vec<KnownTab>) -> Vec<KnownTab> {
    now.into_iter()
        .filter(|tab| !before.contains(&tab.id))
        .collect()
}

/// Brings the active tab of the instance `ids` address back to the front
/// (ISSUE-68), after the browser showed a tab an action opened.
async fn front_active(handler: &McpHandler, browser: BrowserKind, ids: &TargetArgs) {
    match browser {
        BrowserKind::Chrome => {
            let Ok(session) = handler.session(ids.instance_id.clone()).await else {
                return;
            };
            crate::handler::chrome_instance::tab_lifecycle::front_active(&session.tabs).await;
        }
        BrowserKind::Firefox => {
            let Ok(session) = handler.firefox(ids.instance_id.as_deref()).await else {
                return;
            };
            crate::backend::bidi::session::front_active(&session.tabs).await;
        }
    }
}

/// The sentence a reply adds for the tabs an action opened: nothing told
/// the agent to look at them.
fn opened_note(opened: &[(String, String)]) -> Option<String> {
    let named: Vec<String> = opened
        .iter()
        .map(|(id, url)| match url.as_str() {
            "" | "about:blank" => id.clone(),
            url => format!("{id} ({url})"),
        })
        .collect();
    match named.as_slice() {
        [] => None,
        [one] => Some(format!(
            "The action opened tab {one}: call switch_tab with its tab_id to act in it."
        )),
        many => Some(format!(
            "The action opened tabs {}: call switch_tab with a tab_id to act in one.",
            many.join(", ")
        )),
    }
}

/// The page of the tab `ids` address, unless the action left it blocked
/// (a dialog open, or paused in the debugger): it answers nothing then.
async fn page_after(
    handler: &McpHandler,
    browser: BrowserKind,
    ids: &TargetArgs,
) -> Option<settle::Page> {
    match browser {
        BrowserKind::Chrome => {
            let target =
                super::resolve_target(handler, ids.instance_id.clone(), ids.tab_id.clone());
            let target = target.await.ok()?;
            target
                .paused()
                .await
                .is_none()
                .then_some(settle::Page::Chrome(target))
        }
        BrowserKind::Firefox => {
            let (session, id, tab) = crate::backend::bidi::tools::target(handler, ids)
                .await
                .ok()?;
            let paused = crate::backend::bidi::debugger::paused(&session, Some(&id)).await;
            paused.is_none().then_some(settle::Page::Firefox(tab.tab))
        }
    }
}

/// `result` of an input action once the page settled, with a line saying
/// how it reacted, when it did.
async fn settled(
    handler: &McpHandler,
    before: NetworkBefore,
    mut result: CallToolResult,
) -> CallToolResult {
    let Some(page) = page_after(handler, before.browser, &before.ids).await else {
        return result;
    };
    // Bounded even if the page stops answering mid-way (a navigation).
    let limit = settle::MAX + std::time::Duration::from_secs(1);
    let watched = settle::settle(&page, &before.network, &before.known);
    let watched = tokio::time::timeout(limit, watched);
    let (reacted, interruption) = match unless_blocked(handler, &before, &page, watched).await {
        Ok(reaction) => (reaction.ok().flatten().and_then(|r| r.note()), None),
        Err(interruption) => (None, Some(interruption)),
    };
    let opened = tabs_opened(handler, before.browser, &before.ids, &before.tabs).await;
    if !opened.is_empty() {
        // The browser shows the new tab; the agent stays on its tab.
        front_active(handler, before.browser, &before.ids).await;
    }
    let opened = opened_note(&opened);
    let (downloads, begun) = &before.downloads;
    let started = crate::handler::downloads::wait_ended(downloads, *begun, ACTION_WAIT).await;
    let downloaded = crate::handler::downloads::action_note(&started);
    let page_notes = before.page.notes().await;
    let first_text = result.content.iter_mut().find_map(|block| match block {
        rust_mcp_sdk::schema::ContentBlock::TextContent(text) => Some(text),
        _ => None,
    });
    if let Some(text) = first_text {
        if let Some(interruption) = interruption {
            text.text = interruption.appended_to(&text.text);
        }
        let notes = [reacted, opened, downloaded].into_iter().flatten();
        for note in notes.chain(page_notes) {
            text.text.push('\n');
            text.text.push_str(&note);
        }
    }
    result
}

/// Runs `watch` (the page settling after an action) unless the page pauses
/// in the debugger or opens a dialog meanwhile, as a handler does after an
/// async step (an `await`, a timer): the blocked page would keep the watch
/// waiting for its limit (ISSUE-65).
async fn unless_blocked<T>(
    handler: &McpHandler,
    before: &NetworkBefore,
    page: &settle::Page,
    watch: impl std::future::Future<Output = T>,
) -> Result<T, crate::handler::interruption::Interruption> {
    match page {
        settle::Page::Chrome(target) => target.race_interruptions("the watch", watch).await,
        settle::Page::Firefox(_) => {
            let target = crate::backend::bidi::tools::target(handler, &before.ids);
            let Some((session, id, tab)) = target.await.ok() else {
                return Ok(watch.await);
            };
            crate::backend::bidi::debugger::race_blockers(&session, &id, &tab.dialog, watch).await
        }
    }
}

/// Where the tab a call addresses is stopped in the debugger, if it is. A
/// target that cannot be resolved is left to the tool, which reports why.
async fn paused_target(
    handler: &McpHandler,
    browser: BrowserKind,
    params: &CallToolRequestParams,
) -> Option<Paused> {
    let ids = target_args(params);
    match browser {
        BrowserKind::Chrome => {
            let target = super::resolve_target(handler, ids.instance_id, ids.tab_id);
            target.await.ok()?.paused().await
        }
        BrowserKind::Firefox => {
            let session = handler.firefox(ids.instance_id.as_deref()).await.ok()?;
            crate::backend::bidi::debugger::paused(&session, ids.tab_id.as_deref()).await
        }
    }
}

fn unknown_tool(name: String) -> CallToolError {
    match RENAMED.iter().find(|(old, _)| *old == name) {
        Some((old, new)) => CallToolError::from_message(format!(
            "Tool '{old}' was renamed to '{new}' in browser-control-mcp."
        )),
        None => CallToolError::unknown_tool(name),
    }
}

/// Refuses an argument the tool's schema does not have: serde ignores
/// unknown fields, so a typo (`tabId`) silently fell back to a default (the
/// active tab) and the call ran on the wrong target.
fn check_arguments(tool: &Tool, params: &CallToolRequestParams) -> Result<(), CallToolError> {
    let known: Vec<&str> = tool
        .input_schema
        .properties
        .iter()
        .flat_map(|properties| properties.keys().map(String::as_str))
        .collect();
    let unknown = params
        .arguments
        .iter()
        .flat_map(|arguments| arguments.keys())
        .find(|key| !known.contains(&key.as_str()));
    let Some(unknown) = unknown else {
        return Ok(());
    };
    let squashed = |name: &str| name.replace('_', "").to_ascii_lowercase();
    let hint = match known.iter().find(|k| squashed(k) == squashed(unknown)) {
        Some(meant) => format!("did you mean `{meant}`?"),
        None if known.is_empty() => "it takes no arguments".to_string(),
        None => format!("Valid arguments: {}", known.join(", ")),
    };
    Err(CallToolError::from_message(format!(
        "Unknown argument `{unknown}` for `{}`; {hint}",
        params.name
    )))
}

fn argument(params: &CallToolRequestParams, key: &str) -> Option<serde_json::Value> {
    params.arguments.as_ref().and_then(|a| a.get(key)).cloned()
}

/// The browser the call addresses: `open_instance`'s `browser` argument (or
/// the default browser), else the instance named by `instance_id`.
fn target_browser(handler: &McpHandler, params: &CallToolRequestParams) -> BrowserKind {
    if params.name == "open_instance" {
        return argument(params, "browser")
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or(handler.default_browser);
    }
    let id = argument(params, "instance_id");
    handler.browser_of(id.as_ref().and_then(|v| v.as_str()))
}

/// Capabilities some optional arguments need on top of the tool's own.
fn argument_needs(params: &CallToolRequestParams) -> impl Iterator<Item = Capability> {
    let cookies = argument(params, "copy_cookies") == Some(serde_json::Value::Bool(true));
    let features = argument(params, "features")
        .and_then(|v| v.as_array().map(|a| !a.is_empty()))
        .unwrap_or(false);
    let ws_only = argument(params, "type_filter")
        .and_then(|v| v.as_str().map(|t| t.eq_ignore_ascii_case("websocket")))
        .unwrap_or(false);
    [
        cookies.then_some(Capability::CookieImport),
        features.then_some(Capability::LaunchFeatures),
        ws_only.then_some(Capability::WebsocketFrames),
    ]
    .into_iter()
    .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn names(tools: Vec<Tool>) -> BTreeSet<String> {
        tools.into_iter().map(|t| t.name).collect()
    }

    /// ISSUE-14: with --secrets, a value in a tool's result or error comes
    /// back masked, and never reaches the logs.
    #[tokio::test]
    async fn secrets_are_masked_in_outcomes() {
        use crate::handler::cdp_domains::tests::{mock_handler, reply_text, spawn_recording_mock};
        // Given a page whose evaluation answers a secret, then throws it
        let (logs, _guard) = crate::util::log_capture::capture();
        let mock = spawn_recording_mock().await;
        mock.respond_with("Runtime.evaluate", |_| {
            serde_json::json!({"result": {"result": {"type": "string", "value": "pw=hunter22"}}})
        });
        let mut handler = mock_handler(mock.http_port);
        handler.secrets = crate::secrets::Secrets::parse("PASS=hunter22").unwrap();
        // When evaluate_js reads it
        let args = serde_json::json!({"expression": "document.cookie"});
        let reply = call(&handler, params("evaluate_js", args.clone()))
            .await
            .unwrap();
        // Then the reply has the name, not the value
        let text = reply_text(&reply);
        assert!(
            text.contains("pw=<secret:PASS>") && !text.contains("hunter22"),
            "{text}"
        );
        // And an error carrying it is masked too
        mock.respond_with("Runtime.evaluate", |_| {
            serde_json::json!({"result": {"result": {"type": "object"}, "exceptionDetails": {
                "text": "Uncaught", "lineNumber": 0, "columnNumber": 0,
                "exception": {"type": "string", "value": "bad hunter22"}}}})
        });
        let error = call(&handler, params("evaluate_js", args))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("bad <secret:PASS>"), "{error}");
        // And the value never reached the logs
        assert!(!logs.text().contains("hunter22"));
    }

    /// A page's answer to an evaluation: the settle snippet sees
    /// `mutations`; the click snippet finds the element at (10, 20).
    fn page_reacting(params: &serde_json::Value, mutations: u64) -> serde_json::Value {
        let expression = params["expression"].as_str().unwrap_or_default();
        let value = if expression.contains("MutationObserver") {
            serde_json::json!({"mutations": mutations, "settled": true})
        } else {
            serde_json::json!({"status": "ok", "x": 10, "y": 20})
        };
        serde_json::json!({"result": {"result": {"type": "object", "value": value}}})
    }

    /// ISSUE-41: an input action that opened tabs says so, naming each
    /// with its URL (none while it is still blank) and how to act in it.
    #[test]
    fn opened_tabs_are_noted() {
        let before: HashSet<String> = ["tab-1".to_string()].into();
        let tab = |id: &str, url: &str| KnownTab {
            id: id.into(),
            target: None,
            url: url.into(),
        };
        let named = |tabs: Vec<KnownTab>| -> Vec<(String, String)> {
            opened_tabs(&before, tabs)
                .into_iter()
                .map(|t| (t.id, t.url))
                .collect()
        };
        let opened = named(vec![
            tab("tab-1", "http://a/"),
            tab("tab-2", "http://a/b.html"),
        ]);
        assert_eq!(
            opened_note(&opened).as_deref(),
            Some(
                "The action opened tab tab-2 (http://a/b.html): call switch_tab with its tab_id to act in it."
            )
        );
        let two = named(vec![tab("tab-3", "about:blank"), tab("tab-4", "")]);
        assert_eq!(
            opened_note(&two).as_deref(),
            Some(
                "The action opened tabs tab-3, tab-4: call switch_tab with a tab_id to act in one."
            )
        );
        assert_eq!(opened_note(&named(Vec::new())), None);
    }

    /// ISSUE-81: an action that opened a file chooser, or changed the URL
    /// without loading a page, says so.
    #[test]
    fn page_event_notes() {
        assert_eq!(
            chooser_note(1, true).as_deref(),
            Some(
                "The action opened a file chooser (several files allowed): set its files with upload_file on the file input."
            )
        );
        assert_eq!(
            chooser_note(1, false).as_deref(),
            Some(
                "The action opened a file chooser: set its files with upload_file on the file input."
            )
        );
        assert_eq!(chooser_note(0, true), None);
        assert_eq!(
            moved_note(1, Some("http://a/pushed")).as_deref(),
            Some(
                "The URL changed to http://a/pushed without loading a new page (history API or #fragment)."
            )
        );
        assert_eq!(moved_note(0, Some("http://a/")), None);
    }

    /// ISSUE-77 (Chrome): a click that downloads a file says where the
    /// file went (it only said the page reacted).
    #[tokio::test]
    async fn input_actions_name_their_downloads() {
        use crate::handler::cdp_domains::tests::{mock_handler, reply_text, spawn_recording_mock};
        use crate::handler::downloads::{Download, DownloadStatus};
        // Given a link whose click downloads data.csv while the page settles
        let mock = spawn_recording_mock().await;
        let handler = mock_handler(mock.http_port);
        let session = handler.session(None).await.unwrap();
        let downloads = session.downloads.clone();
        mock.respond_with("Runtime.evaluate", move |params| {
            let watching = params["expression"]
                .to_string()
                .contains("MutationObserver");
            if let (true, Ok(mut state)) = (watching, downloads.try_lock()) {
                let mut download = Download::begun("g1", "http://a/download", "data.csv", None);
                download.state = DownloadStatus::Completed;
                download.path = Some(std::path::PathBuf::from("/dl/data.csv"));
                download.received_bytes = Some(8);
                state.begin(download);
            }
            page_reacting(params, 0)
        });
        // When it is clicked
        let args = serde_json::json!({"selector": "#dl"});
        let reply = call(&handler, params("click_element", args)).await.unwrap();
        // Then the reply names the file
        let text = reply_text(&reply);
        let path = std::path::Path::new("/dl/data.csv").display();
        assert!(
            text.ends_with(&format!(
                "\nThe action downloaded data.csv (8 bytes) to {path}."
            )),
            "{text}"
        );
    }

    /// ISSUE-21: after an input action the page is watched until it
    /// settles (awaiting the snippet), and the reply says how it reacted;
    /// an action nothing reacts to keeps its reply.
    #[tokio::test]
    async fn input_actions_wait_for_the_page_to_settle() {
        use crate::handler::cdp_domains::tests::{mock_handler, reply_text, spawn_recording_mock};
        for mutations in [3, 0] {
            // Given a page whose DOM changes (or not) after the click
            let mock = spawn_recording_mock().await;
            mock.respond_with("Runtime.evaluate", move |params| {
                page_reacting(params, mutations)
            });
            let handler = mock_handler(mock.http_port);
            // When the element is clicked
            let args = serde_json::json!({"selector": "#b"});
            let reply = call(&handler, params("click_element", args)).await.unwrap();
            // Then the page was watched, awaiting the snippet
            let watched: Vec<_> = mock
                .commands_named("Runtime.evaluate")
                .into_iter()
                .filter(|c| {
                    c.params["expression"]
                        .to_string()
                        .contains("MutationObserver")
                })
                .collect();
            assert_eq!(watched.len(), 1);
            assert_eq!(watched[0].params["awaitPromise"], true);
            // And the reply says how the page reacted, only when it did
            let text = reply_text(&reply);
            assert!(
                text.starts_with("Successfully clicked on element '#b'"),
                "{text}"
            );
            let noted = text.contains(
                "
The page reacted (the DOM changed) and settled after ",
            );
            assert_eq!(noted, mutations > 0, "{text}");
        }
    }

    /// ISSUE-65 (Chrome): a handler that pauses in the debugger after an
    /// async step (an `await`, a timer), while the page is being watched
    /// to settle, ends the watch at once and the reply notes the pause, as
    /// for a handler that pauses right away.
    #[tokio::test]
    async fn a_pause_while_settling_is_noted_at_once() {
        use crate::handler::cdp_domains::tests::{mock_handler, reply_text, spawn_recording_mock};
        // Given a click that starts a request still loading, whose handler
        // pauses at cart.js:3 once the page is being watched
        let mock = spawn_recording_mock().await;
        let handler = mock_handler(mock.http_port);
        let session = handler.session(None).await.unwrap();
        let network = session.network_state(None).unwrap();
        mock.respond_with("Runtime.evaluate", move |params| {
            let watching = params["expression"].to_string().contains("MutationObserver");
            if let (true, Ok(mut log)) = (watching, network.try_lock()) {
                let request = crate::handler::NetworkRequest {
                    url: "http://a/api".into(),
                    ..Default::default()
                };
                log.insert_request("r65".into(), request);
                let frame = serde_json::json!({"callFrameId": "f1", "url": "http://a/cart.js", "location": {"scriptId": "35", "lineNumber": 3, "columnNumber": 2}});
                let paused = serde_json::json!({"reason": "other", "callFrames": [frame]});
                mock.emit("Debugger.paused", paused, None);
            }
            page_reacting(params, 0)
        });
        // When the element is clicked
        let start = std::time::Instant::now();
        let args = serde_json::json!({"selector": "#b"});
        let reply = call(&handler, params("click_element", args)).await.unwrap();
        // Then the reply notes the pause, well before the 1 s watch ended
        let text = reply_text(&reply);
        assert!(text.contains("The page paused in the debugger"), "{text}");
        assert!(text.contains("\"paused\": true"), "{text}");
        assert!(start.elapsed() < std::time::Duration::from_millis(900));
    }

    /// ISSUE-17: on a page paused in the debugger, the input tools refuse
    /// to run (the page would handle their events only after `resume`),
    /// naming the pause; nothing is sent to the page.
    #[tokio::test]
    async fn input_tools_refuse_to_run_while_paused() {
        use crate::handler::cdp_domains::debugger::process_debugger_event;
        use crate::handler::cdp_domains::tests::{make_event, mock_handler, spawn_recording_mock};
        // Given a Chrome tab paused in http://a/cart.js
        let mock = spawn_recording_mock().await;
        let handler = mock_handler(mock.http_port);
        let session = handler.session(None).await.unwrap();
        let state = session.debugger_state(None).unwrap();
        let parsed = serde_json::json!({"scriptId": "35", "hash": "h", "url": "http://a/cart.js"});
        process_debugger_event(&make_event("Debugger.scriptParsed", parsed), &state).await;
        state.lock().await.breakpoints.add(
            crate::handler::cdp_domains::debugger::frames::ChromeBreakpoint::on_tab(
                "1:3:0:http://a/cart.js",
            ),
        );
        let frame = serde_json::json!({"callFrameId": "f1", "url": "", "location": {"scriptId": "35", "lineNumber": 3, "columnNumber": 2}});
        let paused = serde_json::json!({"reason": "other", "callFrames": [frame]});
        process_debugger_event(&make_event("Debugger.paused", paused), &state).await;
        // When each input tool runs
        let field = serde_json::json!({"selector": "#n", "value": "x"});
        for (name, arguments) in [
            ("click_element", serde_json::json!({"selector": "#b"})),
            ("hover", serde_json::json!({"selector": "#b"})),
            (
                "fill_input",
                serde_json::json!({"selector": "#n", "text": "x"}),
            ),
            ("fill_form", serde_json::json!({"fields": [field]})),
            ("press_key", serde_json::json!({"key": "Enter"})),
        ] {
            let error = call(&handler, params(name, arguments)).await.unwrap_err();
            // Then it fails naming the pause and the tool
            let message = error.to_string();
            assert!(
                message.contains("paused in the debugger at http://a/cart.js line 3")
                    && message.contains(name),
                "{message}"
            );
        }
        // And nothing reached the page (only the session's setup)
        let sent: Vec<String> = mock.commands().into_iter().map(|c| c.method).collect();
        let acts = |m: &String| {
            m.starts_with("Input.") || m.starts_with("DOM.") || m == "Runtime.evaluate"
        };
        assert!(!sent.iter().any(acts), "{sent:?}");
    }

    /// RFC-03: Listing and calling use the same tool table.
    #[tokio::test]
    async fn disabled_tools_are_neither_listed_nor_callable() {
        // Given a handler without --proxy-server and without --enable-webmcp
        let handler = McpHandler::new_test();
        // Then the gated tools are not listed
        let listed = names(listed_tools(&handler));
        assert!(!listed.contains("enable_proxy_auth"));
        assert!(!listed.iter().any(|n| n.starts_with("webmcp_")));
        // And calling one fails naming the missing flag
        let params: CallToolRequestParams = serde_json::from_value(
            serde_json::json!({"name": "enable_proxy_auth", "arguments": {}}),
        )
        .unwrap();
        let message = format!("{:?}", call(&handler, params).await.err().unwrap());
        assert!(message.contains("--proxy-server"), "{message}");
    }

    /// TOOLS-01: opt-in groups are neither listed nor callable until
    /// `--tool-groups` names them (or `all`); the error names the flag.
    #[tokio::test]
    async fn tool_groups_are_opt_in() {
        // Given a handler without tool groups
        let mut handler = McpHandler::new_test();
        let listed = names(listed_tools(&handler));
        // Then no group's tool is listed, and calling one names its group
        for name in [
            "cookies",
            "web_storage",
            "upload_file",
            "send_cdp_command",
            "get_custom_events",
            "get_performance_metrics",
            "profile_page_performance",
            "audit_page",
            "emulate",
            "send_bidi_command",
            "send_rdp_request",
        ] {
            assert!(!listed.contains(name), "{name} listed by default");
        }
        assert!(listed.contains("set_breakpoint") && listed.contains("snapshot"));
        let message = format!(
            "{:?}",
            call(
                &handler,
                params("cookies", serde_json::json!({"action": "get"}))
            )
            .await
            .err()
            .unwrap()
        );
        assert!(message.contains("--tool-groups storage"), "{message}");
        // When one group is enabled, only its tools are added
        handler.tool_groups = vec![ToolGroup::Storage];
        let with_storage = names(listed_tools(&handler));
        assert_eq!(with_storage.len(), listed.len() + 2);
        assert!(with_storage.contains("cookies") && !with_storage.contains("emulate"));
    }

    /// RFC-04: Every listed tool is callable and every callable tool is listed.
    #[test]
    fn listed_and_callable_tools_match() {
        // Given a handler with every flag enabled
        let mut handler = McpHandler::new_test();
        handler.proxy.proxy_server = Some("http://p:1".into());
        handler.flags.enable_webmcp = true;
        handler.tool_groups = vec![crate::cli::ToolGroup::All];
        // Then the listed names equal the table's names, without duplicates
        let listed = listed_tools(&handler);
        let table_names: Vec<String> = table().iter().map(|e| (e.tool)().name).collect();
        assert_eq!(listed.len(), table_names.len());
        assert_eq!(names(listed), table_names.into_iter().collect());
    }

    fn params(name: &str, arguments: serde_json::Value) -> CallToolRequestParams {
        serde_json::from_value(serde_json::json!({"name": name, "arguments": arguments})).unwrap()
    }

    /// A handler with a registered Firefox instance "ff" (no session behind it:
    /// any dispatch to an implementation would fail with "not found").
    fn handler_with_firefox() -> McpHandler {
        let mut handler = McpHandler::new_test();
        handler.tool_groups = vec![ToolGroup::All];
        let mut desc =
            crate::handler::chrome_instance::registry::InstanceDescriptor::default_instance(
                0,
                vec![],
            );
        desc.id = "ff".into();
        desc.is_default = false;
        desc.browser = BrowserKind::Firefox;
        handler.registry.register_descriptor(desc);
        handler
    }

    async fn error_of(handler: &McpHandler, name: &str, arguments: serde_json::Value) -> String {
        let result = call(handler, params(name, arguments)).await;
        format!("{:?}", result.expect_err("the call must fail"))
    }

    /// AUD-RW-12: an argument the tool does not have fails the call before
    /// anything runs, naming the argument it was probably meant to be (a
    /// `tabId` typo used to run on the active tab).
    #[tokio::test]
    async fn unknown_arguments_are_refused() {
        // Given a handler with no browser
        let handler = McpHandler::new_test();
        // When evaluate_js gets `tabId` instead of `tab_id`
        let error = error_of(
            &handler,
            "evaluate_js",
            serde_json::json!({"expression": "1", "tabId": "tab-2"}),
        )
        .await;
        // Then it fails naming the argument and its likely meaning
        assert!(
            error.contains("Unknown argument `tabId` for `evaluate_js`"),
            "{error}"
        );
        assert!(error.contains("did you mean `tab_id`?"), "{error}");
        // And an argument with no close match lists the valid ones
        let error = error_of(&handler, "list_tabs", serde_json::json!({"verbose": true})).await;
        assert!(error.contains("Valid arguments: instance_id"), "{error}");
        let error = error_of(&handler, "list_instances", serde_json::json!({"all": true})).await;
        assert!(error.contains("it takes no arguments"), "{error}");
    }

    /// REV-10: once shutdown started, tool calls are refused (a call in
    /// flight would otherwise launch a browser nothing stops).
    #[tokio::test]
    async fn calls_after_shutdown_are_refused() {
        // Given a handler whose browsers were stopped for shutdown
        let handler = McpHandler::new_test();
        handler
            .shutdown_hook()
            .stop_all(std::time::Duration::from_secs(1))
            .await;
        // When a tool is called
        let message = error_of(&handler, "list_instances", serde_json::json!({})).await;
        // Then it is refused
        assert!(message.contains("shutting down"), "{message}");
    }

    /// TOL-01: The tool list has the 0.1.0 names.
    #[test]
    fn tool_list_has_v0_1_names() {
        let mut handler = McpHandler::new_test();
        handler.proxy.proxy_server = Some("http://p:1".into());
        handler.flags.enable_webmcp = true;
        handler.tool_groups = vec![crate::cli::ToolGroup::All];
        let listed = names(listed_tools(&handler));
        assert_eq!(listed.len(), 62);
        assert!(listed.contains("restart_browser") && listed.contains("stop_browser"));
        assert!(!listed.contains("restart_chrome") && !listed.contains("stop_chrome"));
    }

    /// TOL-02: Old tool names are rejected with a hint.
    #[tokio::test]
    async fn old_tool_names_point_to_new_ones() {
        let handler = McpHandler::new_test();
        let message = error_of(&handler, "restart_chrome", serde_json::json!({})).await;
        assert!(
            message.contains("renamed to 'restart_browser'"),
            "{message}"
        );
        let message = error_of(&handler, "stop_chrome", serde_json::json!({})).await;
        assert!(message.contains("renamed to 'stop_browser'"), "{message}");
    }

    /// CAP-02: Every tool declares the capabilities it needs.
    #[test]
    fn every_tool_declares_its_capabilities() {
        let instance_tools = [
            "open_instance",
            "close_instance",
            "list_instances",
            "restart_browser",
            "stop_browser",
        ];
        for entry in table() {
            let name = (entry.tool)().name;
            assert_eq!(
                entry.needs.is_empty(),
                instance_tools.contains(&name.as_str()),
                "{name}"
            );
        }
    }

    /// BKD-03: open_instance creates an instance of the requested browser.
    #[tokio::test]
    async fn open_instance_creates_requested_browser() {
        // Given a Chrome default and a Firefox binary that does not exist
        let mut handler = McpHandler::new_test();
        handler.firefox_spec.firefox.firefox_path = Some("/nonexistent/firefox".into());
        // When open_instance asks for Firefox
        let open = serde_json::json!({"browser": "firefox", "label": "ff"});
        let message = error_of(&handler, "open_instance", open).await;
        // Then the Firefox implementation ran (it failed to launch that
        // binary) and no instance was left behind
        assert!(message.contains("/nonexistent/firefox"), "{message}");
        assert!(handler.registry.get_firefox("ff").is_none());
        // And with "chrome" the instance is a Chrome one under the label
        let open = serde_json::json!({"browser": "chrome", "label": "cc"});
        call(&handler, params("open_instance", open)).await.unwrap();
        assert_eq!(handler.browser_of(Some("cc")), BrowserKind::Chrome);
    }

    /// CAP-03: A tool the browser cannot serve fails with a capability error.
    #[tokio::test]
    async fn unsupported_tool_fails_without_touching_the_browser() {
        // Given an instance backed by Firefox (BiDi)
        let handler = handler_with_firefox();
        // When the agent calls "pause_on_load" on it
        let message = error_of(
            &handler,
            "pause_on_load",
            serde_json::json!({"instance_id": "ff"}),
        )
        .await;
        // Then the error names the tool and the browser, and no implementation ran
        assert!(
            message.contains("`pause_on_load` is not available on Firefox (BiDi)"),
            "{message}"
        );
    }

    /// RDP-01: Debugger tools need the Firefox debugger port.
    #[tokio::test]
    async fn debugger_tools_need_rdp() {
        // Given a Firefox instance started with --no-firefox-debugger
        let mut handler = handler_with_firefox();
        handler.firefox_spec.firefox.no_firefox_debugger = true;
        // When a debugger tool is called on it
        let args =
            serde_json::json!({"instance_id": "ff", "url": "http://x/a.js", "line_number": 0});
        let message = error_of(&handler, "set_breakpoint", args).await;
        // Then the capability error names the missing debugger port
        assert!(message.contains("debugger port"), "{message}");
    }

    /// CAP-05: Optional arguments that need a capability are checked too.
    #[tokio::test]
    async fn capability_gated_arguments_are_checked() {
        let mut handler = handler_with_firefox();
        handler.flags.allow_cookie_import = true;
        let open = serde_json::json!({"browser": "firefox", "features": ["WEBGL_SOFTWARE"]});
        let message = error_of(&handler, "open_instance", open).await;
        assert!(
            message.contains("`open_instance` is not available on Firefox"),
            "{message}"
        );
        let nav =
            serde_json::json!({"instance_id": "ff", "url": "http://a/", "copy_cookies": true});
        let message = error_of(&handler, "navigate", nav).await;
        assert!(message.contains("cookie import"), "{message}");
    }

    /// CAP-06: Raw tools are bound to their protocol (CDP half; BiDi/RDP in F8).
    #[tokio::test]
    async fn raw_tools_are_bound_to_their_protocol() {
        let handler = handler_with_firefox();
        let raw = serde_json::json!({"instance_id": "ff", "method": "Page.reload"});
        let message = error_of(&handler, "send_cdp_command", raw).await;
        assert!(
            message.contains("not available on Firefox (BiDi)"),
            "{message}"
        );
    }

    /// Argument and result field names that descriptions quote.
    const ARGUMENT_NAMES: &[&str] = &[
        "active_tab_id",
        "click_count",
        "collect_garbage",
        "color_scheme",
        "confirm_restart",
        "copy_cookies",
        "current_navigation_only",
        "device_scale_factor",
        "disable_cache",
        "duration_ms",
        "forced_colors",
        "frame_index",
        "full_page",
        "http_only",
        "save_path",
        "include_details",
        "include_sensitive_headers",
        "instance_id",
        "is_default",
        "prewarm_url",
        "profile_dir",
        "prompt_text",
        "reduced_motion",
        "resource_type",
        "same_site",
        "show_values",
        "source_profile",
        "tab_id",
        "target_id",
        "timeout_ms",
        "type_filter",
        "value_length",
    ];

    fn all_tools() -> Vec<Tool> {
        table().into_iter().map(|e| (e.tool)()).collect()
    }

    /// The capabilities of `browser` with every server flag on.
    fn all_flags(browser: BrowserKind) -> Capabilities {
        let flags = CapabilityFlags {
            server: crate::cli::ServerFlags {
                allow_cookie_import: true,
                enable_webmcp: true,
                local: false,
            },
            proxy: true,
            firefox_debugger: true,
        };
        Capabilities::new(browser, flags)
    }

    /// TOL-03: Chrome-only tools say so in their description.
    #[test]
    fn browser_limited_tools_say_so() {
        let firefox = all_flags(BrowserKind::Firefox);
        for entry in table() {
            let tool = (entry.tool)();
            let description = tool.description.unwrap_or_default();
            let chrome_only = entry.needs.iter().any(|c| !firefox.has(*c));
            let says_so = description.ends_with("Chrome only (not available on Firefox).");
            assert_eq!(chrome_only, says_so, "{}: {description}", tool.name);
        }
    }

    /// TOL-06: Descriptions only reference existing tools.
    #[test]
    fn descriptions_reference_existing_tools() {
        let tools = all_tools();
        let known: BTreeSet<String> = tools.iter().map(|t| t.name.clone()).collect();
        for tool in &tools {
            let description = tool.description.clone().unwrap_or_default();
            for quoted in description.split(['\'', '`']).skip(1).step_by(2) {
                let is_tool_like = quoted.contains('_')
                    && quoted.chars().all(|c| c.is_ascii_lowercase() || c == '_');
                let arguments = ARGUMENT_NAMES;
                if is_tool_like && !arguments.contains(&quoted) {
                    assert!(
                        known.contains(quoted),
                        "{} references unknown `{quoted}`",
                        tool.name
                    );
                }
            }
        }
        let remove = tools
            .iter()
            .find(|t| t.name == "remove_breakpoint")
            .unwrap();
        assert!(
            !remove
                .description
                .clone()
                .unwrap_or_default()
                .contains("requires an active, paused")
        );
    }

    /// BID-21 / FWS-05: WebSocket frames on Firefox need the debugger port.
    #[tokio::test]
    async fn websocket_frames_need_the_firefox_debugger() {
        let mut handler = handler_with_firefox();
        handler.firefox_spec.firefox.no_firefox_debugger = true;
        let args = serde_json::json!({"instance_id": "ff", "type_filter": "websocket"});
        let message = error_of(&handler, "get_network_logs", args).await;
        assert!(message.contains("--no-firefox-debugger"), "{message}");
    }

    /// RDP-13 / CAP-06: raw Firefox tools need their protocol and the debugger port.
    #[tokio::test]
    async fn raw_rdp_needs_rdp() {
        let mut handler = handler_with_firefox();
        handler.firefox_spec.firefox.no_firefox_debugger = true;
        let args =
            serde_json::json!({"instance_id": "ff", "to": "root", "request_type": "listTabs"});
        let message = error_of(&handler, "send_rdp_request", args).await;
        assert!(message.contains("--no-firefox-debugger"), "{message}");
        let mut chrome = McpHandler::new_test();
        chrome.tool_groups = vec![ToolGroup::All];
        let message = error_of(
            &chrome,
            "send_bidi_command",
            serde_json::json!({"method": "x.y"}),
        )
        .await;
        assert!(
            message.contains("not available on Chrome (CDP)"),
            "{message}"
        );
    }

    /// The section of the README that starts at `heading`.
    fn readme_section<'a>(readme: &'a str, heading: &str) -> &'a str {
        let start = readme.find(heading).expect(heading);
        let rest = &readme[start + heading.len()..];
        &rest[..rest.find("\n## ").unwrap_or(rest.len())]
    }

    /// DOC-02: The README feature matrix matches the tool table, and every
    /// tool Firefox cannot serve is listed under "Limitations and TODO".
    #[test]
    fn readme_matrix_matches_tool_table() {
        // Given the README and the capabilities of each browser
        let readme = include_str!("../../README.md");
        let matrix = readme_section(readme, "## Feature matrix");
        let limitations = readme_section(readme, "## Limitations and TODO");
        let (chrome, firefox) = (
            all_flags(BrowserKind::Chrome),
            all_flags(BrowserKind::Firefox),
        );
        for entry in table() {
            let name = (entry.tool)().name;
            // When the tool's matrix row is read
            let rows: Vec<&str> = matrix
                .lines()
                .filter(|l| l.starts_with(&format!("| `{name}` |")))
                .collect();
            assert_eq!(rows.len(), 1, "{name} appears once in the matrix");
            let cells: Vec<&str> = rows[0].split('|').map(str::trim).collect();
            // Then each verdict matches what the browser can serve
            let serves = |caps: &Capabilities| entry.needs.iter().all(|c| caps.has(*c));
            let firefox_ok =
                serves(&firefox) && (entry.firefox.is_some() || entry.needs.is_empty());
            assert_eq!(cells[2] != "✗", serves(&chrome), "{name}: Chrome cell");
            assert_eq!(cells[3] != "✗", firefox_ok, "{name}: Firefox cell");
            if !firefox_ok {
                assert!(
                    limitations.contains(&format!("`{name}`")),
                    "{name} not in Limitations"
                );
            }
        }
    }
}
