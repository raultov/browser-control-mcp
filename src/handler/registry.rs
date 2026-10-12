//! The tool table: one entry per tool with its schema, the flag that gates
//! it and its handler. Listing and calling both iterate this table, so a
//! flag-gated tool can never be listed without being callable or vice versa.

use super::McpHandler;
use super::cdp_domains;
use super::chrome_instance;
use crate::backend::{BrowserKind, Capability};
#[cfg(test)]
use crate::backend::{Capabilities, capabilities::CapabilityFlags};
use rust_mcp_sdk::schema::{CallToolError, CallToolRequestParams, CallToolResult, Tool};
use std::future::Future;
use std::pin::Pin;

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
}

impl Gate {
    fn is_open(self, handler: &McpHandler) -> bool {
        match self {
            Self::Always => true,
            Self::ProxyServer => handler.proxy.proxy_server.is_some(),
            Self::Webmcp => handler.flags.enable_webmcp,
        }
    }

    fn disabled_message(self, name: &str) -> String {
        let (flag, what) = match self {
            Self::Always => ("", ""),
            Self::ProxyServer => ("--proxy-server", "proxy support"),
            Self::Webmcp => ("--enable-webmcp", "WebMCP support"),
        };
        format!(
            "Tool '{name}' is disabled. Start browser-control-mcp with {flag} to enable {what}."
        )
    }
}

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
    ($tool:ty, $needs:expr, $firefox:path) => {
        ToolEntry {
            tool: <$tool>::tool,
            gate: Gate::Always,
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
    tools.extend(firefox_tools());
    tools.extend(gated_tools());
    tools
}

fn page_tools() -> Vec<ToolEntry> {
    use crate::backend::bidi::tools as ff;
    use Capability::*;
    use Gate::Always;
    use cdp_domains::input::{
        click_element::ClickElementTool as Click, fill_input::FillInputTool as Fill,
    };
    use cdp_domains::page::{
        capture_screenshot::CaptureScreenshotTool as Shot, navigate::NavigateTool as Nav,
    };
    use cdp_domains::page::{reload::ReloadTool as Reload, scroll::ScrollTool as Scroll};
    use cdp_domains::runtime::{
        evaluate_js::EvaluateJsTool as Eval, inspect_dom::InspectDomTool as Dom_,
    };
    vec![
        entry!(Shot, Always, &[Screenshot], ff::capture_screenshot),
        entry!(Click, Always, &[Input], ff::click_element),
        entry!(Fill, Always, &[Input], ff::fill_input),
        entry!(Eval, Always, &[Evaluate], ff::evaluate_js),
        entry!(Nav, Always, &[Navigation], ff::navigate),
        entry!(Dom_, Always, &[Dom], ff::inspect_dom),
        entry!(Reload, Always, &[Navigation], ff::reload),
        entry!(Scroll, Always, &[Dom], ff::scroll),
    ]
}

fn debugger_tools() -> Vec<ToolEntry> {
    use crate::backend::bidi::debugger as ff;
    use Capability::{JsDebugger, PauseOnLoad};
    use Gate::Always;
    use cdp_domains::debugger::evaluate_on_call_frame::EvaluateOnCallFrameTool as Frame;
    use cdp_domains::debugger::pause_on_load::PauseOnLoadTool as PauseLoad;
    use cdp_domains::debugger::remove_breakpoint::RemoveBreakpointTool as Remove;
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
    ]
}

fn observation_tools() -> Vec<ToolEntry> {
    use crate::backend::bidi::tools as ff;
    use Capability::*;
    use Gate::Always;
    use cdp_domains::performance::get_performance_metrics as metrics;
    use cdp_domains::tracing::profile_page_performance as profile;
    use cdp_domains::{custom, log, network};
    vec![
        entry!(
            network::get_network_logs::GetNetworkLogsTool,
            Always,
            &[NetworkLogs],
            ff::network_logs
        ),
        entry!(
            log::get_console_logs::GetConsoleLogsTool,
            Always,
            &[ConsoleLogs],
            ff::console_logs
        ),
        entry!(
            metrics::GetPerformanceMetricsTool,
            Always,
            &[PerformanceMetrics]
        ),
        entry!(profile::ProfilePagePerformanceTool, Always, &[Tracing]),
        entry!(
            custom::send_cdp_command::SendCdpCommandTool,
            Always,
            &[RawCdp]
        ),
        entry!(
            custom::get_custom_events::GetCustomEventsTool,
            Always,
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
            &[Capability::RawBidi],
            raw::send_bidi_command
        ),
        firefox_only!(
            SendRdpRequestTool,
            &[Capability::RawRdp],
            raw::send_rdp_request
        ),
    ]
}

fn gated_tools() -> Vec<ToolEntry> {
    use cdp_domains::fetch::enable_proxy_auth::EnableProxyAuthTool;
    use cdp_domains::webmcp::{GetWebmcpInvocationTool, InvokeWebmcpToolTool};
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

/// Runs the tool named in `params`: flag gate, then the target instance's
/// capabilities (so a refused call sends nothing), then the implementation of
/// that instance's browser.
pub(crate) async fn call(
    handler: &McpHandler,
    params: CallToolRequestParams,
) -> Result<CallToolResult, CallToolError> {
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
    let browser = target_browser(handler, &params);
    let needs = argument_needs(&params).chain(entry.needs.iter().copied());
    handler
        .capabilities(browser)
        .require(&params.name, &needs.collect::<Vec<_>>())?;
    match (browser, entry.firefox) {
        (BrowserKind::Chrome, _) => (entry.chrome)(params, handler).await,
        (BrowserKind::Firefox, Some(firefox)) => firefox(params, handler).await,
        (BrowserKind::Firefox, None) => Err(CallToolError::from_message(format!(
            "`{}` is not implemented for Firefox yet (see the README's TODO list)",
            params.name
        ))),
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

    /// RFC-04: Every listed tool is callable and every callable tool is listed.
    #[test]
    fn listed_and_callable_tools_match() {
        // Given a handler with every flag enabled
        let mut handler = McpHandler::new_test();
        handler.proxy.proxy_server = Some("http://p:1".into());
        handler.flags.enable_webmcp = true;
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
        let handler = McpHandler::new_test();
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

    /// TOL-01: The tool list has the 0.1.0 names.
    #[test]
    fn tool_list_has_v0_1_names() {
        let mut handler = McpHandler::new_test();
        handler.proxy.proxy_server = Some("http://p:1".into());
        handler.flags.enable_webmcp = true;
        let listed = names(listed_tools(&handler));
        assert_eq!(listed.len(), 37);
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

    /// CAP-03: A tool the browser cannot serve fails with a capability error.
    #[tokio::test]
    async fn unsupported_tool_fails_without_touching_the_browser() {
        // Given an instance backed by Firefox (BiDi)
        let handler = handler_with_firefox();
        // When the agent calls "profile_page_performance" on it
        let message = error_of(
            &handler,
            "profile_page_performance",
            serde_json::json!({"instance_id": "ff"}),
        )
        .await;
        // Then the error names the tool and the browser, and no implementation ran
        assert!(
            message.contains("`profile_page_performance` is not available on Firefox (BiDi)"),
            "{message}"
        );
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
        "confirm_restart",
        "copy_cookies",
        "current_navigation_only",
        "disable_cache",
        "duration_ms",
        "full_page",
        "include_details",
        "include_sensitive_headers",
        "instance_id",
        "is_default",
        "prewarm_url",
        "profile_dir",
        "resource_type",
        "source_profile",
        "tab_id",
        "target_id",
        "type_filter",
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

    /// BID-21: WebSocket frames are reported as unavailable on Firefox.
    #[tokio::test]
    async fn websocket_frames_reported_unavailable() {
        let handler = handler_with_firefox();
        let args = serde_json::json!({"instance_id": "ff", "type_filter": "websocket"});
        let message = error_of(&handler, "get_network_logs", args).await;
        assert!(message.contains("WebSocket frame"), "{message}");
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
        let chrome = McpHandler::new_test();
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
