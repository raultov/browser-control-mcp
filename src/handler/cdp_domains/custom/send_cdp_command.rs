use crate::handler::{McpHandler, is_local_address};
use rust_mcp_sdk::{
    macros,
    schema::{CallToolError, CallToolRequestParams, CallToolResult},
};

/// Commands that would silently stop what the server captures or break
/// the instance, with what to do instead.
const REFUSED: &[(&str, &str)] = &[
    (
        "Network.disable",
        "get_network_logs reads this domain's events, and they would stop without a word",
    ),
    (
        "Page.disable",
        "navigate, reload, dialogs and the network log follow this domain's events",
    ),
    (
        "Runtime.disable",
        "get_console_logs reads this domain's events, and they would stop without a word",
    ),
    (
        "Log.disable",
        "get_console_logs reads this domain's events, and they would stop without a word",
    ),
    (
        "Debugger.disable",
        "breakpoints and search_scripts need the debugger; remove_breakpoint and resume instead",
    ),
    ("Browser.close", "use close_instance or stop_browser"),
];

/// Commands that are sent, with what the agent should know afterwards.
const NOTED: &[(&str, &str)] = &[(
    "Fetch.enable",
    "requests now pause until answered with Fetch.continueRequest, Fetch.fulfillRequest or Fetch.failRequest (their Fetch.requestPaused events are in get_custom_events); Fetch.disable lets them through again",
)];

#[macros::mcp_tool(
    name = "send_cdp_command",
    description = "EXPERIMENTAL: Sends raw Chrome DevTools Protocol (CDP) commands to the browser for advanced use cases not covered by specialized tools. Side effects: depends on command; may modify page state, DOM, or trigger navigation. Auth requirements: subject to local-only restrictions if enabled (Page.navigate and Target.createTarget checked). Refuses commands that would silently stop what the server captures or close the browser: Network.disable, Page.disable, Runtime.disable, Log.disable, Debugger.disable, Browser.close. Fetch.enable comes with a note: requests then pause until answered. Prerequisites: requires knowledge of CDP protocol; active Chrome connection. Returns: raw CDP command response as JSON. Use this only when specialized tools inadequate. Alternatives: use domain-specific tools (navigate, click_element, evaluate_js). Chrome only (not available on Firefox)."
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct SendCdpCommandTool {
    /// Chrome instance id from open_instance/list_instances. Omit for the default instance.
    pub instance_id: Option<String>,
    /// The Tab ID of the target tab. Omit to use the active tab.
    pub tab_id: Option<String>,
    /// CDP protocol method name (e.g., 'DOM.getDocument', 'Runtime.evaluate'). Constraints: valid CDP domain.method format. Interactions: method must be recognized by Chrome protocol version.
    pub method: String,
    /// JSON-formatted parameters for the CDP command. Constraints: valid JSON object string. Interactions: Page.navigate and Target.createTarget URLs subject to local-only restrictions; empty string or '{}' for no parameters. Defaults to: None.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<String>,
}

impl SendCdpCommandTool {
    pub async fn handle(
        params: CallToolRequestParams,
        handler: &McpHandler,
    ) -> Result<CallToolResult, CallToolError> {
        let tool: SendCdpCommandTool = serde_json::from_value(serde_json::Value::Object(
            params.arguments.unwrap_or_default(),
        ))
        .map_err(|e| CallToolError::from_message(format!("Failed to parse arguments: {}", e)))?;
        if let Some((_, why)) = REFUSED.iter().find(|(m, _)| *m == tool.method) {
            return Err(CallToolError::from_message(format!(
                "`{}` is refused: {why}",
                tool.method
            )));
        }
        let parsed_params: serde_json::Value = match &tool.params {
            Some(s) if !s.trim().is_empty() => serde_json::from_str(s).map_err(|e| {
                CallToolError::from_message(format!("Invalid JSON in params: {}", e))
            })?,
            _ => serde_json::Value::Object(Default::default()),
        };

        if handler.flags.local
            && let Some(url) = crate::util::url::raw_navigation_url(&tool.method, &parsed_params)
            && !is_local_address(url)
        {
            return Err(CallToolError::from_message(format!(
                "Navigation to '{}' via raw CDP command is blocked. This MCP server is running with --local, which restricts navigation to local addresses only (localhost, 127.0.0.1, 192.168.x.x, or *.local). To allow navigation to external addresses, restart the MCP server without --local.",
                url
            )));
        }

        let session = handler.session(tool.instance_id.clone()).await?;
        let target = session.target(tool.tab_id.clone()).await?;

        // Extract domain from method (e.g., "DOM" from "DOM.getDocument")
        if let Some(domain) = tool.method.split('.').next() {
            let custom_state = session.custom_state(tool.tab_id.clone())?;
            super::ensure_domain_listener(&target, &custom_state, domain).await;
        }

        let response = target.send_raw_command(&tool.method, parsed_params).await;
        match response {
            Ok(res) => {
                let mut text = crate::handler::reply_text(
                    &format!("Command '{}' executed successfully.", tool.method),
                    &res,
                );
                if let Some((_, note)) = NOTED.iter().find(|(m, _)| *m == tool.method) {
                    text.push_str(&format!("\nNote: {note}"));
                }
                Ok(CallToolResult::text_content(vec![text.into()]))
            }
            Err(e) => Err(CallToolError::from_message(format!(
                "Failed to execute CDP command '{}': {}",
                tool.method, e
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::cdp_domains::tests::spawn_mock_chrome_server;
    use crate::handler::chrome_instance::MockChromeManager;
    use rust_mcp_sdk::schema::CallToolRequestParams;
    use serde_json::json;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    /// AUD-RW-09: a command that would silently stop what the server
    /// captures (Network.disable left get_network_logs empty) is refused
    /// before anything is sent.
    #[tokio::test]
    async fn state_breaking_commands_are_refused() {
        use crate::handler::cdp_domains::tests::{call, mock_handler, spawn_recording_mock};
        let mock = spawn_recording_mock().await;
        let handler = mock_handler(mock.http_port);
        for method in [
            "Network.disable",
            "Page.disable",
            "Runtime.disable",
            "Log.disable",
            "Debugger.disable",
            "Browser.close",
        ] {
            let args = json!({"method": method});
            let error = SendCdpCommandTool::handle(call("send_cdp_command", args), &handler)
                .await
                .unwrap_err()
                .to_string();
            assert!(
                error.starts_with(&format!("`{method}` is refused: ")),
                "{error}"
            );
            assert!(mock.commands_named(method).is_empty(), "{method} was sent");
        }
    }

    /// AUD-RW-09: Fetch.enable is sent, with a note: every matching request
    /// now waits for the agent.
    #[tokio::test]
    async fn fetch_enable_comes_with_a_note() {
        use crate::handler::cdp_domains::tests::{
            call, mock_handler, reply_text, spawn_recording_mock,
        };
        let mock = spawn_recording_mock().await;
        let handler = mock_handler(mock.http_port);
        let args = json!({"method": "Fetch.enable", "params": "{}"});
        let reply = SendCdpCommandTool::handle(call("send_cdp_command", args), &handler)
            .await
            .unwrap();
        assert_eq!(mock.commands_named("Fetch.enable").len(), 1);
        let text = reply_text(&reply);
        assert!(text.contains("Note: requests now pause"), "{text}");
    }

    #[tokio::test]
    async fn test_send_cdp_command_schema() {
        let tool_def = SendCdpCommandTool::tool();
        println!("{}", serde_json::to_string_pretty(&tool_def).unwrap());
    }

    #[tokio::test]
    async fn test_send_cdp_command_params_deserialization() {
        let params: Result<CallToolRequestParams, _> = serde_json::from_value(json!({
            "name": "send_cdp_command",
            "arguments": {
                "method": "Runtime.enable",
                "params": "{}"
            }
        }));
        assert!(params.is_ok());
    }

    #[tokio::test]
    async fn test_send_cdp_command_handle() {
        let port = spawn_mock_chrome_server().await;

        let mut handler = McpHandler::new_test();
        Arc::get_mut(&mut handler.default_session)
            .unwrap()
            .chrome_manager = Arc::new(Mutex::new(MockChromeManager::new(port)));

        let params: CallToolRequestParams = serde_json::from_value(json!({
            "name": "send_cdp_command",
            "arguments": {
                "method": "Runtime.enable",
                "params": "{}"
            }
        }))
        .unwrap();

        let result = SendCdpCommandTool::handle(params, &handler).await;
        assert!(result.is_ok(), "Handle should succeed: {:?}", result.err());

        let call_result = result.unwrap();
        assert!(!call_result.content.is_empty());
        let content_str = format!("{:?}", call_result.content);
        assert!(
            content_str.contains("Command 'Runtime.enable' executed successfully"),
            "Content didn't match: {}",
            content_str
        );

        // Verify active_domains has 'Runtime'
        let st = handler.default_session.custom_state.lock().await;
        assert!(st.active_domains.contains("Runtime"));
    }

    #[tokio::test]
    async fn test_send_cdp_command_local_only_restriction() {
        let mut handler = McpHandler::new_test();
        handler.flags.local = true;

        let params: CallToolRequestParams = serde_json::from_value(json!({
            "name": "send_cdp_command",
            "arguments": {
                "method": "Page.navigate",
                "params": "{\"url\": \"https://google.com\"}"
            }
        }))
        .unwrap();

        let result = SendCdpCommandTool::handle(params, &handler).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.to_string()
                .contains("Navigation to 'https://google.com' via raw CDP command is blocked")
        );

        // Test with local address
        let params_local: CallToolRequestParams = serde_json::from_value(json!({
            "name": "send_cdp_command",
            "arguments": {
                "method": "Page.navigate",
                "params": "{\"url\": \"http://localhost:3000\"}"
            }
        }))
        .unwrap();

        let port = spawn_mock_chrome_server().await;
        let mut handler_local2 = McpHandler::new_test_with_port(port);
        handler_local2.flags.local = true;

        let result_local = SendCdpCommandTool::handle(params_local, &handler_local2).await;
        assert!(
            result_local.is_ok(),
            "Local navigation should succeed: {:?}",
            result_local.err()
        );
    }

    #[tokio::test]
    async fn test_send_cdp_command_routes_custom_state_to_tab() {
        let handler = McpHandler::new_test();
        let session = handler.session(None).await.unwrap();

        // Register a real tab over the mock DevTools server so that
        // session.target(tab_id) resolves to the tab's own CDP target.
        let tab_id = {
            use cdp_browser_lite::BrowserClient;
            use std::time::Duration;

            let port = spawn_mock_chrome_server().await;
            let browser =
                BrowserClient::connect(&format!("127.0.0.1:{}", port), Duration::from_secs(5))
                    .await
                    .expect("BrowserClient connect to mock");
            let tab = browser.attach("T-page-1").await.expect("attach to mock");

            session
                .tabs
                .write()
                .unwrap()
                .register_tab(
                    tab,
                    crate::handler::chrome_instance::tab_registry::NewTab::new(
                        None,
                        "https://example.test",
                    ),
                    false,
                )
                .expect("register tab")
        };

        let params: CallToolRequestParams = serde_json::from_value(json!({
            "name": "send_cdp_command",
            "arguments": {
                "tab_id": tab_id,
                "method": "DOM.getDocument",
                "params": "{}"
            }
        }))
        .unwrap();

        let result = SendCdpCommandTool::handle(params, &handler).await;
        assert!(result.is_ok(), "Handle should succeed: {:?}", result.err());

        // The custom listener must live in the TAB's state, not the session's.
        let tab_custom_state = session.custom_state(Some(tab_id)).unwrap();
        assert!(
            tab_custom_state.lock().await.active_domains.contains("DOM"),
            "custom listener must be registered in the tab's CustomState"
        );
        assert!(
            session.custom_state.lock().await.active_domains.is_empty(),
            "session CustomState must stay untouched when a tab was targeted"
        );
    }

    /// SEC-08: --local blocks raw navigation methods.
    #[tokio::test]
    async fn local_mode_blocks_raw_navigation_methods() {
        for method in ["Page.navigate", "Target.createTarget"] {
            // Given --local and a browser nobody can reach (any command would fail)
            let mut handler = McpHandler::new_test_with_port(0);
            handler.flags.local = true;
            // When a raw navigation to an external URL is sent
            let call: CallToolRequestParams = serde_json::from_value(serde_json::json!({
                "name": "send_cdp_command",
                "arguments": {"method": method, "params": "{\"url\":\"https://example.com\"}"}
            }))
            .unwrap();
            let result = SendCdpCommandTool::handle(call, &handler).await;
            // Then it fails with the --local error and nothing is sent
            let message = format!("{:?}", result.err().unwrap());
            assert!(message.contains("--local"), "{method}: {message}");
        }
    }
}
