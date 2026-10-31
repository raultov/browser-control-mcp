use crate::handler::McpHandler;
use rust_mcp_sdk::{
    macros,
    schema::{CallToolError, CallToolRequestParams, CallToolResult},
};

#[macros::mcp_tool(
    name = "close_instance",
    description = "Stops an instance: other instances are removed; 'default' is stopped and relaunched on its next use."
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct CloseInstanceTool {
    /// Instance id; omit for the default instance.
    pub instance_id: String,
}

impl CloseInstanceTool {
    pub async fn handle(
        params: CallToolRequestParams,
        handler: &McpHandler,
    ) -> Result<CallToolResult, CallToolError> {
        let args_value = serde_json::Value::Object(params.arguments.unwrap_or_default());
        let args: CloseInstanceTool = serde_json::from_value(args_value)
            .map_err(|e| CallToolError::from_message(e.to_string()))?;

        if args.instance_id == "default" {
            // Stop the default instance, but do not remove it from the registry
            let session = handler.default_session.clone();
            session.reset_connection_state().await;
            let mut mgr = session.chrome_manager.lock().await;
            mgr.stop_instance().await.map_err(|e| {
                CallToolError::from_message(format!("Failed to stop default instance: {}", e))
            })?;
            return Ok(CallToolResult::text_content(vec![serde_json::json!({
                "instance_id": "default",
                "stopped": true,
                "removed_from_registry": false,
                "note": "The default instance is stopped but kept; it is re-created lazily on the next tool call.",
            })
            .to_string()
            .into()]));
        }

        if let Some(session) = handler.registry.remove_session(&args.instance_id) {
            session.reset_connection_state().await;
            let mut mgr = session.chrome_manager.lock().await;
            mgr.stop_instance().await.map_err(|e| {
                CallToolError::from_message(format!("Failed to stop instance: {}", e))
            })?;
            Ok(CallToolResult::text_content(vec![
                serde_json::json!({
                    "instance_id": args.instance_id,
                    "stopped": true,
                    "removed_from_registry": true,
                })
                .to_string()
                .into(),
            ]))
        } else {
            let available: Vec<_> = handler
                .registry
                .list_descriptors()
                .into_iter()
                .map(|d| d.id)
                .collect();
            Err(CallToolError::from_message(format!(
                "Instance '{}' not found. Available instances: {}. Use 'list_instances' to discover instance IDs.",
                args.instance_id,
                available.join(", ")
            )))
        }
    }
}
