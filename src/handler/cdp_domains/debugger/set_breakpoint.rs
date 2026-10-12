use crate::handler::McpHandler;
use rust_mcp_sdk::{
    macros,
    schema::{CallToolError, CallToolRequestParams, CallToolResult},
};
use serde_json::json;

#[macros::mcp_tool(
    name = "set_breakpoint",
    description = "Sets a debugger breakpoint at a specific location, pausing execution when reached. Side effects: modifies debugger state (breakpoint added until removed). Prerequisites: requires an active Chrome tab; target script must be loaded. Returns: breakpoint identifier and location confirmation. Use this to debug specific code paths. Alternatives: 'pause_on_load' for early script execution, 'search_scripts' to find scripts by pattern."
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct SetBreakpointTool {
    /// Chrome instance id from open_instance/list_instances. Omit for the default instance.
    pub instance_id: Option<String>,
    /// The Tab ID of the target tab. Omit to use the active tab.
    pub tab_id: Option<String>,
    /// Script hash to identify the target script. Constraints: one of 'script_hash', 'script_id', or 'url' must be provided. Defaults to: None.
    pub script_hash: Option<String>,
    /// Script ID (from Debugger.scriptParsed event). Constraints: one of 'script_hash', 'script_id', or 'url' must be provided. Interactions: mutually exclusive with 'script_hash' and 'url' (first match wins). Defaults to: None.
    pub script_id: Option<String>,
    /// Script URL to match. Constraints: one of 'script_hash', 'script_id', or 'url' must be provided. Interactions: mutually exclusive with 'script_hash' and 'script_id'. Defaults to: None.
    pub url: Option<String>,
    /// Line number where breakpoint is set (0-indexed). Constraints: non-negative integer, must be within script bounds. Interactions: required parameter; combined with 'column_number' to pinpoint exact location.
    pub line_number: u32,
    /// Column number within the line. Constraints: non-negative integer. Interactions: optional; narrower precision if provided. Defaults to: start of line (0).
    pub column_number: Option<u32>,
}

impl SetBreakpointTool {
    pub async fn handle(
        params: CallToolRequestParams,
        handler: &McpHandler,
    ) -> Result<CallToolResult, CallToolError> {
        let args: SetBreakpointTool = crate::handler::parse_args(params)?;
        let session = handler.session(args.instance_id.clone()).await?;
        let (method, params) = args.command().ok_or_else(|| {
            CallToolError::from_message("Either script_id, url or script_hash must be provided")
        })?;
        let target = session.target(args.tab_id.clone()).await?;
        let failure = "Failed to set breakpoint";
        let response = crate::handler::send_or(&target, method, params, failure).await?;
        Ok(CallToolResult::text_content(vec![
            format!("Breakpoint set successfully. Response: {:?}", response).into(),
        ]))
    }

    /// The CDP command for the location given, by precedence script id, URL,
    /// script hash; `None` when none is given.
    fn command(&self) -> Option<(&'static str, serde_json::Value)> {
        let (method, mut location, wrap) = if let Some(id) = &self.script_id {
            ("Debugger.setBreakpoint", json!({"scriptId": id}), true)
        } else if let Some(url) = &self.url {
            ("Debugger.setBreakpointByUrl", json!({"url": url}), false)
        } else {
            let hash = self.script_hash.as_ref()?;
            (
                "Debugger.setBreakpointByUrl",
                json!({"scriptHash": hash}),
                false,
            )
        };
        if let Some(map) = location.as_object_mut() {
            map.insert("lineNumber".into(), json!(self.line_number));
            if let Some(col) = self.column_number {
                map.insert("columnNumber".into(), json!(col));
            }
        }
        let params = if wrap {
            json!({ "location": location })
        } else {
            location
        };
        Some((method, params))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::cdp_domains::tests::spawn_mock_chrome_server;
    use crate::handler::chrome_instance::MockChromeManager;
    use rust_mcp_sdk::schema::CallToolRequestParams;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    #[tokio::test]
    async fn test_set_breakpoint_validation() {
        let handler = McpHandler::new_test();

        // Missing all identifiers
        let params: CallToolRequestParams = serde_json::from_value(json!({
            "name": "set_breakpoint",
            "arguments": {
                "line_number": 10
            }
        }))
        .unwrap();
        let result = SetBreakpointTool::handle(params, &handler).await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Either script_id, url or script_hash must be provided")
        );
    }

    #[tokio::test]
    async fn test_set_breakpoint_handle() {
        let port = spawn_mock_chrome_server().await;

        let mut handler = McpHandler::new_test();
        Arc::get_mut(&mut handler.default_session)
            .unwrap()
            .chrome_manager = Arc::new(Mutex::new(MockChromeManager::new(port)));

        let params: CallToolRequestParams = serde_json::from_value(json!({
            "name": "set_breakpoint",
            "arguments": {
                "script_id": "1",
                "line_number": 10,
                "column_number": 5
            }
        }))
        .unwrap();

        let result = SetBreakpointTool::handle(params, &handler).await;
        assert!(result.is_ok(), "Handle should succeed: {:?}", result.err());

        let call_result = result.unwrap();
        assert!(!call_result.content.is_empty());
        let content_str = format!("{:?}", call_result.content);
        assert!(
            content_str.contains("Breakpoint set successfully"),
            "Content didn't match: {}",
            content_str
        );
    }
}
