use crate::handler::McpHandler;
use rust_mcp_sdk::{
    macros,
    schema::{CallToolError, CallToolRequestParams, CallToolResult},
};

#[macros::mcp_tool(
    name = "list_tabs",
    description = "Lists the instance's tabs: {active_tab_id, tabs: [{tab_id, label, target_id, url}]}.",
    read_only_hint = true
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct ListTabsTool {
    /// Default: 'default'.
    pub instance_id: Option<String>,
}

impl ListTabsTool {
    pub async fn handle(
        params: CallToolRequestParams,
        handler: &McpHandler,
    ) -> Result<CallToolResult, CallToolError> {
        let args_value = serde_json::Value::Object(params.arguments.unwrap_or_default());
        let args: ListTabsTool = serde_json::from_value(args_value)
            .map_err(|e| CallToolError::from_message(e.to_string()))?;
        let session = handler.session(args.instance_id.clone()).await?;
        session.check_browser().await?;
        discover_tabs(&session).await?;
        // Current URLs from the browser, when it is running (the registry only
        // knows the URL a tab was opened with).
        let running = session.chrome_manager.lock().await.is_running().await;
        let mut current = std::collections::HashMap::new();
        // `.ok()` first: the error type is not `Send` across the next await.
        let browser = match running {
            true => session.browser_client().await.ok(),
            false => None,
        };
        if let Some(browser) = browser {
            current = current_urls(browser.list_tabs().await.unwrap_or_default());
        }
        let registry = crate::util::sync::read(&session.tabs);

        let mut entries: Vec<_> = registry.tabs.iter().collect();
        entries.sort_by_key(|(id, _)| super::tab_registry::tab_number(id));
        let tabs: Vec<serde_json::Value> = entries
            .into_iter()
            .map(|(id, entry)| {
                serde_json::json!({
                    "tab_id": id,
                    "label": entry.label,
                    "target_id": entry.tab.target_id(),
                    "url": current.get(entry.tab.target_id()).unwrap_or(&entry.url),
                })
            })
            .collect();

        let result = if tabs.is_empty() {
            serde_json::json!({
                "active_tab_id": serde_json::Value::Null,
                "tabs": tabs,
                "note": "No tabs registered in this session. Tools that omit 'tab_id' fall back to the instance's default single-tab connection.",
            })
        } else {
            serde_json::json!({
                "active_tab_id": registry.active_tab_id,
                "tabs": tabs,
            })
        };

        Ok(CallToolResult::text_content(vec![
            result.to_string().into(),
        ]))
    }
}

/// How long `list_tabs` waits for the pages Chrome announced to be
/// registered as tabs.
const DISCOVERY_WAIT: std::time::Duration = std::time::Duration::from_secs(3);

/// Connects a session no tool has used yet, launching its browser as any
/// other tool does, and waits for the browser's pages to be registered: a
/// fresh instance lists its start tab, as on Firefox (#127: it listed none
/// until another tool had connected). The session connects to a page: a
/// browser with none left is not connected to, and lists no tab.
async fn discover_tabs(session: &crate::handler::BrowserSession) -> Result<(), CallToolError> {
    let connected = session.client.lock().await.is_some();
    if !connected && session.page_count().await != Some(0) {
        drop(session.get_or_connect().await?);
    }
    super::tab_lifecycle::registrations_done(&session.tabs, DISCOVERY_WAIT).await;
    Ok(())
}

/// Target id → current URL.
fn current_urls(
    targets: Vec<cdp_browser_lite::TargetInfo>,
) -> std::collections::HashMap<String, String> {
    targets.into_iter().map(|t| (t.target_id, t.url)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::cdp_domains::tests::{
        call, fire_events_after, mock_handler, reply_text, spawn_recording_mock,
    };
    use serde_json::json;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    type MockDevTools = cdp_browser_lite::test_support::mock_devtools::MockDevTools;

    /// A handler whose default Chrome instance no tool has used yet: `mock`
    /// is its browser, showing `pages`, and it saves downloads in `downloads`.
    fn fresh_instance(
        mock: &'static MockDevTools,
        pages: serde_json::Value,
        downloads: &std::path::Path,
    ) -> McpHandler {
        mock.respond_with(
            "Target.getTargets",
            move |_| json!({"targetInfos": pages.clone()}),
        );
        let mut handler = mock_handler(mock.http_port);
        let session = Arc::get_mut(&mut handler.default_session).unwrap();
        let manager = super::super::MockChromeManager::new(mock.http_port).with_browser_client();
        session.chrome_manager = Arc::new(Mutex::new(manager));
        let state = crate::handler::downloads::DownloadState::with_root(Some(downloads.into()));
        session.downloads = Arc::new(Mutex::new(state));
        handler
    }

    /// The JSON of the `list_tabs` reply of `handler`'s default instance.
    async fn listed(handler: &McpHandler) -> serde_json::Value {
        let reply = ListTabsTool::handle(call("list_tabs", json!({})), handler).await;
        serde_json::from_str(&reply_text(&reply.unwrap())).unwrap()
    }

    /// #127: a fresh Chrome instance lists its start tab, as Firefox does.
    #[tokio::test]
    async fn a_fresh_instance_lists_its_start_tab() {
        // Given a Chrome instance no tool has used yet, showing its start page
        let mock = spawn_recording_mock().await;
        let page = json!({"targetId": "T-page-1", "type": "page", "title": "",
            "url": "about:blank", "attached": false});
        let downloads = tempfile::tempdir().unwrap();
        let handler = fresh_instance(mock, json!([page.clone()]), downloads.path());
        let announced = vec![("Target.targetCreated", json!({"targetInfo": page}))];
        fire_events_after(mock, "Target.setDiscoverTargets", announced);
        // When its tabs are listed
        let reply = listed(&handler).await;
        // Then the start page is its active tab
        let tab = json!({"tab_id": "tab-1", "label": null, "target_id": "T-page-1",
            "url": "about:blank"});
        assert_eq!(reply, json!({"active_tab_id": "tab-1", "tabs": [tab]}));
    }

    /// #127: a Chrome with no page left lists no tab, and is not connected
    /// to (the session connects to a page).
    #[tokio::test]
    async fn a_browser_without_pages_lists_no_tab() {
        // Given a browser with no page, which no tool has used yet
        let mock = spawn_recording_mock().await;
        let downloads = tempfile::tempdir().unwrap();
        let handler = fresh_instance(mock, json!([]), downloads.path());
        // When its tabs are listed
        let reply = listed(&handler).await;
        // Then none is, without an error or a connection to a page
        assert_eq!(reply["tabs"], json!([]));
        assert_eq!(reply["active_tab_id"], json!(null));
        assert!(handler.default_session.client.lock().await.is_none());
    }

    /// #130: once the last tab was closed (its connection with it), a
    /// Chrome lists no tab, without an error, as Firefox does.
    #[tokio::test]
    async fn a_browser_whose_last_tab_closed_lists_no_tab() {
        use cdp_browser_lite::test_support::mock_devtools::MockWsBehavior;
        // Given a browser with no page left, whose session connection closed
        let mock = spawn_recording_mock().await;
        let downloads = tempfile::tempdir().unwrap();
        let handler = fresh_instance(mock, json!([]), downloads.path());
        let closed = MockDevTools::start(MockWsBehavior::CloseAfterOneCommand).await;
        let address = format!("127.0.0.1:{}", closed.http_port);
        let timeout = std::time::Duration::from_secs(2);
        let browser = cdp_browser_lite::BrowserClient::connect(&address, timeout)
            .await
            .unwrap();
        let page = browser.attach("T-closed").await.unwrap();
        let client = page.client().clone();
        for _ in 0..100 {
            let probe = client
                .send_raw_command("Browser.getVersion", json!({}))
                .await;
            if matches!(probe, Err(cdp_browser_lite::CdpError::Disconnected)) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        *handler.default_session.client.lock().await = Some(client);
        // When its tabs are listed
        let reply = listed(&handler).await;
        // Then none is, and the dead connection is forgotten, not replaced
        // (with no page, Chrome has nothing to connect to)
        assert_eq!(reply["tabs"], json!([]));
        assert_eq!(reply["active_tab_id"], json!(null));
        assert!(handler.default_session.client.lock().await.is_none());
    }

    /// FIX-09: list_tabs shows the current URL.
    #[test]
    fn list_tabs_shows_current_url() {
        // Given the browser reports that target T1 navigated to https://a/
        let info: cdp_browser_lite::TargetInfo = serde_json::from_value(serde_json::json!(
            {"targetId": "T1", "type": "page", "title": "", "url": "https://a/"}
        ))
        .unwrap();
        // Then its current URL is the reported one
        assert_eq!(current_urls(vec![info])["T1"], "https://a/");
    }
}
