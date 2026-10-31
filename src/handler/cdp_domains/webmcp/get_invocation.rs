use crate::handler::McpHandler;
use rust_mcp_sdk::{
    macros,
    schema::{CallToolError, CallToolRequestParams, CallToolResult},
};

#[macros::mcp_tool(
    name = "webmcp_get_invocation",
    description = "Returns a WebMCP invocation's status (Pending, Completed, Error, Canceled) and output, e.g. to poll one waiting for the user's consent on the page. Chrome only (not available on Firefox)."
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct GetWebmcpInvocationTool {
    /// Instance id; omit for the default instance.
    pub instance_id: Option<String>,
    /// Tab id; omit for the active tab.
    pub tab_id: Option<String>,
    /// Id from webmcp_invoke_tool.
    #[serde(rename = "invocationId")]
    pub invocation_id: String,
}

impl GetWebmcpInvocationTool {
    pub async fn handle(
        params: CallToolRequestParams,
        handler: &McpHandler,
    ) -> Result<CallToolResult, CallToolError> {
        let tool: GetWebmcpInvocationTool = serde_json::from_value(serde_json::Value::Object(
            params.arguments.unwrap_or_default(),
        ))
        .map_err(|e| CallToolError::from_message(format!("Failed to parse arguments: {}", e)))?;

        let session = handler.session(tool.instance_id.clone()).await?;
        let webmcp_state = session.webmcp_state(tool.tab_id.clone())?;
        let st = webmcp_state.lock().await;
        let invocation = st.invocations.get(&tool.invocation_id).ok_or_else(|| {
            CallToolError::from_message(format!(
                "No WebMCP invocation found with id '{}'. The invocation may predate the current Chrome session, or the id is wrong. Use webmcp_list_invocations to see the known invocations.",
                tool.invocation_id
            ))
        })?;

        let status = invocation
            .status
            .clone()
            .unwrap_or_else(|| "Pending".to_string());

        let mut result = serde_json::json!({
            "invocationId": invocation.invocation_id,
            "toolName": invocation.tool_name,
            "frameId": invocation.frame_id,
            "input": invocation.input,
            "status": status,
        });

        if let Some(output) = &invocation.output {
            result["output"] = output.clone();
        }
        if let Some(error_text) = &invocation.error_text {
            result["errorText"] = serde_json::Value::String(error_text.clone());
        }
        if invocation.untrusted_content {
            result["untrustedContent"] = serde_json::Value::Bool(true);
        }

        let content = serde_json::to_string_pretty(&result)
            .map_err(|e| CallToolError::from_message(format!("Failed to serialize: {}", e)))?;
        Ok(CallToolResult::text_content(vec![content.into()]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::cdp_domains::webmcp::WebmcpInvocation;
    use serde_json::json;

    async fn make_handler_with_invocation(
        status: Option<&str>,
        output: Option<serde_json::Value>,
    ) -> McpHandler {
        let handler = McpHandler::new_test();
        {
            let mut st = handler.default_session.webmcp_state.lock().await;
            st.invocations.insert(
                "inv-1".to_string(),
                WebmcpInvocation {
                    tool_name: "myTool".to_string(),
                    frame_id: "frame-1".to_string(),
                    invocation_id: "inv-1".to_string(),
                    input: "{}".to_string(),
                    status: status.map(String::from),
                    output,
                    error_text: None,
                    ..Default::default()
                },
            );
        }
        handler
    }

    fn extract_text(result: &CallToolResult) -> String {
        let content_val = serde_json::to_value(&result.content).unwrap();
        content_val[0]["text"].as_str().unwrap().to_string()
    }

    #[tokio::test]
    async fn test_get_invocation_pending() {
        let handler = make_handler_with_invocation(None, None).await;
        let params: CallToolRequestParams = serde_json::from_value(json!({
            "name": "webmcp_get_invocation",
            "arguments": { "instance_id": "default", "invocationId": "inv-1" }
        }))
        .unwrap();

        let result = GetWebmcpInvocationTool::handle(params, &handler)
            .await
            .unwrap();
        let payload: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
        assert_eq!(payload["status"], "Pending");
        assert_eq!(payload["invocationId"], "inv-1");
        assert!(payload.get("output").is_none());
    }

    #[tokio::test]
    async fn test_get_invocation_untrusted_output() {
        // AUD-RW-17: an untrusted tool's output is flagged here too.
        let handler =
            make_handler_with_invocation(Some("Completed"), Some(json!({"ok": true}))).await;
        if let Some(inv) = handler
            .default_session
            .webmcp_state
            .lock()
            .await
            .invocations
            .get_mut("inv-1")
        {
            inv.untrusted_content = true;
        }
        let params: CallToolRequestParams = serde_json::from_value(json!({
            "name": "webmcp_get_invocation",
            "arguments": { "invocationId": "inv-1" }
        }))
        .unwrap();
        let result = GetWebmcpInvocationTool::handle(params, &handler)
            .await
            .unwrap();
        let payload: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
        assert_eq!(payload["untrustedContent"], true);
    }

    #[tokio::test]
    async fn test_get_invocation_completed() {
        let handler =
            make_handler_with_invocation(Some("Completed"), Some(json!({"ok": true}))).await;
        let params: CallToolRequestParams = serde_json::from_value(json!({
            "name": "webmcp_get_invocation",
            "arguments": { "instance_id": "default", "invocationId": "inv-1" }
        }))
        .unwrap();

        let result = GetWebmcpInvocationTool::handle(params, &handler)
            .await
            .unwrap();
        let payload: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
        assert_eq!(payload["status"], "Completed");
        assert_eq!(payload["output"]["ok"], true);
    }

    #[tokio::test]
    async fn test_get_invocation_unknown_id() {
        let handler = McpHandler::new_test();
        let params: CallToolRequestParams = serde_json::from_value(json!({
            "name": "webmcp_get_invocation",
            "arguments": { "instance_id": "default", "invocationId": "nope" }
        }))
        .unwrap();

        let err = GetWebmcpInvocationTool::handle(params, &handler)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("No WebMCP invocation found"));
    }

    #[test]
    fn test_get_invocation_schema_no_unknown_types() {
        let schema = GetWebmcpInvocationTool::json_schema();
        let serialized = serde_json::to_string(&schema).unwrap();
        assert!(
            !serialized.contains("\"unknown\""),
            "schema must not contain type=unknown: {}",
            serialized
        );
    }
}
