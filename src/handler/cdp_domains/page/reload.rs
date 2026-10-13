use crate::handler::McpHandler;
use rust_mcp_sdk::{
    macros,
    schema::{CallToolError, CallToolRequestParams, CallToolResult},
};
use serde_json::json;

#[macros::mcp_tool(
    name = "reload",
    description = "Reloads the current page, discarding all unsaved changes and re-fetching resources from the server. Side effects: destructive of unsaved state; clears dynamic DOM state. Prerequisites: requires an active Chrome tab. Returns: JSON {url, tab_id, browser, notes?} once the page fired `load` (at most 30 s), the same shape as 'navigate'. Use this to refresh page content or reset to initial load state. Alternatives: 'navigate' to load a different URL, 'pause_on_load' to debug reload execution."
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
        let session = handler.session(ids.instance_id.clone()).await?;
        let target = session.target(ids.tab_id.clone()).await?;
        // Subscribed first: `load` can fire before the reply is read.
        let mut page_events = target.on_domain("Page");
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
        let loaded = super::navigate::wait_for_load(&mut page_events).await;
        let browser = resp.result.unwrap_or_else(|| json!({}));
        let outcome = super::navigate::finish(&target, "", browser, loaded).await;
        let tab_id = session.tab_id_of(ids.tab_id.as_deref());
        Ok(outcome.reply("Page reloaded.", tab_id.as_deref()))
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
    use crate::handler::cdp_domains::tests::{
        call, committed_url, fire_load_after, mock_handler, reply_text, spawn_recording_mock,
    };
    use rust_mcp_sdk::schema::CallToolRequestParams;
    use serde_json::json;

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

    /// NAV-05: reload answers the same shape as navigate, after `load`.
    #[tokio::test]
    async fn reload_reports_url_after_load() {
        // Given a Chrome showing /page
        let mock = spawn_recording_mock().await;
        committed_url(mock, "http://127.0.0.1:8/page");
        fire_load_after(mock, "Page.reload");
        let handler = mock_handler(mock.http_port);
        // When reload runs
        let reply = ReloadTool::handle(call("reload", json!({})), &handler)
            .await
            .unwrap();
        // Then it waited for load and reports the URL in the shared shape
        let text = reply_text(&reply);
        assert!(text.starts_with("Page reloaded."), "{text}");
        let body: serde_json::Value =
            serde_json::from_str(&text[text.find('{').unwrap()..]).unwrap();
        assert_eq!(body["url"], "http://127.0.0.1:8/page");
        assert!(body.get("tab_id").is_some() && body.get("browser").is_some());
        assert!(body.get("notes").is_none(), "load fired: {body}");
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
