//! What the server tells MCP clients about itself at `initialize`.

use rust_mcp_sdk::schema::{
    Implementation, InitializeResult, ProtocolVersion, ServerCapabilities, ServerCapabilitiesTools,
};

use crate::cli::ToolGroup;
use crate::handler::{McpHandler, group_enabled};

/// Human-readable product name.
pub const TITLE: &str = "Browser Control MCP";

/// One-paragraph description; it leads with debugging because the name does not say it.
pub const DESCRIPTION: &str = "Control and debug real browsers at runtime: set breakpoints, pause on load, inspect call frames, evaluate in a paused frame and step through JavaScript on Chrome (CDP) and Firefox (WebDriver BiDi + Firefox RDP), plus navigation, DOM, input, screenshots, console and network capture. The debugger tools, except resume, are listed with --tool-groups debug.";

/// The rules no single tool owns, in every server's instructions. A rule
/// one tool owns is in that tool's description instead: clients put the
/// instructions in the model's context on every turn (CTX-02).
const CORE_INSTRUCTIONS: &str = concat!(
    "Browser Control MCP Server Instructions:\n",
    "- Instances: tools act on the 'default' instance unless given instance_id. It runs the server's --browser (Chrome unless configured) and starts on first use; open_instance starts another, Chrome or Firefox: choose Firefox only when the user asks for it or Chrome is unavailable.\n",
    "- Tabs: tools act on the active tab, the one the browser shows, unless given tab_id. A tab a click opens does not become active; the reply names it.\n",
    "- Selectors: a snapshot ref (e12; f2e5 in a frame of another origin), CSS, 'host >>> inner' into an open shadow root or same-origin iframe, or 'f2 >>> css' into a frame of another origin.\n",
    "- Input tools (click_element, fill_input, fill_form, press_key, hover, drag) wait up to 1 s for the page to settle and say how it reacted; wait_for waits longer.\n",
    "- Firefox allows one automation session at a time: stop Firefox instances with close_instance or stop_browser when done.\n",
);

/// What each opt-in group adds, for the line that names the groups that
/// are off (the agent cannot turn them on: it tells the user the flag).
const GROUPS: [(ToolGroup, &str); 7] = [
    (ToolGroup::Debug, "debug (JavaScript debugger)"),
    (ToolGroup::Storage, "storage (cookies, web storage)"),
    (ToolGroup::Files, "files (upload, PDF, video)"),
    (ToolGroup::Perf, "perf (performance, audits)"),
    (ToolGroup::Emulation, "emulation (viewport, media)"),
    (ToolGroup::Network, "network (mocks, offline)"),
    (ToolGroup::Raw, "raw (protocol commands)"),
];

/// Listed with `--tool-groups debug`.
const DEBUG_INSTRUCTIONS: &str = "- Debugging: set_breakpoint, remove_breakpoint, resume, step_over, step_into, step_out, call_stack (frames, why it paused, what was thrown; on Chrome each frame's scopes), evaluate_on_call_frame (in any frame of the stack; on Chrome 156+ in one of its scopes), pause_on_exceptions and search_scripts work on Chrome and Firefox; pause_on_load is Chrome only for now. On Firefox they use the Firefox debugger port, enabled unless the server runs with --no-firefox-debugger; set breakpoints by script URL.\n";

/// Listed with `--allow-cookie-import`.
const COOKIE_IMPORT_INSTRUCTIONS: &str = "- Cookie import: navigate, open_instance and restart_browser accept copy_cookies: true (Chrome only). Always ask the user before copying their real cookies.\n";

/// Listed with `--proxy-server`.
const PROXY_INSTRUCTIONS: &str = "- Proxy: instances use the proxy, and enable_proxy_auth answers proxy (407) authentication challenges.\n";

/// Listed with `--enable-webmcp`.
const WEBMCP_INSTRUCTIONS: &str = "- WebMCP: Chrome instances expose webmcp_list_tools, webmcp_invoke_tool, webmcp_get_invocation, webmcp_list_invocations and webmcp_cancel_invocation.\n";

/// Usage instructions for the agent: the core rules, then a paragraph per
/// flag that is on, and the groups that are off. Kept in sync with the tool
/// table by `instructions_reference_existing_tools`.
pub(crate) fn instructions(handler: &McpHandler) -> String {
    let mut text = String::from(CORE_INSTRUCTIONS);
    if group_enabled(handler, ToolGroup::Debug) {
        text.push_str(DEBUG_INSTRUCTIONS);
    }
    if handler.flags.allow_cookie_import {
        text.push_str(COOKIE_IMPORT_INSTRUCTIONS);
    }
    if handler.proxy.proxy_server.is_some() {
        text.push_str(PROXY_INSTRUCTIONS);
    }
    if handler.flags.enable_webmcp {
        text.push_str(WEBMCP_INSTRUCTIONS);
    }
    let off: Vec<&str> = GROUPS
        .iter()
        .filter(|(group, _)| !group_enabled(handler, *group))
        .map(|(_, what)| *what)
        .collect();
    if !off.is_empty() {
        text.push_str("- Off unless the server is started with --tool-groups (tell the user when a task needs one): ");
        text.push_str(&off.join(", "));
        text.push_str(", or all.\n");
    }
    text.truncate(text.trim_end().len());
    text
}

/// The `initialize` result: name and version from Cargo, title, description,
/// tools capability and the instructions for `handler`'s flags.
pub(crate) fn initialize_result(handler: &McpHandler) -> InitializeResult {
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
        instructions: Some(instructions(handler)),
        meta: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The most bytes the instructions of a server started without flags
    /// may take. Lowered with every change that shortens them, never
    /// raised: the curve is in `docs/specs/13-roadmap.md`.
    const DEFAULT_INSTRUCTIONS_BUDGET: usize = 1_237;

    /// CTX-02: the default instructions fit their budget.
    #[test]
    fn the_default_instructions_fit_their_budget() {
        // Given a server started without flags
        let handler = crate::handler::test_handler();
        // When it answers initialize
        let size = initialize_result(&handler)
            .instructions
            .unwrap_or_default()
            .len();
        // Then its instructions are within the budget
        assert!(
            size <= DEFAULT_INSTRUCTIONS_BUDGET,
            "the default instructions take {size} bytes, over their budget of {DEFAULT_INSTRUCTIONS_BUDGET}"
        );
    }

    /// SRV-01: Server info names the product and leads with debugging.
    #[test]
    fn server_info_leads_with_debugging() {
        let info = initialize_result(&crate::handler::test_handler()).server_info;
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

    /// A handler with every flag on.
    fn every_flag() -> McpHandler {
        let mut handler = crate::handler::test_handler();
        handler.proxy.proxy_server = Some("http://p:1".into());
        handler.flags.enable_webmcp = true;
        handler.flags.allow_cookie_import = true;
        handler.tool_groups = vec![ToolGroup::All];
        handler
    }

    /// SRV-02: Instructions mention only tools the server lists, plus the
    /// browser rules, without flags and with every flag.
    #[test]
    fn instructions_reference_existing_tools() {
        for handler in [crate::handler::test_handler(), every_flag()] {
            let text = instructions(&handler);
            let tools: Vec<String> = crate::handler::listed_tool_names(&handler);
            for word in text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')) {
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
                assert!(text.contains(needle), "{needle} missing");
            }
        }
    }

    /// SRV-03: the instructions describe what this run lists: a flag's
    /// paragraph only with the flag, and the groups that are off.
    #[test]
    fn instructions_follow_the_flags() {
        // Given a server without flags
        let plain = instructions(&crate::handler::test_handler());
        // Then no flag's paragraph is there, and every group is named as off
        for absent in ["- Debugging:", "- Cookie import:", "- Proxy:", "- WebMCP:"] {
            assert!(!plain.contains(absent), "{absent} listed without its flag");
        }
        for (_, what) in GROUPS {
            assert!(plain.contains(what), "{what} not named as off");
        }
        // Given the debug group only
        let mut handler = crate::handler::test_handler();
        handler.tool_groups = vec![ToolGroup::Debug];
        let debug = instructions(&handler);
        // Then the debugger paragraph is there and debug is not named as off
        assert!(debug.contains("- Debugging:"), "{debug}");
        assert!(!debug.contains("debug (JavaScript debugger)"), "{debug}");
        assert!(debug.contains("storage (cookies, web storage)"), "{debug}");
        // Given every flag
        let all = instructions(&every_flag());
        // Then every paragraph is there and no group is off
        for present in ["- Debugging:", "- Cookie import:", "- Proxy:", "- WebMCP:"] {
            assert!(all.contains(present), "{present} missing with its flag");
        }
        assert!(!all.contains("--tool-groups (tell the user"), "{all}");
    }
}
