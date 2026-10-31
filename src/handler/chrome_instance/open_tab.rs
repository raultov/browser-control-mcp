use crate::handler::McpHandler;
use rust_mcp_sdk::{
    macros,
    schema::{CallToolError, CallToolRequestParams, CallToolResult},
};

#[macros::mcp_tool(
    name = "open_tab",
    description = "Opens a tab (launching the instance if needed) and waits for 'url' to load. Returns {tab_id, target_id, url, label}; a network error fails naming the tab, which stays open."
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct OpenTabTool {
    /// Instance id; omit for the default instance.
    pub instance_id: Option<String>,
    /// URL to load. Default: about:blank.
    pub url: Option<String>,
    /// Unique label for the tab.
    pub label: Option<String>,
}

/// The page a new tab is created on.
const BLANK: &str = "about:blank";

/// The URL to navigate a freshly created (blank) tab to, if any.
fn deferred_navigation(url: &str) -> Option<&str> {
    (url != BLANK).then_some(url)
}

/// Creates a blank tab and registers it with its listeners. The label and
/// the tab limit are checked first, and a tab that still fails to register
/// is closed: no page is left open that no tab id reaches.
async fn create_tab(
    session: &crate::handler::BrowserSession,
    label: Option<String>,
    url: &str,
) -> Result<(String, String), CallToolError> {
    crate::util::sync::read(&session.tabs)
        .check_new(label.as_deref())
        .map_err(CallToolError::from_message)?;
    let browser_client = session.browser_client().await?;
    // The tab starts blank and navigates only once its listeners are
    // enabled: created straight on `url`, the page's first requests (and
    // their network events) were already gone.
    let tab = browser_client
        .new_tab(BLANK)
        .await
        .map_err(|e| CallToolError::from_message(format!("Failed to create new tab: {}", e)))?;
    let target_id = tab.target_id().to_string();
    let info = super::tab_registry::NewTab::new(label, url);
    let registered =
        super::tab_lifecycle::register_and_listen(&session.tabs, &tab, info, session.enable_webmcp)
            .await;
    match registered {
        Ok(tab_id) => Ok((tab_id, target_id)),
        Err(e) => {
            let _ = browser_client.close_tab(&target_id).await;
            Err(CallToolError::from_message(format!(
                "Failed to register tab in registry: {e}"
            )))
        }
    }
}

/// Navigates a registered blank tab to `url`, through the registered
/// session: the target-discovery listener may have registered this page
/// first, with its own session, and still be enabling its domains. Chrome
/// answers a session's commands in order, so once this (idempotent)
/// `Network.enable` replies, the page's first requests will be recorded.
/// Waits for `load` like `navigate`; a network error fails, naming the tab
/// that stays open.
async fn open_url(
    session: &crate::handler::BrowserSession,
    tab_id: &str,
    url: &str,
) -> Result<String, CallToolError> {
    let registered = session.target(Some(tab_id.to_string())).await?;
    let _ = registered
        .send_raw_command("Network.enable", serde_json::json!({}))
        .await;
    let network = session.network_state(Some(tab_id.to_string()))?;
    let outcome =
        crate::handler::cdp_domains::page::navigate::navigate_and_wait(&registered, &network, url)
            .await
            .map_err(|e| {
                CallToolError::from_message(crate::handler::navigation::open_tab_navigation_failed(
                    &e.to_string(),
                    tab_id,
                ))
            })?;
    Ok(outcome.url)
}

impl OpenTabTool {
    pub async fn handle(
        params: CallToolRequestParams,
        handler: &McpHandler,
    ) -> Result<CallToolResult, CallToolError> {
        let args: OpenTabTool = crate::handler::parse_args(params)?;
        let session = handler.session(args.instance_id.clone()).await?;

        // Refused before anything starts a browser.
        let url = args.url.clone().unwrap_or_else(|| BLANK.to_string());
        if handler.flags.local && !crate::handler::is_local_address(&url) {
            return Err(CallToolError::from_message(format!(
                "Navigation to '{}' is blocked by local-only restrictions.",
                url
            )));
        }

        session.check_browser().await?;
        {
            let mut manager = session.chrome_manager.lock().await;
            manager.ensure_instance().await.map_err(|e| {
                CallToolError::from_message(format!(
                    "Failed to ensure Chrome instance: {}",
                    super::launch::describe_launch_failure(&e.to_string())
                ))
            })?;
        }
        // The session connection starts target discovery (popups, tabs the
        // page or the user closes, the browser's first page): without it a
        // session whose first call is open_tab would never track them.
        drop(session.get_or_connect().await?);

        let (tab_id, target_id) = create_tab(&session, args.label.clone(), &url).await?;
        let url = match deferred_navigation(&url) {
            Some(url) => open_url(&session, &tab_id, url).await?,
            None => url,
        };

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

    /// NAV-04: open_tab to an unreachable host fails and names the tab.
    #[tokio::test]
    async fn open_tab_with_bad_url_fails_naming_the_tab() {
        use crate::handler::cdp_domains::tests::{mock_handler, spawn_recording_mock};
        // Given a tab open_tab created, on a Chrome that cannot resolve the host
        let mock = spawn_recording_mock().await;
        mock.respond_with("Page.navigate", |_| {
            serde_json::json!({"frameId": "F", "loaderId": "L", "errorText": "net::ERR_NAME_NOT_RESOLVED"})
        });
        let handler = mock_handler(mock.http_port);
        let session = handler.session(None).await.unwrap();
        let browser = cdp_browser_lite::BrowserClient::connect(
            &format!("127.0.0.1:{}", mock.http_port),
            std::time::Duration::from_secs(5),
        )
        .await
        .unwrap();
        let tab = browser.attach("T-page-1").await.unwrap();
        let info = super::super::tab_registry::NewTab::new(None, BLANK);
        let tab_id = crate::util::sync::write(&session.tabs)
            .register_tab(tab, info, false)
            .unwrap();
        // When it navigates the tab to the URL
        let error = open_url(&session, &tab_id, "http://nonexistent-host.invalid/")
            .await
            .unwrap_err()
            .to_string();
        // Then the network error is reported with the tab that stays open
        assert_eq!(
            error,
            format!(
                "Navigation failed: net::ERR_NAME_NOT_RESOLVED (http://nonexistent-host.invalid/). The tab stays open as {tab_id} (close it with close_tab)."
            )
        );
    }
}
