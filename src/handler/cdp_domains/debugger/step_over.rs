use crate::handler::McpHandler;
use crate::handler::debugging;
use rust_mcp_sdk::{
    macros,
    schema::{CallToolError, CallToolRequestParams, CallToolResult},
};

#[macros::mcp_tool(
    name = "step_over",
    description = "Executes the current line of code without entering function calls, pausing at the next line. Side effects: advances debugger execution state. Prerequisites: requires an active, paused debugger session. Returns: JSON {paused, location: {url, line_number, column_number} (0-based) | null} where the thread stopped next (waits up to 2 s). Use this to skip function internals during debugging. Alternatives: use 'resume' to continue full execution, or 'evaluate_on_call_frame' to inspect state without stepping."
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct StepOverTool {
    /// Chrome instance id from open_instance/list_instances. Omit for the default instance.
    pub instance_id: Option<String>,
    /// The Tab ID of the target tab. Omit to use the active tab.
    pub tab_id: Option<String>,
}

impl StepOverTool {
    pub async fn handle(
        params: CallToolRequestParams,
        handler: &McpHandler,
    ) -> Result<CallToolResult, CallToolError> {
        let ids: crate::handler::TargetArgs = crate::handler::parse_args(params)?;
        let session = handler.session(ids.instance_id.clone()).await?;
        let state = session.debugger_state(ids.tab_id.clone())?;
        if state.lock().await.paused_call_frame_id.is_none() {
            return Err(CallToolError::from_message(debugging::NOT_PAUSED));
        }
        let target = session.target(ids.tab_id.clone()).await?;
        // Subscribed first: the new pause can arrive before the reply.
        let mut events = target.on_domain("Debugger");
        let params = serde_json::json!({});
        let failure = "Failed to step over";
        crate::handler::send_or(&target, "Debugger.stepOver", params, failure).await?;
        let location = super::wait_for_pause(&mut events, debugging::STEP_WAIT).await;
        Ok(debugging::stepped(location.as_ref()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_mcp_sdk::schema::CallToolRequestParams;
    use serde_json::json;

    #[tokio::test]
    async fn test_step_over_params_deserialization() {
        let params: Result<CallToolRequestParams, _> = serde_json::from_value(json!({
            "name": "step_over",
            "arguments": {}
        }));
        assert!(params.is_ok());
    }

    #[tokio::test]
    async fn test_step_over_tool_deserialization() {
        let tool: Result<StepOverTool, _> = serde_json::from_value(json!({}));
        assert!(tool.is_ok());
    }

    /// DBG-03 (Chrome): step_over reports where the thread stopped next,
    /// 0-based; without a pause it fails like on Firefox.
    #[tokio::test]
    async fn step_over_reports_the_next_pause() {
        use crate::handler::cdp_domains::tests::{
            call, fire_after, mock_handler, reply_text, spawn_recording_mock,
        };
        // Given a Chrome that is not paused
        let mock = spawn_recording_mock().await;
        let handler = mock_handler(mock.http_port);
        let error = StepOverTool::handle(call("step_over", json!({})), &handler)
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), debugging::NOT_PAUSED);
        // When it is paused and step_over runs
        let session = handler.session(None).await.unwrap();
        session
            .debugger_state(None)
            .unwrap()
            .lock()
            .await
            .paused_call_frame_id = Some("f1".into());
        let paused = json!({"reason": "other", "callFrames": [{"callFrameId": "f2", "url": "http://a/app.js", "location": {"scriptId": "7", "lineNumber": 4, "columnNumber": 2}}]});
        fire_after(mock, "Debugger.stepOver", "Debugger.paused", paused);
        let reply = StepOverTool::handle(call("step_over", json!({})), &handler)
            .await
            .unwrap();
        // Then it reports the new location
        let text = reply_text(&reply);
        let body: serde_json::Value =
            serde_json::from_str(&text[text.find('{').unwrap()..]).unwrap();
        assert_eq!(
            body,
            json!({"paused": true, "location": {"url": "http://a/app.js", "line_number": 4, "column_number": 2}})
        );
    }
}
