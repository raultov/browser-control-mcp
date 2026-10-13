//! What the server tells MCP clients about itself at `initialize`.

use rust_mcp_sdk::schema::{
    Implementation, InitializeResult, ProtocolVersion, ServerCapabilities, ServerCapabilitiesTools,
};

/// Human-readable product name.
pub const TITLE: &str = "Browser Control MCP";

/// One-paragraph description; it leads with debugging because the name does not say it.
pub const DESCRIPTION: &str = "Control and debug real browsers at runtime: set breakpoints, pause on load, inspect call frames, evaluate in a paused frame and step through JavaScript on Chrome (CDP) and Firefox (WebDriver BiDi + Firefox RDP), plus navigation, DOM, input, screenshots, console and network capture.";

/// Usage instructions for the agent (kept in sync with the tool table by
/// `instructions_reference_existing_tools`).
pub const INSTRUCTIONS: &str = concat!(
    "Browser Control MCP Server Instructions:\n",
    "- Browsers: every instance runs Chrome (CDP) or Firefox (WebDriver BiDi + RDP debugger). Pick it with open_instance's `browser` (\"chrome\" or \"firefox\"); the default instance uses the server's --browser flag (Chrome unless configured). Choose Firefox only when the user asks for it or Chrome is unavailable. An instance never changes browser: every command on it uses its protocol.\n",
    "- Instances: a 'default' instance always exists and is launched lazily on first use. `open_instance` with a unique 'label' creates another one (the label becomes its instance_id). Pass instance_id to address it (omit it for the default). `list_instances` shows each instance's browser and capabilities; `close_instance` stops one.\n",
    "- Tabs: `open_tab`, `list_tabs`, `switch_tab`, `close_tab`. Every tool accepts tab_id; omit it for the active tab. Console logs, network traffic and breakpoints are kept per tab.\n",
    "- Browser limits: performance metrics, tracing, WebMCP, cookie import and launch features are Chrome only; Firefox WebSocket frames and page errors need its debugger port. A tool an instance cannot serve fails with an error naming the tool and the browser; nothing is sent to the browser.\n",
    "- Debugging: set_breakpoint, remove_breakpoint, resume, step_over, evaluate_on_call_frame and search_scripts work on Chrome and Firefox; pause_on_load is Chrome only for now. On Firefox they use the Firefox debugger port, enabled unless the server runs with --no-firefox-debugger; set breakpoints by script URL.\n",
    "- Firefox sessions: a Firefox allows one automation session at a time. Always stop Firefox instances with close_instance or stop_browser when done.\n",
    "- Raw protocol: send_cdp_command (Chrome), send_bidi_command and send_rdp_request (Firefox).\n",
    "- Cookie import: with --allow-cookie-import, navigate, open_instance and restart_browser accept copy_cookies: true (Chrome only). Always ask the user before copying their real cookies.\n",
    "- Proxy: with --proxy-server, instances use the proxy and enable_proxy_auth answers proxy (407) authentication challenges.\n",
    "- WebMCP: with --enable-webmcp, Chrome instances expose webmcp_list_tools, webmcp_invoke_tool, webmcp_get_invocation and webmcp_list_invocations."
);

/// The `initialize` result: name and version from Cargo, title, description,
/// tools capability and instructions.
pub fn initialize_result() -> InitializeResult {
    InitializeResult {
        server_info: Implementation {
            name: env!("CARGO_PKG_NAME").into(),
            version: env!("CARGO_PKG_VERSION").into(),
            title: Some(TITLE.into()),
            description: Some(DESCRIPTION.into()),
            icons: vec![],
            website_url: Some(env!("CARGO_PKG_REPOSITORY").into()),
        },
        capabilities: ServerCapabilities {
            tools: Some(ServerCapabilitiesTools { list_changed: None }),
            ..Default::default()
        },
        protocol_version: ProtocolVersion::V2025_11_25.into(),
        instructions: Some(INSTRUCTIONS.to_string()),
        meta: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SRV-01: Server info names the product and leads with debugging.
    #[test]
    fn server_info_leads_with_debugging() {
        let info = initialize_result().server_info;
        assert_eq!(info.name, "browser-control-mcp");
        assert_eq!(info.title.as_deref(), Some("Browser Control MCP"));
        let description = info.description.unwrap_or_default();
        assert!(
            description.starts_with("Control and debug"),
            "{description}"
        );
        for word in ["breakpoints", "Chrome", "Firefox"] {
            assert!(description.contains(word), "{word} missing");
        }
    }

    /// SRV-02: Instructions mention only tools that exist, plus the browser rules.
    #[test]
    fn instructions_reference_existing_tools() {
        let mut handler = crate::handler::test_handler();
        handler.proxy.proxy_server = Some("http://p:1".into());
        handler.flags.enable_webmcp = true;
        let tools: Vec<String> = crate::handler::listed_tool_names(&handler);
        for word in INSTRUCTIONS.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')) {
            let looks_like_tool =
                word.contains('_') && word.chars().all(|c| c.is_ascii_lowercase() || c == '_');
            let known_non_tools = ["instance_id", "tab_id", "copy_cookies"];
            if looks_like_tool && !known_non_tools.contains(&word) {
                assert!(
                    tools.iter().any(|t| t == word),
                    "unknown tool `{word}` in instructions"
                );
            }
        }
        for needle in ["--browser", "close_instance", "one automation session"] {
            assert!(INSTRUCTIONS.contains(needle), "{needle} missing");
        }
    }
}
