use crate::handler::McpHandler;
use rust_mcp_sdk::{
    macros,
    schema::{CallToolError, CallToolRequestParams, CallToolResult},
};

#[macros::mcp_tool(
    name = "close_tab",
    description = "Closes a tab (default: the active one); the most recently active open tab (else the newest) becomes active and is brought to the front. Returns {closed_tab_id, active_tab_id}."
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct CloseTabTool {
    /// Default: 'default'.
    pub instance_id: Option<String>,
    /// Default: the active tab (ids look like 'tab-1').
    pub tab_id: Option<String>,
}

impl CloseTabTool {
    /// The tab to close: the one named, or the active one.
    pub(crate) fn tab_to_close(
        &self,
        registry: &crate::handler::chrome_instance::tab_registry::TabRegistry,
    ) -> Result<String, String> {
        self.tab_id
            .clone()
            .or_else(|| registry.active_tab_id.clone())
            .ok_or_else(|| NO_TAB.to_string())
    }
}

/// How long `close_tab` waits for the browser to stop listing the page.
#[cfg(not(test))]
const GONE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
#[cfg(test)]
const GONE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(200);

/// Waits (at most [`GONE_TIMEOUT`], a slow check included; checking every
/// 20 ms) until `still_listed` says the closed page is gone, or cannot tell
/// (`None`).
async fn until_gone<F, Fut>(mut still_listed: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<bool>>,
{
    let deadline = tokio::time::Instant::now() + GONE_TIMEOUT;
    // `timeout_at` polls the check first: a check that answers at once is
    // stopped by the deadline test, one that never answers by the timeout.
    while tokio::time::Instant::now() < deadline
        && tokio::time::timeout_at(deadline, still_listed()).await == Ok(Some(true))
    {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// Why `close_tab` without `tab_id` closed nothing.
pub(crate) const NO_TAB: &str = "There is no tab to close (no tab_id given and no active tab).";

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

        let (tab_id, target_id) = {
            let registry = crate::util::sync::read(&session.tabs);
            let tab_id = args
                .tab_to_close(&registry)
                .map_err(CallToolError::from_message)?;
            let target = registry
                .tabs
                .get(&tab_id)
                .map(|e| e.tab.target_id().to_string());
            (tab_id, target)
        };
        let Some(target_id) = target_id else {
            return Err(unknown_tab(&session, &tab_id));
        };

        // Forgotten only once the browser closed it: a tab whose close
        // failed stays reachable.
        let browser_client = session.browser_client().await?;
        browser_client.close_tab(&target_id).await.map_err(|e| {
            CallToolError::from_message(format!("Failed to close tab on browser: {}", e))
        })?;
        // Chrome answers before the page is gone: a tool called right after
        // would connect to the dying page (ISSUE-101).
        let still_listed = || async {
            let pages = browser_client.list_tabs().await.ok()?;
            Some(pages.iter().any(|page| page.target_id == target_id))
        };
        until_gone(still_listed).await;
        let new_active = super::tab_lifecycle::forget_tab(&session.tabs, &tab_id).await;

        Ok(CallToolResult::text_content(vec![
            serde_json::json!({
                "closed_tab_id": tab_id,
                "active_tab_id": new_active,
            })
            .to_string()
            .into(),
        ]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ISSUE-101 (Chrome): close_tab answers once the browser stops listing
    /// the page (Chrome answers Target.closeTarget before), giving up after
    /// a bound or when the browser cannot be asked.
    #[tokio::test]
    async fn closing_waits_until_the_page_is_gone() {
        // Given a page listed for three more checks
        let checks = std::sync::atomic::AtomicUsize::new(0);
        let listed = || async {
            let seen = checks.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Some(seen < 3)
        };
        // When close_tab waits for it
        until_gone(listed).await;
        // Then it asked until the page was gone
        assert_eq!(checks.load(std::sync::atomic::Ordering::SeqCst), 4);
        // A page that never goes, or a browser that cannot tell, ends it
        let started = tokio::time::Instant::now();
        until_gone(|| async { Some(true) }).await;
        assert!(started.elapsed() >= GONE_TIMEOUT);
        until_gone(|| async { None }).await;
        // A check that never answers does not outlast the bound
        let started = tokio::time::Instant::now();
        until_gone(std::future::pending::<Option<bool>>).await;
        assert!(started.elapsed() < GONE_TIMEOUT * 2);
    }

    /// ISSUE-69 (Chrome): close_tab without tab_id closes the active tab,
    /// as every tool omitting tab_id acts on it; with no tab it says so.
    #[test]
    fn tab_to_close_defaults_to_the_active_tab() {
        // Given a registry with tab-3 active, and an empty one
        let mut registry = crate::handler::chrome_instance::tab_registry::TabRegistry::new(8);
        let tool =
            |args: serde_json::Value| -> CloseTabTool { serde_json::from_value(args).unwrap() };
        // Then an explicit tab is kept, a missing one is the active tab
        registry.active_tab_id = Some("tab-3".into());
        let named = tool(serde_json::json!({"tab_id": "tab-1"})).tab_to_close(&registry);
        assert_eq!(named.unwrap(), "tab-1");
        let active = tool(serde_json::json!({})).tab_to_close(&registry);
        assert_eq!(active.unwrap(), "tab-3");
        registry.active_tab_id = None;
        let none = tool(serde_json::json!({}))
            .tab_to_close(&registry)
            .unwrap_err();
        assert!(none.contains("no tab to close"), "{none}");
    }
}
