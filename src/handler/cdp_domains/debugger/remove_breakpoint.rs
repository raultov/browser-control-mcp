use crate::handler::McpHandler;
use rust_mcp_sdk::{
    macros,
    schema::{CallToolError, CallToolRequestParams, CallToolResult},
};
use serde_json::json;

#[macros::mcp_tool(
    name = "remove_breakpoint",
    description = "Removes a previously set debugger breakpoint by its ID, allowing execution to pass that location uninterrupted. Side effects: modifies debugger state (breakpoint deleted). Prerequisites: the breakpoint ID returned by 'set_breakpoint' (works whether or not execution is stopped). Returns: confirmation of breakpoint removal. Use this to clean up breakpoints or disable debugging at specific locations. Alternatives: 'set_breakpoint' to add new breakpoints, 'resume' to continue execution."
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct RemoveBreakpointTool {
    /// Chrome instance id from open_instance/list_instances. Omit for the default instance.
    pub instance_id: Option<String>,
    /// The Tab ID of the target tab. Omit to use the active tab.
    pub tab_id: Option<String>,
    /// Unique identifier of the breakpoint (returned from set_breakpoint). Constraints: non-empty string matching format from set_breakpoint response. Interactions: must correspond to an active breakpoint or operation will fail.
    pub breakpoint_id: String,
}

impl RemoveBreakpointTool {
    pub async fn handle(
        params: CallToolRequestParams,
        handler: &McpHandler,
    ) -> Result<CallToolResult, CallToolError> {
        let args: RemoveBreakpointTool = crate::handler::parse_args(params)?;
        let target = crate::handler::resolve_target(handler, args.instance_id, args.tab_id).await?;
        let params = json!({ "breakpointId": args.breakpoint_id });
        let failure = "Failed to remove breakpoint";
        let resp =
            crate::handler::send_or(&target, "Debugger.removeBreakpoint", params, failure).await?;
        Ok(CallToolResult::text_content(vec![
            crate::handler::reply_text("Breakpoint removed.", &resp).into(),
        ]))
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
    async fn test_remove_breakpoint_missing_id_fails_deserialization() {
        let handler = McpHandler::new_test();

        let params: CallToolRequestParams = serde_json::from_value(json!({
            "name": "remove_breakpoint",
            "arguments": {}
        }))
        .unwrap();
        let result = RemoveBreakpointTool::handle(params, &handler).await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("missing field `breakpoint_id`")
        );
    }

    #[tokio::test]
    async fn test_remove_breakpoint_tool_deserialization() {
        let tool: Result<RemoveBreakpointTool, _> = serde_json::from_value(json!({
            "breakpoint_id": "bp-123"
        }));
        assert!(tool.is_ok());
        assert_eq!(tool.unwrap().breakpoint_id, "bp-123");
    }

    #[tokio::test]
    async fn test_remove_breakpoint_handle() {
        let port = spawn_mock_chrome_server().await;

        let mut handler = McpHandler::new_test();
        Arc::get_mut(&mut handler.default_session)
            .unwrap()
            .chrome_manager = Arc::new(Mutex::new(MockChromeManager::new(port)));

        let params: CallToolRequestParams = serde_json::from_value(json!({
            "name": "remove_breakpoint",
            "arguments": {
                "breakpoint_id": "mock_bp_id_1"
            }
        }))
        .unwrap();

        let result = RemoveBreakpointTool::handle(params, &handler).await;
        assert!(result.is_ok(), "Handle should succeed: {:?}", result.err());

        let call_result = result.unwrap();
        assert!(!call_result.content.is_empty());
        let content_str = format!("{:?}", call_result.content);
        assert!(
            content_str.contains("Breakpoint removed"),
            "Content didn't match: {}",
            content_str
        );
    }
}
