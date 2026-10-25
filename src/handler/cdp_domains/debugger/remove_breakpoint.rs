use crate::handler::McpHandler;
use rust_mcp_sdk::{
    macros,
    schema::{CallToolError, CallToolRequestParams, CallToolResult},
};
use serde_json::json;

#[macros::mcp_tool(
    name = "remove_breakpoint",
    description = "Removes a previously set debugger breakpoint by its ID, allowing execution to pass that location uninterrupted. Side effects: modifies debugger state (breakpoint deleted). Prerequisites: the breakpoint ID returned by 'set_breakpoint' (works whether or not execution is stopped). Returns: JSON {removed: breakpoint_id}; an unknown id fails listing the tab's breakpoint ids. Use this to clean up breakpoints or disable debugging at specific locations. Alternatives: 'set_breakpoint' to add new breakpoints, 'resume' to continue execution."
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
        let session = handler.session(args.instance_id.clone()).await?;
        let state = session.debugger_state(args.tab_id.clone())?;
        let native = state
            .lock()
            .await
            .breakpoints
            .get(&args.breakpoint_id)
            .map_err(CallToolError::from_message)?
            .clone();
        let target = session.target(args.tab_id.clone()).await?;
        let params = json!({ "breakpointId": native });
        let failure = "Failed to remove breakpoint";
        crate::handler::send_or(&target, "Debugger.removeBreakpoint", params, failure).await?;
        let paused = {
            let mut state = state.lock().await;
            state.breakpoints.remove(&args.breakpoint_id);
            state.paused_call_frame_id.is_some()
        };
        // While paused, skipping would also cancel the next step: `resume`
        // brings the setting up to date.
        if !paused {
            super::sync_pauses(&target, &state).await;
        }
        Ok(crate::handler::debugging::removed(&args.breakpoint_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_mcp_sdk::schema::CallToolRequestParams;

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

    /// REV-F11-9: a Chrome reply without a breakpoint id is an error, not
    /// an MCP id mapped to nothing.
    #[tokio::test]
    async fn breakpoint_without_cdp_id_fails() {
        use crate::handler::cdp_domains::debugger::set_breakpoint::SetBreakpointTool;
        use crate::handler::cdp_domains::tests::{call, mock_handler, spawn_recording_mock};
        let mock = spawn_recording_mock().await;
        let handler = mock_handler(mock.http_port);
        let args = json!({"url": "http://a/app.js", "line_number": 1});
        let error = SetBreakpointTool::handle(call("set_breakpoint", args), &handler)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("no breakpointId"), "{error}");
    }

    /// DBG-02 (Chrome): an unknown id fails before anything is sent.
    #[tokio::test]
    async fn unknown_breakpoint_id_fails() {
        use crate::handler::cdp_domains::tests::{call, mock_handler, spawn_recording_mock};
        let mock = spawn_recording_mock().await;
        let handler = mock_handler(mock.http_port);
        let args = json!({"breakpoint_id": "1:3:0:http://a/app.js"});
        let error = RemoveBreakpointTool::handle(call("remove_breakpoint", args), &handler)
            .await
            .unwrap_err();
        assert!(error.to_string().starts_with("No breakpoint"), "{error}");
        assert!(mock.commands_named("Debugger.removeBreakpoint").is_empty());
    }
}
