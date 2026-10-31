use crate::backend::BackendError;
use crate::handler::NetworkState;
use crate::handler::cdp_domains::cdp_target::CdpTarget;
use crate::handler::navigation::{
    DOWNLOAD, LOAD_TIMEOUT, NOT_LOADED_NOTE, NavigationOutcome, document_response, load_before,
};
use crate::handler::{BrowserSession, McpHandler, is_local_address};
use rust_mcp_sdk::{
    macros,
    schema::{CallToolError, CallToolRequestParams, CallToolResult},
};
use serde_json::{Value, json};
use tokio::sync::Mutex;

#[macros::mcp_tool(
    name = "navigate",
    description = "Loads a URL in the tab and waits for load (at most 30 s). Returns {url (after redirects), tab_id, browser, status?, notes?}; a network error or a download fails with 'Navigation failed: <reason> (<url>)'. With --local only local addresses are allowed. Note: copy_cookies imports the user's real Chrome cookies: ask the user first. On a running instance it relaunches it, closing its tabs: the call first fails listing them, then re-issue it with confirm_restart: true."
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct NavigateTool {
    /// Instance id; omit for the default instance.
    pub instance_id: Option<String>,
    /// Tab id; omit for the active tab.
    pub tab_id: Option<String>,
    /// Absolute URL (http, https or file).
    pub url: String,
    /// Import cookies from the user's Chrome profile (needs --allow-cookie-import; Chrome only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub copy_cookies: Option<bool>,
    /// Chrome profile to import cookies from (e.g. 'Profile 1'). Default: the last used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_profile: Option<String>,
    /// Allow relaunching a running instance to import cookies (closes its tabs). Default: false.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirm_restart: Option<bool>,
}

impl NavigateTool {
    pub async fn handle(
        params: CallToolRequestParams,
        handler: &McpHandler,
    ) -> Result<CallToolResult, CallToolError> {
        let args: NavigateTool = crate::handler::parse_args(params)?;
        let session = handler.session(args.instance_id.clone()).await?;

        if handler.flags.local && !is_local_address(&args.url) {
            return Err(CallToolError::from_message(format!(
                "Navigation to '{}' is blocked. This MCP server is running with the 'local' argument, which restricts navigation to local addresses only (localhost, 127.0.0.1, 192.168.x.x, or *.local). To allow navigation to external addresses, restart the MCP server without the 'local' argument.",
                args.url
            )));
        }
        if args.copy_cookies == Some(true) {
            args.import_cookies(handler, &session).await?;
        }
        let target = session.target(args.tab_id.clone()).await?;
        let network = session.network_state(args.tab_id.clone())?;
        let outcome = navigate_and_wait(&target, &network, &args.url).await?;
        let tab_id = session.tab_id_of(args.tab_id.as_deref());
        Ok(outcome.reply(&format!("Navigated to {}.", outcome.url), tab_id.as_deref()))
    }

    /// Seeds the instance's profile with the user's cookies: on the next
    /// launch if it is not running, else by relaunching it (only after the
    /// user confirmed, because every tab is closed).
    async fn import_cookies(
        &self,
        handler: &McpHandler,
        session: &std::sync::Arc<BrowserSession>,
    ) -> Result<(), CallToolError> {
        use crate::handler::chrome_instance::cookie_seed::{
            RealEnvProvider, RealFsProbe, resolve_source, seed_cookies,
        };
        if !handler.flags.allow_cookie_import {
            return Err(CallToolError::from_message(
                "Cookie import is disabled. Restart the MCP server with the '--allow-cookie-import' argument to enable it.",
            ));
        }
        if handler.base_params.browser.user_profile {
            return Err(CallToolError::from_message(
                "Cookie import is redundant when running in --user-profile mode, as the user's real profile is already in use.",
            ));
        }
        let source = resolve_source(
            self.source_profile.as_deref(),
            &RealEnvProvider,
            &RealFsProbe,
        )
        .map_err(CallToolError::from_message)?;
        let is_running = session.chrome_manager.lock().await.is_running().await;
        if is_running && self.confirm_restart != Some(true) {
            let instance_id = self.instance_id.as_deref().unwrap_or("default");
            return Err(CallToolError::from_message(restart_warning(
                instance_id,
                session,
            )));
        }
        let report = seed_cookies(&source, &RealEnvProvider, &RealFsProbe)
            .map_err(CallToolError::from_message)?;
        if is_running {
            crate::handler::chrome_instance::seeded_relaunch::relaunch_seeded(session, report)
                .await
                .map_err(CallToolError::from_message)
        } else {
            session
                .chrome_manager
                .lock()
                .await
                .set_seed_profile(Some(report));
            Ok(())
        }
    }
}

/// Navigates `target` and waits (at most [`LOAD_TIMEOUT`]) for the new
/// document to fire `load`. Chrome answers `Page.navigate` as soon as the
/// navigation starts and reports a network error only as `errorText` inside
/// a successful reply: that becomes a tool error here ([`check_error`]).
/// `network` is the tab's request log, where the document's HTTP status is.
pub(crate) async fn navigate_and_wait(
    target: &CdpTarget,
    network: &Mutex<NetworkState>,
    url: &str,
) -> Result<NavigationOutcome, CallToolError> {
    // Subscribed first: `load` can fire before the reply is read.
    let mut page_events = target.on_domain("Page");
    let deadline = tokio::time::Instant::now() + LOAD_TIMEOUT;
    let reply = match target
        .send_raw_command("Page.navigate", json!({"url": url}))
        .await
    {
        Ok(reply) => reply,
        // Chrome answers once the response headers arrive: a slow server
        // outlasts the command timeout while the navigation goes on.
        Err(cdp_browser_lite::CdpError::Timeout { .. }) => {
            let loaded = load_before(&mut page_events, deadline, is_load).await;
            let outcome = finish(target, url, json!({}), loaded).await;
            return Ok(with_status(outcome, target, network).await);
        }
        Err(e) => {
            return Err(CallToolError::from_message(crate::handler::cdp_failure(
                "CDP Error",
                &e,
            )));
        }
    };
    let browser = reply.result.unwrap_or_else(|| json!({}));
    check_error(&browser, network, url).await?;
    // Same-document navigations (fragment, history) have no loader and fire no `load`.
    let loaded = browser.get("loaderId").is_none() || wait_for_load(&mut page_events).await;
    let outcome = finish(target, url, browser, loaded).await;
    Ok(with_status(outcome, target, network).await)
}

/// Fails on the `errorText` of a `Page.navigate` reply, saying so for a
/// download. An error status with an empty body
/// (`ERR_HTTP_RESPONSE_CODE_FAILURE`, Chrome then shows its own page) is
/// not a failure when the response is logged: the server answered, and the
/// reply carries its status, as on Firefox.
async fn check_error(
    browser: &Value,
    network: &Mutex<NetworkState>,
    url: &str,
) -> Result<(), CallToolError> {
    let text = |key: &str| browser.get(key).and_then(Value::as_str);
    let Some(reason) = text("errorText").filter(|e| !e.is_empty()) else {
        return Ok(());
    };
    if browser.get("isDownload").and_then(Value::as_bool) == Some(true) {
        return Err(navigation_failed(DOWNLOAD, url));
    }
    let answered = reason == "net::ERR_HTTP_RESPONSE_CODE_FAILURE"
        && document_response(network, text("loaderId"), url)
            .await
            .is_some();
    if answered {
        return Ok(());
    }
    Err(navigation_failed(reason, url))
}

fn navigation_failed(reason: &str, url: &str) -> CallToolError {
    BackendError::Navigation {
        reason: reason.to_string(),
        url: url.to_string(),
    }
    .into()
}

/// The outcome once the page settled: its committed URL, plus a note when
/// `load` did not fire in time.
pub(crate) async fn finish(
    target: &CdpTarget,
    requested: &str,
    browser: Value,
    loaded: bool,
) -> NavigationOutcome {
    let url = committed_url(target)
        .await
        .unwrap_or_else(|| requested.to_string());
    let notes = if loaded {
        Vec::new()
    } else {
        vec![NOT_LOADED_NOTE.to_string()]
    };
    NavigationOutcome {
        url,
        browser,
        notes,
        status: None,
    }
}

/// `outcome` with its document's HTTP status, read from the tab's request
/// log `network` (the document request id is its loader id).
pub(crate) async fn with_status(
    outcome: NavigationOutcome,
    target: &CdpTarget,
    network: &Mutex<NetworkState>,
) -> NavigationOutcome {
    let loader = match outcome.browser.get("loaderId").and_then(Value::as_str) {
        Some(id) => Some(id.to_string()),
        None => main_loader(target).await,
    };
    let document = document_response(network, loader.as_deref(), &outcome.url).await;
    outcome.with_status(document.and_then(|d| d.response_status))
}

/// The loader id of the tab's current document (its document request id),
/// for replies that carry none (`Page.reload`, a timed-out navigation).
async fn main_loader(target: &CdpTarget) -> Option<String> {
    let reply = target
        .send_raw_command("Page.getFrameTree", json!({}))
        .await
        .ok()?
        .result?;
    let loader = reply.pointer("/frameTree/frame/loaderId")?.as_str()?;
    Some(loader.to_string())
}

/// Waits for `Page.loadEventFired` on `events`; `false` after [`LOAD_TIMEOUT`].
pub(crate) async fn wait_for_load(events: &mut cdp_browser_lite::EventFilter) -> bool {
    let deadline = tokio::time::Instant::now() + LOAD_TIMEOUT;
    load_before(events, deadline, is_load).await
}

fn is_load(event: &cdp_browser_lite::CdpResult<cdp_browser_lite::WsResponse>) -> bool {
    event
        .as_ref()
        .is_ok_and(|e| e.method.as_deref() == Some("Page.loadEventFired"))
}

/// The URL of the current history entry (the committed document, after
/// redirects), read without running script in the page.
async fn committed_url(target: &CdpTarget) -> Option<String> {
    let reply = target
        .send_raw_command("Page.getNavigationHistory", json!({}))
        .await
        .ok()?
        .result?;
    let index = reply.get("currentIndex")?.as_u64()?;
    let entry = reply.get("entries")?.get(usize::try_from(index).ok()?)?;
    entry.get("url")?.as_str().map(str::to_string)
}

/// The message asking the agent to get the user's consent before a seeded
/// relaunch, listing the tabs that would be closed.
fn restart_warning(instance_id: &str, session: &BrowserSession) -> String {
    let registry = crate::util::sync::read(&session.tabs);
    let tab_list = registry
        .tabs
        .iter()
        .map(|(id, entry)| {
            let label = entry
                .label
                .as_ref()
                .map(|l| format!(" \"{l}\""))
                .unwrap_or_default();
            let active = if registry.active_tab_id.as_ref() == Some(id) {
                " (active)"
            } else {
                ""
            };
            format!("  - {id}{label}{active}: {}", entry.url)
        })
        .collect::<Vec<_>>()
        .join("\n");
    let tab_count = registry.tabs.len();
    let tab_info = if tab_count > 0 {
        format!(
            "its {tab_count} open tab(s), discarding their state (unsaved form input, breakpoints, and captured logs):\n{tab_list}"
        )
    } else {
        "any open tabs".to_string()
    };
    format!(
        "Cookie import requires relaunching instance '{instance_id}' with a seeded profile: a running browser cannot pick up cookies from disk.\n\nThis will CLOSE the instance and {tab_info}\n\nTell the user exactly which tabs will be closed and ask them to confirm. Only if they agree, call navigate again with the same arguments plus `confirm_restart: true`."
    )
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
    async fn test_navigate_params_deserialization() {
        let params: Result<CallToolRequestParams, _> = serde_json::from_value(json!({
            "name": "navigate",
            "arguments": {
                "url": "https://example.com"
            }
        }));
        assert!(params.is_ok());
    }

    #[tokio::test]
    async fn test_navigate_tool_deserialization() {
        let tool: Result<NavigateTool, _> = serde_json::from_value(json!({
            "url": "https://example.com"
        }));
        assert!(tool.is_ok());
        assert_eq!(tool.unwrap().url, "https://example.com");
    }

    use crate::handler::cdp_domains::tests::{
        call, committed_url, fire_events_after, fire_load_after, mock_handler, reply_text,
        spawn_recording_mock,
    };
    use crate::handler::navigation::DOWNLOAD;

    /// The JSON object after the first line of a reply.
    fn body(reply: &CallToolResult) -> Value {
        let text = reply_text(reply);
        serde_json::from_str(&text[text.find('{').unwrap()..]).unwrap()
    }

    /// NAV-01 / NAV-02: navigate waits for `load` and answers the final URL
    /// (after redirects) in the shared shape.
    #[tokio::test]
    async fn navigate_reports_final_url_after_load() {
        // Given a Chrome whose navigation to /start ends on /final
        let mock = spawn_recording_mock().await;
        mock.respond_with(
            "Page.navigate",
            |_| json!({"frameId": "F1", "loaderId": "L1"}),
        );
        committed_url(mock, "http://127.0.0.1:8/final");
        fire_load_after(mock, "Page.navigate");
        let handler = mock_handler(mock.http_port);
        // When navigate runs
        let args = json!({"url": "http://127.0.0.1:8/start"});
        let reply = NavigateTool::handle(call("navigate", args), &handler)
            .await
            .unwrap();
        // Then the reply has the final URL, the tab and the CDP ids
        assert!(reply_text(&reply).starts_with("Navigated to http://127.0.0.1:8/final."));
        let body = body(&reply);
        assert_eq!(body["url"], "http://127.0.0.1:8/final");
        assert_eq!(body["browser"]["loaderId"], "L1");
        assert!(body.get("tab_id").is_some());
        assert!(body.get("notes").is_none(), "load fired: {body}");
    }

    /// The CDP events of a document response with `status` on loader `L1`.
    fn document_events(status: i64) -> Vec<(&'static str, Value)> {
        let url = "http://127.0.0.1:8/page";
        vec![
            (
                "Network.requestWillBeSent",
                json!({"requestId": "L1", "loaderId": "L1", "type": "Document", "request": {"url": url, "method": "GET", "headers": {}}}),
            ),
            (
                "Network.responseReceived",
                json!({"requestId": "L1", "loaderId": "L1", "type": "Document", "response": {"url": url, "status": status, "statusText": "", "headers": {}}}),
            ),
            ("Page.loadEventFired", json!({"timestamp": 1.0})),
        ]
    }

    /// AUD-EV-11: the document's HTTP status is in the reply; an error
    /// status adds a note.
    #[tokio::test]
    async fn navigate_reports_the_http_status() {
        // Given a server answering 404
        let mock = spawn_recording_mock().await;
        mock.respond_with(
            "Page.navigate",
            |_| json!({"frameId": "F1", "loaderId": "L1"}),
        );
        committed_url(mock, "http://127.0.0.1:8/page");
        fire_events_after(mock, "Page.navigate", document_events(404));
        let handler = mock_handler(mock.http_port);
        // When navigate runs
        let args = json!({"url": "http://127.0.0.1:8/page"});
        let reply = NavigateTool::handle(call("navigate", args), &handler)
            .await
            .unwrap();
        // Then the reply has the status and says it is an error
        let body = body(&reply);
        assert_eq!(body["status"], 404, "{body}");
        assert_eq!(body["notes"][0], "the server answered HTTP 404");
    }

    /// AUD-EV-11: an error status with an empty body is a navigation that
    /// reached the server (Chrome reports ERR_HTTP_RESPONSE_CODE_FAILURE and
    /// shows its own page; Firefox shows the empty page).
    #[tokio::test]
    async fn empty_error_page_is_reported_with_its_status() {
        // Given a server answering 500 with no body
        let mock = spawn_recording_mock().await;
        mock.respond_with("Page.navigate", |_| {
            json!({"frameId": "F1", "loaderId": "L1", "errorText": "net::ERR_HTTP_RESPONSE_CODE_FAILURE"})
        });
        committed_url(mock, "http://127.0.0.1:8/page");
        fire_events_after(mock, "Page.navigate", document_events(500));
        let handler = mock_handler(mock.http_port);
        // When navigate runs
        let args = json!({"url": "http://127.0.0.1:8/page"});
        let reply = NavigateTool::handle(call("navigate", args), &handler)
            .await
            .unwrap();
        // Then it succeeds with the status
        let body = body(&reply);
        assert_eq!(body["status"], 500, "{body}");
        assert_eq!(body["notes"][0], "the server answered HTTP 500");
    }

    /// AUD-EV-21: a URL the browser downloads fails, saying so (same on Firefox).
    #[tokio::test]
    async fn download_fails_saying_so() {
        let mock = spawn_recording_mock().await;
        mock.respond_with("Page.navigate", |_| {
            json!({"frameId": "F1", "loaderId": "L1", "errorText": "net::ERR_ABORTED", "isDownload": true})
        });
        let handler = mock_handler(mock.http_port);
        let args = json!({"url": "http://127.0.0.1:8/a.bin"});
        let error = NavigateTool::handle(call("navigate", args), &handler)
            .await
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            format!("Navigation failed: {DOWNLOAD} (http://127.0.0.1:8/a.bin)")
        );
    }

    /// NAV-03: a network error fails the call (Chrome reports it inside a
    /// successful Page.navigate reply).
    #[tokio::test]
    async fn unknown_host_fails_navigation() {
        // Given a host Chrome cannot resolve
        let mock = spawn_recording_mock().await;
        mock.respond_with("Page.navigate", |_| {
            json!({"frameId": "F1", "loaderId": "L1", "errorText": "net::ERR_NAME_NOT_RESOLVED"})
        });
        let handler = mock_handler(mock.http_port);
        // When navigate runs
        let args = json!({"url": "http://nonexistent-host.invalid/"});
        let error = NavigateTool::handle(call("navigate", args), &handler)
            .await
            .unwrap_err();
        // Then it fails with the network error and the URL
        assert_eq!(
            error.to_string(),
            "Navigation failed: net::ERR_NAME_NOT_RESOLVED (http://nonexistent-host.invalid/)"
        );
    }

    #[tokio::test]
    async fn test_navigate_handle() {
        let port = spawn_mock_chrome_server().await;

        let mut handler = McpHandler::new_test();
        Arc::get_mut(&mut handler.default_session)
            .unwrap()
            .chrome_manager = Arc::new(Mutex::new(MockChromeManager::new(port)));

        let params: CallToolRequestParams = serde_json::from_value(json!({
            "name": "navigate",
            "arguments": {
                "url": "https://example.com"
            }
        }))
        .unwrap();

        let result = NavigateTool::handle(params, &handler).await;
        assert!(result.is_ok(), "Handle should succeed: {:?}", result.err());

        let call_result = result.unwrap();
        assert!(!call_result.content.is_empty());
        let content_str = format!("{:?}", call_result.content);
        assert!(
            content_str.contains("Navigated to https://example.com"),
            "Content didn't match: {}",
            content_str
        );
    }

    #[tokio::test]
    async fn test_navigate_local_only_restriction() {
        let mut handler = McpHandler::new_test();
        handler.flags.local = true;

        let params: CallToolRequestParams = serde_json::from_value(json!({
            "name": "navigate",
            "arguments": {
                "url": "https://google.com"
            }
        }))
        .unwrap();

        let result = NavigateTool::handle(params, &handler).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.to_string()
                .contains("Navigation to 'https://google.com' is blocked")
        );

        // Test with local address
        let params_local: CallToolRequestParams = serde_json::from_value(json!({
            "name": "navigate",
            "arguments": {
                "url": "http://localhost:3000"
            }
        }))
        .unwrap();

        let port = spawn_mock_chrome_server().await;
        let mut handler_local = McpHandler::new_test_with_port(port);
        handler_local.flags.local = true;

        let result_local = NavigateTool::handle(params_local, &handler_local).await;
        assert!(
            result_local.is_ok(),
            "Local navigation should succeed: {:?}",
            result_local.err()
        );
    }

    #[tokio::test]
    async fn test_navigate_local_only_addresses() {
        let mut handler = McpHandler::new_test();
        handler.flags.local = true;
        let port = spawn_mock_chrome_server().await;
        Arc::get_mut(&mut handler.default_session)
            .unwrap()
            .chrome_manager = Arc::new(Mutex::new(MockChromeManager::new(port)));

        let local_urls = vec![
            "http://127.0.0.1:8080",
            "http://localhost:5173",
            "http://192.168.1.50/index.html",
            "http://myapp.local/",
        ];

        for url in local_urls {
            let params: CallToolRequestParams = serde_json::from_value(json!({
                "name": "navigate",
                "arguments": {
                    "url": url
                }
            }))
            .unwrap();
            let result = NavigateTool::handle(params, &handler).await;
            assert!(result.is_ok(), "URL {} should be allowed", url);
        }

        let blocked_urls = vec![
            "https://github.com",
            "http://10.0.0.1", // Currently not in my list, following prompt's specific list
            "http://1.1.1.1",
        ];

        for url in blocked_urls {
            let params: CallToolRequestParams = serde_json::from_value(json!({
                "name": "navigate",
                "arguments": {
                    "url": url
                }
            }))
            .unwrap();
            let result = NavigateTool::handle(params, &handler).await;
            assert!(result.is_err(), "URL {} should be blocked", url);
        }
    }

    #[tokio::test]
    async fn test_navigate_cookie_import_disabled_by_default() {
        let handler = McpHandler::new_test();

        let params: CallToolRequestParams = serde_json::from_value(json!({
            "name": "navigate",
            "arguments": {
                "url": "https://example.com",
                "copy_cookies": true
            }
        }))
        .unwrap();

        let result = NavigateTool::handle(params, &handler).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("Cookie import is disabled"));
    }

    #[tokio::test]
    async fn test_navigate_cookie_import_user_profile_error() {
        let mut handler = McpHandler::new_test();
        handler.flags.allow_cookie_import = true;
        handler.base_params.browser.user_profile = true;

        let params: CallToolRequestParams = serde_json::from_value(json!({
            "name": "navigate",
            "arguments": {
                "url": "https://example.com",
                "copy_cookies": true
            }
        }))
        .unwrap();

        let result = NavigateTool::handle(params, &handler).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.to_string()
                .contains("redundant when running in --user-profile mode")
        );
    }

    /// TOL-08: Protocol replies are returned as JSON, not Rust Debug.
    #[tokio::test]
    async fn protocol_replies_are_json() {
        // Given a browser (mock) answering Page.navigate
        let port = spawn_mock_chrome_server().await;
        let mut handler = McpHandler::new_test();
        Arc::get_mut(&mut handler.default_session)
            .unwrap()
            .chrome_manager = Arc::new(Mutex::new(MockChromeManager::new(port)));
        // When navigate is called
        let params: CallToolRequestParams = serde_json::from_value(
            json!({"name": "navigate", "arguments": {"url": "https://example.com"}}),
        )
        .unwrap();
        let result = NavigateTool::handle(params, &handler).await.unwrap();
        // Then the text is the prefix followed by a JSON object
        let text = result.content[0].as_text_content().unwrap().text.clone();
        let (prefix, body) = text.split_once('\n').unwrap();
        assert_eq!(prefix, "Navigated to https://example.com.");
        assert!(
            serde_json::from_str::<serde_json::Value>(body)
                .unwrap()
                .is_object()
        );
        assert!(!text.contains("WsResponse"));
    }
}
