//! The tool table: one entry per tool with its schema, the flag that gates
//! it and its handler. Listing and calling both iterate this table, so a
//! flag-gated tool can never be listed without being callable or vice versa.

use super::McpHandler;
use super::cdp_domains;
use super::chrome_instance;
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

/// One row of the tool table.
pub(crate) struct ToolEntry {
    /// The tool's schema (name, description, input schema).
    pub(crate) tool: fn() -> Tool,
    /// The flag it needs.
    pub(crate) gate: Gate,
    /// Its handler.
    pub(crate) call: for<'a> fn(CallToolRequestParams, &'a McpHandler) -> ToolFuture<'a>,
}

macro_rules! entry {
    ($tool:ty, $gate:expr) => {
        ToolEntry {
            tool: <$tool>::tool,
            gate: $gate,
            call: |params, handler| Box::pin(<$tool>::handle(params, handler)),
        }
    };
}

/// Every tool, in listing order.
pub(crate) fn table() -> Vec<ToolEntry> {
    use Gate::{Always, ProxyServer, Webmcp};
    use cdp_domains::{custom, debugger, fetch, input, log, network, page, performance, runtime};
    vec![
        entry!(page::capture_screenshot::CaptureScreenshotTool, Always),
        entry!(input::click_element::ClickElementTool, Always),
        entry!(input::fill_input::FillInputTool, Always),
        entry!(runtime::evaluate_js::EvaluateJsTool, Always),
        entry!(page::navigate::NavigateTool, Always),
        entry!(runtime::inspect_dom::InspectDomTool, Always),
        entry!(debugger::pause_on_load::PauseOnLoadTool, Always),
        entry!(debugger::step_over::StepOverTool, Always),
        entry!(debugger::resume::ResumeTool, Always),
        entry!(debugger::search_scripts::SearchScriptsTool, Always),
        entry!(debugger::set_breakpoint::SetBreakpointTool, Always),
        entry!(
            debugger::evaluate_on_call_frame::EvaluateOnCallFrameTool,
            Always
        ),
        entry!(page::reload::ReloadTool, Always),
        entry!(page::scroll::ScrollTool, Always),
        entry!(debugger::remove_breakpoint::RemoveBreakpointTool, Always),
        entry!(chrome_instance::restart_chrome::RestartChromeTool, Always),
        entry!(chrome_instance::stop_chrome::StopChromeTool, Always),
        entry!(chrome_instance::open_instance::OpenInstanceTool, Always),
        entry!(chrome_instance::list_instances::ListInstancesTool, Always),
        entry!(chrome_instance::close_instance::CloseInstanceTool, Always),
        entry!(chrome_instance::open_tab::OpenTabTool, Always),
        entry!(chrome_instance::list_tabs::ListTabsTool, Always),
        entry!(chrome_instance::close_tab::CloseTabTool, Always),
        entry!(chrome_instance::switch_tab::SwitchTabTool, Always),
        entry!(network::get_network_logs::GetNetworkLogsTool, Always),
        entry!(log::get_console_logs::GetConsoleLogsTool, Always),
        entry!(
            performance::get_performance_metrics::GetPerformanceMetricsTool,
            Always
        ),
        entry!(
            cdp_domains::tracing::profile_page_performance::ProfilePagePerformanceTool,
            Always
        ),
        entry!(custom::send_cdp_command::SendCdpCommandTool, Always),
        entry!(custom::get_custom_events::GetCustomEventsTool, Always),
        entry!(fetch::enable_proxy_auth::EnableProxyAuthTool, ProxyServer),
        entry!(cdp_domains::webmcp::ListWebmcpToolsTool, Webmcp),
        entry!(cdp_domains::webmcp::InvokeWebmcpToolTool, Webmcp),
        entry!(cdp_domains::webmcp::GetWebmcpInvocationTool, Webmcp),
        entry!(cdp_domains::webmcp::ListWebmcpInvocationsTool, Webmcp),
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

/// Runs the tool named in `params`, if it exists and its flag is set.
pub(crate) async fn call(
    handler: &McpHandler,
    params: CallToolRequestParams,
) -> Result<CallToolResult, CallToolError> {
    let entry = table()
        .into_iter()
        .find(|entry| (entry.tool)().name == params.name);
    match entry {
        None => Err(CallToolError::unknown_tool(params.name)),
        Some(entry) if !entry.gate.is_open(handler) => Err(CallToolError::from_message(
            entry.gate.disabled_message(&params.name),
        )),
        Some(entry) => (entry.call)(params, handler).await,
    }
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
}
