use crate::handler::McpHandler;
use rust_mcp_sdk::{
    macros,
    schema::{CallToolError, CallToolRequestParams, CallToolResult},
};

#[macros::mcp_tool(
    name = "open_tab",
    description = "Opens a new tab in the specified Chrome instance. Side effects: creates a browser tab and registers it under a generated Tab ID; the tab stays open until closed with 'close_tab'. Prerequisites: none - the instance is launched lazily if not running yet. Returns: structured JSON with 'tab_id' (use it as the 'tab_id' argument of other tools), 'target_id' (raw CDP target id), 'url' and 'label'. Use this to work with several pages in parallel while keeping their CDP state isolated per tab. Alternatives: 'navigate' to load content in the active tab, 'list_tabs' to enumerate already open tabs."
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct OpenTabTool {
    /// Chrome instance id from open_instance/list_instances. Omit for the default instance.
    pub instance_id: Option<String>,
    /// Target URL to navigate to in the new tab. Defaults to 'about:blank'.
    pub url: Option<String>,
    /// Optional unique label to identify the tab. Rejected if another tab in this instance already uses it.
    pub label: Option<String>,
}

/// The page a new tab is created on.
const BLANK: &str = "about:blank";

/// The URL to navigate a freshly created (blank) tab to, if any.
fn deferred_navigation(url: &str) -> Option<&str> {
    (url != BLANK).then_some(url)
}

/// Navigates a registered blank tab to `url`, through the registered
/// session: the target-discovery listener may have registered this page
/// first, with its own session, and still be enabling its domains. Chrome
/// answers a session's commands in order, so once this (idempotent)
/// `Network.enable` replies, the page's first requests will be recorded.
async fn open_url(
    session: &crate::handler::BrowserSession,
    tab_id: &str,
    url: &str,
) -> Result<(), CallToolError> {
    let registered = session.target(Some(tab_id.to_string())).await?;
    let _ = registered
        .send_raw_command("Network.enable", serde_json::json!({}))
        .await;
    registered
        .send_raw_command("Page.navigate", serde_json::json!({ "url": url }))
        .await
        .map_err(|e| CallToolError::from_message(format!("Failed to open {url}: {e:?}")))?;
    Ok(())
}

impl OpenTabTool {
    pub async fn handle(
        params: CallToolRequestParams,
        handler: &McpHandler,
    ) -> Result<CallToolResult, CallToolError> {
        let args: OpenTabTool = crate::handler::parse_args(params)?;
        let session = handler.session(args.instance_id.clone()).await?;

        // Ensure the instance is running
        {
            let mut manager = session.chrome_manager.lock().await;
            manager.ensure_instance().await.map_err(|e| {
                CallToolError::from_message(format!("Failed to ensure Chrome instance: {}", e))
            })?;
        }

        let url = args.url.clone().unwrap_or_else(|| BLANK.to_string());
        if handler.flags.local && !crate::handler::is_local_address(&url) {
            return Err(CallToolError::from_message(format!(
                "Navigation to '{}' is blocked by local-only restrictions.",
                url
            )));
        }

        // Create the tab using the browser-level connection (browser_client)
        let browser_client = session.browser_client().await?;

        // The tab starts blank and navigates only once its listeners are
        // enabled: created straight on `url`, the page's first requests
        // (and their network events) were already gone.
        let tab = browser_client
            .new_tab(BLANK)
            .await
            .map_err(|e| CallToolError::from_message(format!("Failed to create new tab: {}", e)))?;

        let target_id = tab.target_id().to_string();

        let info = super::tab_registry::NewTab::new(args.label.clone(), url.clone());
        let tab_id = super::tab_lifecycle::register_and_listen(
            &session.tabs,
            &tab,
            info,
            session.enable_webmcp,
        )
        .await
        .map_err(|e| {
            CallToolError::from_message(format!("Failed to register tab in registry: {}", e))
        })?;
        if let Some(url) = deferred_navigation(&url) {
            open_url(&session, &tab_id, url).await?;
        }

        Ok(CallToolResult::text_content(vec![
            serde_json::json!({
                "tab_id": tab_id,
                "target_id": target_id,
                "url": url,
                "label": args.label,
            })
            .to_string()
            .into(),
        ]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// TAB-OPEN1: `open_tab` with a URL creates a blank tab and navigates it
    /// after its listeners are enabled, so the first requests are captured.
    #[test]
    fn open_tab_navigates_only_after_creating_a_blank_tab() {
        // Given a URL and the default blank page
        // When the deferred navigation is computed
        // Then only the real URL is navigated to
        assert_eq!(
            deferred_navigation("http://127.0.0.1/"),
            Some("http://127.0.0.1/")
        );
        assert_eq!(deferred_navigation(BLANK), None);
    }
}
