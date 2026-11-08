use crate::handler::McpHandler;
use crate::handler::debugging::{self, Step};
use rust_mcp_sdk::{
    macros,
    schema::{CallToolError, CallToolRequestParams, CallToolResult},
};

#[macros::mcp_tool(
    name = "step_over",
    description = "Runs the paused line, stepping over calls. Returns {paused, location: {url, line_number, column_number} | null} (0-based), waiting up to 2 s for the next pause."
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct StepOverTool {
    /// Default: 'default'.
    pub instance_id: Option<String>,
    /// Default: the active tab.
    pub tab_id: Option<String>,
}

impl StepOverTool {
    pub async fn handle(
        params: CallToolRequestParams,
        handler: &McpHandler,
    ) -> Result<CallToolResult, CallToolError> {
        step(params, handler, Step::Over).await
    }
}

#[macros::mcp_tool(
    name = "step_into",
    description = "Enters the call on the paused line (as step_over when it calls nothing), waiting up to 2 s for the next pause; returns as step_over does."
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct StepIntoTool {
    /// Default: 'default'.
    pub instance_id: Option<String>,
    /// Default: the active tab.
    pub tab_id: Option<String>,
}

impl StepIntoTool {
    pub async fn handle(
        params: CallToolRequestParams,
        handler: &McpHandler,
    ) -> Result<CallToolResult, CallToolError> {
        step(params, handler, Step::Into).await
    }
}

#[macros::mcp_tool(
    name = "step_out",
    description = "Runs to the end of the paused function and stops in its caller, waiting up to 2 s; returns as step_over does (paused false when no caller stops it)."
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct StepOutTool {
    /// Default: 'default'.
    pub instance_id: Option<String>,
    /// Default: the active tab.
    pub tab_id: Option<String>,
}

impl StepOutTool {
    pub async fn handle(
        params: CallToolRequestParams,
        handler: &McpHandler,
    ) -> Result<CallToolResult, CallToolError> {
        step(params, handler, Step::Out).await
    }
}

/// Steps the paused thread of the tab `params` addresses (in the frame of
/// another origin that paused, if it was one), then reports where it
/// stopped next.
async fn step(
    params: CallToolRequestParams,
    handler: &McpHandler,
    step: Step,
) -> Result<CallToolResult, CallToolError> {
    let ids: crate::handler::TargetArgs = crate::handler::parse_args(params)?;
    let session = handler.session(ids.instance_id.clone()).await?;
    let state = session.debugger_state(ids.tab_id.clone())?;
    if state.lock().await.paused_call_frame_id.is_none() {
        return Err(CallToolError::from_message(debugging::NOT_PAUSED));
    }
    let target = session.target(ids.tab_id.clone()).await?;
    let (paused, recorded, mut events) = super::frames::paused_with_events(&target, &state).await;
    let params = serde_json::json!({});
    let failure = format!("Failed to run {}", step.cdp_method());
    // As text: the error type is not `Send` across the await below.
    let sent = crate::handler::send_or(&paused, step.cdp_method(), params, &failure)
        .await
        .map_err(|e| e.to_string());
    if let Err(e) = sent {
        let message = match super::forget_refused_pause(&e, &state).await {
            true => debugging::NOT_PAUSED.to_string(),
            false => e,
        };
        return Err(CallToolError::from_message(message));
    }
    let watch = (debugging::STEP_WAIT, recorded);
    let location = super::wait_for_pause(&mut events, watch, &state).await;
    Ok(debugging::stepped(step, location.as_ref()))
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

    /// ISSUE-67 (Chrome): step_into and step_out send their command and
    /// report where the thread stopped next.
    #[tokio::test]
    async fn step_into_and_out_report_the_next_pause() {
        use crate::handler::cdp_domains::tests::{
            call, fire_after, mock_handler, reply_text, spawn_recording_mock,
        };
        for (name, method, first) in [
            ("step_into", "Debugger.stepInto", "Stepped into the call"),
            ("step_out", "Debugger.stepOut", "Stepped out to the caller"),
        ] {
            let mock = spawn_recording_mock().await;
            let handler = mock_handler(mock.http_port);
            let session = handler.session(None).await.unwrap();
            let state = session.debugger_state(None).unwrap();
            state.lock().await.paused_call_frame_id = Some("f1".into());
            let paused = json!({"reason": "other", "callFrames": [{"callFrameId": "f2", "url": "http://a/app.js", "location": {"scriptId": "7", "lineNumber": 1, "columnNumber": 4}}]});
            fire_after(mock, method, "Debugger.paused", paused);
            let params = call(name, json!({}));
            let reply = match name {
                "step_into" => StepIntoTool::handle(params, &handler).await,
                _ => StepOutTool::handle(params, &handler).await,
            };
            let text = reply_text(&reply.unwrap());
            assert!(text.starts_with(first), "{text}");
            assert!(text.contains("\"line_number\": 1"), "{text}");
            assert_eq!(mock.commands_named(method).len(), 1);
        }
    }

    /// ISSUE-83 (Chrome): a kept pause Chrome no longer has (it refuses the
    /// step or the resume as not paused) is forgotten: steps fail with the
    /// usual "not paused" message, not Chrome's raw error, resume answers
    /// that nothing was paused, and the tab no longer counts as paused.
    #[tokio::test]
    async fn a_pause_chrome_no_longer_has_is_forgotten() {
        use crate::handler::cdp_domains::tests::{
            call, mock_handler, reply_text, spawn_recording_mock,
        };
        let refusal = |_: &serde_json::Value| json!({"error": {"code": -32000, "message": "Can only perform operation while paused."}});
        let mock = spawn_recording_mock().await;
        mock.respond_with("Debugger.stepOver", refusal);
        mock.respond_with("Debugger.resume", refusal);
        let handler = mock_handler(mock.http_port);
        let session = handler.session(None).await.unwrap();
        let state = session.debugger_state(None).unwrap();
        for name in ["step_over", "resume"] {
            // Given a kept pause Chrome does not have
            state.lock().await.paused_call_frame_id = Some("f1".into());
            // When the tool runs
            let params = call(name, json!({}));
            let reply = match name {
                "step_over" => StepOverTool::handle(params, &handler).await,
                _ => super::super::resume::ResumeTool::handle(params, &handler).await,
            };
            // Then it says nothing is paused
            let text = match reply {
                Ok(result) => reply_text(&result),
                Err(e) => e.to_string(),
            };
            let expected = match name {
                "step_over" => debugging::NOT_PAUSED,
                _ => "The debugger was not paused",
            };
            assert!(text.starts_with(expected), "{name}: {text}");
            // And the pause is forgotten
            assert!(state.lock().await.paused_call_frame_id.is_none(), "{name}");
        }
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
        // (Chrome leaves the frame's URL empty: it comes from the parsed script)
        let parsed = json!({"scriptId": "7", "hash": "h", "url": "http://a/app.js"});
        let state = session.debugger_state(None).unwrap();
        super::super::process_debugger_event(
            &crate::handler::cdp_domains::tests::make_event("Debugger.scriptParsed", parsed),
            &state,
        )
        .await;
        let paused = json!({"reason": "other", "callFrames": [{"callFrameId": "f2", "url": "", "location": {"scriptId": "7", "lineNumber": 4, "columnNumber": 2}}]});
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
            json!({"paused": true, "location": {"url": "http://a/app.js", "script_id": "7", "line_number": 4, "column_number": 2}})
        );
    }
}
