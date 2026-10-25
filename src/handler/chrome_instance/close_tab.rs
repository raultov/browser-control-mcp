use crate::handler::McpHandler;
use rust_mcp_sdk::{
    macros,
    schema::{CallToolError, CallToolRequestParams, CallToolResult},
};

#[macros::mcp_tool(
    name = "close_tab",
    description = "Closes a specific tab by its registered Tab ID. Side effects: destructive - removes the tab from the registry and closes it in Chrome; if it was the active tab, another registered tab becomes active. Prerequisites: the tab must exist (see 'list_tabs'). Returns: structured JSON with the closed 'tab_id' and the new 'active_tab_id' (null when no tabs remain). Use this to clean up tabs you no longer need. Alternatives: 'close_instance' to stop a whole Chrome instance."
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct CloseTabTool {
    /// Chrome instance id from open_instance/list_instances. Omit for the default instance.
    pub instance_id: Option<String>,
    /// The Tab ID of the tab to close (e.g. 'tab-1').
    pub tab_id: String,
}

/// The error for a tab id the instance does not know, listing the known ones.
fn unknown_tab(session: &crate::handler::BrowserSession, tab_id: &str) -> CallToolError {
    let registry = crate::util::sync::read(&session.tabs);
    let available: Vec<_> = registry.tabs.keys().cloned().collect();
    let avail_str = if available.is_empty() {
        "none (if Chrome was restarted, previous tab IDs are no longer valid)".to_string()
    } else {
        available.join(", ")
    };
    CallToolError::from_message(format!(
        "Tab ID '{tab_id}' not found. Available tabs: {avail_str}. Use 'list_tabs' to discover current tab IDs."
    ))
}

impl CloseTabTool {
    pub async fn handle(
        params: CallToolRequestParams,
        handler: &McpHandler,
    ) -> Result<CallToolResult, CallToolError> {
        let args_value = serde_json::Value::Object(params.arguments.unwrap_or_default());
        let args: CloseTabTool = serde_json::from_value(args_value)
            .map_err(|e| CallToolError::from_message(e.to_string()))?;
        let session = handler.session(args.instance_id.clone()).await?;
        session.check_browser().await?;

        let target_id = {
            let registry = crate::util::sync::read(&session.tabs);
            registry
                .tabs
                .get(&args.tab_id)
                .map(|e| e.tab.target_id().to_string())
        };
        let Some(target_id) = target_id else {
            return Err(unknown_tab(&session, &args.tab_id));
        };

        // Forgotten only once the browser closed it: a tab whose close
        // failed stays reachable.
        let browser_client = session.browser_client().await?;
        browser_client.close_tab(&target_id).await.map_err(|e| {
            CallToolError::from_message(format!("Failed to close tab on browser: {}", e))
        })?;
        crate::util::sync::write(&session.tabs).remove_tab(&args.tab_id);

        let new_active = crate::util::sync::read(&session.tabs).active_tab_id.clone();

        Ok(CallToolResult::text_content(vec![
            serde_json::json!({
                "closed_tab_id": args.tab_id,
                "active_tab_id": new_active,
            })
            .to_string()
            .into(),
        ]))
    }
}
