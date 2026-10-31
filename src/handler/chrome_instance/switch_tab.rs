use crate::handler::McpHandler;
use rust_mcp_sdk::{
    macros,
    schema::{CallToolError, CallToolRequestParams, CallToolResult},
};

#[macros::mcp_tool(
    name = "switch_tab",
    description = "Makes a tab the target of the tools that omit tab_id, and brings it to the foreground unless 'activate' is false."
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct SwitchTabTool {
    /// Instance id; omit for the default instance.
    pub instance_id: Option<String>,
    /// The Tab ID to switch to (e.g. 'tab-1').
    pub tab_id: String,
    /// Bring it to the foreground. Default: true.
    pub activate: Option<bool>,
}

impl SwitchTabTool {
    pub async fn handle(
        params: CallToolRequestParams,
        handler: &McpHandler,
    ) -> Result<CallToolResult, CallToolError> {
        let args_value = serde_json::Value::Object(params.arguments.unwrap_or_default());
        let args: SwitchTabTool = serde_json::from_value(args_value)
            .map_err(|e| CallToolError::from_message(e.to_string()))?;
        let session = handler.session(args.instance_id.clone()).await?;
        session.check_browser().await?;

        // 1. Look up the tab and set it as the active ID
        let tab = {
            let mut registry = crate::util::sync::write(&session.tabs);
            registry
                .switch_tab(&args.tab_id)
                .map_err(|e| CallToolError::from_message(format!("Failed to switch tab: {}", e)))?;
            registry
                .tabs
                .get(&args.tab_id)
                .map(|entry| entry.tab.clone())
                .ok_or_else(|| {
                    CallToolError::from_message(format!("Unknown tab id '{}'", args.tab_id))
                })?
        };

        // 2. Bring it to the foreground in Chrome if requested
        let activate = args.activate.unwrap_or(true);
        if activate {
            tab.activate().await.map_err(|e| {
                CallToolError::from_message(format!("Failed to activate tab in browser: {}", e))
            })?;
        }

        Ok(CallToolResult::text_content(vec![
            serde_json::json!({
                "active_tab_id": args.tab_id,
                "activated_in_browser": activate,
            })
            .to_string()
            .into(),
        ]))
    }
}
