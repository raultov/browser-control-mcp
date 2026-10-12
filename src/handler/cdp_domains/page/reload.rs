use crate::handler::McpHandler;
use rust_mcp_sdk::{
    macros,
    schema::{CallToolError, CallToolRequestParams, CallToolResult},
};
use serde_json::json;

#[macros::mcp_tool(
    name = "reload",
    description = "Reloads the current page, discarding all unsaved changes and re-fetching resources from the server. Side effects: destructive of unsaved state; clears dynamic DOM state. Prerequisites: requires an active Chrome tab. Returns: reload confirmation. Use this to refresh page content or reset to initial load state. Alternatives: 'navigate' to load a different URL, 'pause_on_load' to debug reload execution."
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct ReloadTool {
    /// Chrome instance id from open_instance/list_instances. Omit for the default instance.
    pub instance_id: Option<String>,
    /// The Tab ID of the target tab. Omit to use the active tab.
    pub tab_id: Option<String>,
}

impl ReloadTool {
    pub async fn handle(
        params: CallToolRequestParams,
        handler: &McpHandler,
    ) -> Result<CallToolResult, CallToolError> {
        let ids: crate::handler::TargetArgs = crate::handler::parse_args(params)?;
        let target = crate::handler::resolve_target(handler, ids.instance_id, ids.tab_id).await?;
        // Right after a navigation Chrome may still be committing the new
        // document and refuse with "Not attached to an active page": retry
        // once after a short pause.
        let mut reply = target.send_raw_command("Page.reload", json!({})).await;
        if reply
            .as_ref()
            .is_err_and(|e| is_transient(&format!("{e:?}")))
        {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            reply = target.send_raw_command("Page.reload", json!({})).await;
        }
        let resp = reply
            .map_err(|e| CallToolError::from_message(format!("Failed to reload page: {e:?}")))?;
        Ok(CallToolResult::text_content(vec![
            crate::handler::reply_text("Page reloaded.", &resp).into(),
        ]))
    }
}

/// Whether a `Page.reload` error is the transient one Chrome returns while
/// it is still committing a navigation, worth one retry.
fn is_transient(error: &str) -> bool {
    error.contains("Not attached to an active page")
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
    async fn test_reload_params_deserialization() {
        let params: Result<CallToolRequestParams, _> = serde_json::from_value(json!({
            "name": "reload",
            "arguments": {}
        }));
        assert!(params.is_ok());
    }

    #[tokio::test]
    async fn test_reload_tool_deserialization() {
        let tool: Result<ReloadTool, _> = serde_json::from_value(json!({}));
        assert!(tool.is_ok());
    }

    #[tokio::test]
    async fn test_reload_handle() {
        let port = spawn_mock_chrome_server().await;

        let mut handler = McpHandler::new_test();
        Arc::get_mut(&mut handler.default_session)
            .unwrap()
            .chrome_manager = Arc::new(Mutex::new(MockChromeManager::new(port)));

        let params: CallToolRequestParams = serde_json::from_value(json!({
            "name": "reload",
            "arguments": {}
        }))
        .unwrap();

        let result = ReloadTool::handle(params, &handler).await;
        assert!(result.is_ok(), "Handle should succeed: {:?}", result.err());

        let call_result = result.unwrap();
        assert!(!call_result.content.is_empty());
        let content_str = format!("{:?}", call_result.content);
        assert!(
            content_str.contains("Page reloaded"),
            "Content didn't match: {}",
            content_str
        );
    }

    /// PAG-RL1: reload right after navigate retries the transient
    /// "Not attached to an active page" error, and only that one.
    #[test]
    fn reload_retries_only_the_not_attached_error() {
        // Given the error Chrome returns mid-navigation and an unrelated one
        let transient = r#"Protocol { code: -32000, message: "Not attached to an active page" }"#;
        let other = r#"Protocol { code: -32601, message: "'Page.reload' wasn't found" }"#;
        // When / Then only the first is retried
        assert!(is_transient(transient));
        assert!(!is_transient(other));
    }
}
