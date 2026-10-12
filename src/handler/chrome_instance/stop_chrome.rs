use crate::handler::McpHandler;
use rust_mcp_sdk::{
    macros,
    schema::{CallToolError, CallToolRequestParams, CallToolResult},
};

#[macros::mcp_tool(
    name = "stop_browser",
    description = "Gracefully terminates the Chrome instance selected by 'instance_id' (the default instance when omitted) and closes all its debugging connections. Side effects: destructive - kills the Chrome process of that instance only; its open tabs are closed; unsaved data lost; other instances keep running. Prerequisites: the instance must be running. Returns: termination success confirmation. Use this to clean up resources, prevent zombie processes, or end a debugging session. Alternatives: 'restart_browser' to restart instead of stop, 'close_instance' to also remove a secondary instance from the registry."
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct StopChromeTool {
    /// Chrome instance id from open_instance/list_instances. Omit for the default instance.
    pub instance_id: Option<String>,
}

impl StopChromeTool {
    pub async fn handle(
        params: CallToolRequestParams,
        handler: &McpHandler,
    ) -> Result<CallToolResult, CallToolError> {
        let tool: StopChromeTool = serde_json::from_value(serde_json::Value::Object(
            params.arguments.unwrap_or_default(),
        ))
        .map_err(|e| CallToolError::from_message(format!("Failed to parse arguments: {}", e)))?;

        let session = handler.session(tool.instance_id.clone()).await?;
        // Before taking the manager: a first connection holds the client
        // slot while it waits for the manager (one lock order everywhere).
        session.reset_connection_state().await;
        let mut manager = session.chrome_manager.lock().await;

        if let Err(e) = manager.stop_instance().await {
            return Err(CallToolError::from_message(format!(
                "Failed to stop Chrome: {}",
                e
            )));
        }

        Ok(CallToolResult::text_content(vec![
            "Chrome instance stopped successfully.".into(),
        ]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::chrome_instance::MockChromeManager;
    use rust_mcp_sdk::schema::CallToolRequestParams;
    use serde_json::json;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    #[tokio::test]
    async fn test_stop_chrome_params_deserialization() {
        let params: Result<CallToolRequestParams, _> = serde_json::from_value(json!({
            "name": "stop_browser",
            "arguments": {}
        }));
        assert!(params.is_ok());
    }

    #[tokio::test]
    async fn test_stop_chrome_tool_deserialization() {
        let tool: Result<StopChromeTool, _> = serde_json::from_value(json!({}));
        assert!(tool.is_ok());
    }

    #[tokio::test]
    async fn test_stop_chrome_handle() {
        let mut handler = McpHandler::new_test();
        Arc::get_mut(&mut handler.default_session)
            .unwrap()
            .chrome_manager = Arc::new(Mutex::new(MockChromeManager::new(9999)));

        let params: CallToolRequestParams = serde_json::from_value(json!({
            "name": "stop_browser",
            "arguments": { "instance_id": "default" }
        }))
        .unwrap();

        let result = StopChromeTool::handle(params, &handler).await;
        assert!(result.is_ok(), "Handle should succeed: {:?}", result.err());

        let call_result = result.unwrap();
        assert!(!call_result.content.is_empty());
        let content_str = format!("{:?}", call_result.content);
        assert!(
            content_str.contains("Chrome instance stopped successfully"),
            "Content didn't match: {}",
            content_str
        );
    }

    /// REV-01: `stop_browser` takes the session locks in the same order as
    /// a tool's first connection (client slot, then manager), so the two
    /// cannot deadlock.
    #[tokio::test]
    async fn stop_while_a_tool_connects_does_not_deadlock() {
        // Given a tool holding the client slot and about to take the manager
        let mut handler = McpHandler::new_test();
        Arc::get_mut(&mut handler.default_session)
            .unwrap()
            .chrome_manager = Arc::new(Mutex::new(MockChromeManager::new(9999)));
        let session = handler.default_session.clone();
        let connecting = tokio::spawn(async move {
            let _client = session.client.lock().await;
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            let _manager = session.chrome_manager.lock().await;
        });
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        // When stop_browser runs meanwhile
        let params: CallToolRequestParams =
            serde_json::from_value(json!({"name": "stop_browser", "arguments": {}})).unwrap();
        let both = async {
            let stopped = StopChromeTool::handle(params, &handler).await;
            connecting.await.unwrap();
            stopped
        };
        // Then both finish
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(2), both).await;
        assert!(outcome.is_ok_and(|r| r.is_ok()), "deadlock");
    }
}
