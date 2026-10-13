use crate::handler::McpHandler;
use crate::handler::debugging;
use rust_mcp_sdk::{
    macros,
    schema::{CallToolError, CallToolRequestParams, CallToolResult},
};

#[macros::mcp_tool(
    name = "resume",
    description = "Continues full execution of the debugger from current breakpoint. Side effects: advances code execution until next breakpoint or completion. Prerequisites: requires an active, paused debugger session. Returns: JSON {paused, location} like step_over: paused is true only if the page stopped again right away (within 250 ms, e.g. another breakpoint). Use this to continue program flow after inspection. Alternatives: 'step_over' for single-step execution."
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct ResumeTool {
    /// Chrome instance id from open_instance/list_instances. Omit for the default instance.
    pub instance_id: Option<String>,
    /// The Tab ID of the target tab. Omit to use the active tab.
    pub tab_id: Option<String>,
}

impl ResumeTool {
    pub async fn handle(
        params: CallToolRequestParams,
        handler: &McpHandler,
    ) -> Result<CallToolResult, CallToolError> {
        let ids: crate::handler::TargetArgs = crate::handler::parse_args(params)?;
        let session = handler.session(ids.instance_id.clone()).await?;
        let debugger_state = session.debugger_state(ids.tab_id.clone())?;
        let pause_script = debugger_state.lock().await.pause_on_load_script.take();
        let target = session.target(ids.tab_id.clone()).await?;
        // Subscribed first: an immediate new pause can arrive before the reply.
        let mut events = target.on_domain("Debugger");
        let params = serde_json::json!({});
        let failure = "Failed to resume execution";
        crate::handler::send_or(&target, "Debugger.resume", params, failure).await?;
        if let Some(identifier) = pause_script {
            let params = serde_json::json!({ "identifier": identifier });
            let _ = target
                .send_raw_command("Page.removeScriptToEvaluateOnNewDocument", params)
                .await;
        }
        let location = super::wait_for_pause(&mut events, debugging::RESUME_WAIT).await;
        Ok(debugging::resumed(location.as_ref()))
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

    #[tokio::test]
    async fn test_resume_params_deserialization() {
        // Valid params should deserialize without error
        let params: Result<CallToolRequestParams, _> = serde_json::from_value(json!({
            "name": "resume",
            "arguments": {}
        }));
        assert!(params.is_ok());
    }

    #[tokio::test]
    async fn test_resume_tool_deserialization() {
        // ResumeTool has no fields, so empty object should work
        let tool: Result<ResumeTool, _> = serde_json::from_value(json!({}));
        assert!(tool.is_ok());
    }

    #[tokio::test]
    async fn test_resume_handle() {
        let port = spawn_mock_chrome_server().await;

        let mut handler = McpHandler::new_test();
        Arc::get_mut(&mut handler.default_session)
            .unwrap()
            .chrome_manager = Arc::new(Mutex::new(MockChromeManager::new(port)));

        let params: CallToolRequestParams = serde_json::from_value(json!({
            "name": "resume",
            "arguments": {}
        }))
        .unwrap();

        let result = ResumeTool::handle(params, &handler).await;
        assert!(result.is_ok(), "Handle should succeed: {:?}", result.err());

        let call_result = result.unwrap();
        assert!(!call_result.content.is_empty());
        let content_str = format!("{:?}", call_result.content);
        assert!(
            content_str.contains("Debugger execution resumed"),
            "Content didn't match: {}",
            content_str
        );
    }

    /// FIX-03: pause_on_load pauses once: resume forgets its script.
    #[tokio::test]
    async fn pause_on_load_script_is_removed() {
        // Given a pause_on_load script recorded for the session page
        let port = spawn_mock_chrome_server().await;
        let mut handler = McpHandler::new_test();
        Arc::get_mut(&mut handler.default_session)
            .unwrap()
            .chrome_manager = Arc::new(Mutex::new(MockChromeManager::new(port)));
        let session = handler.session(None).await.unwrap();
        session.debugger_state.lock().await.pause_on_load_script = Some("1".into());
        // When the agent resumes
        let params: CallToolRequestParams =
            serde_json::from_value(json!({"name": "resume", "arguments": {}})).unwrap();
        ResumeTool::handle(params, &handler).await.unwrap();
        // Then the script is forgotten (its removal was sent), so later navigations do not pause
        assert!(
            session
                .debugger_state
                .lock()
                .await
                .pause_on_load_script
                .is_none()
        );
    }
}
