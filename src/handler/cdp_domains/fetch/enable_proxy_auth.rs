use crate::handler::McpHandler;
use crate::handler::cdp_domains::cdp_target::CdpTarget;
use crate::handler::cdp_domains::event_pump::{ListenerHandles, error_kind};
use cdp_browser_lite::{CdpResult, NoParams, WsResponse};
use rust_mcp_sdk::{
    macros,
    schema::{CallToolError, CallToolRequestParams, CallToolResult},
};
use serde_json::json;
use std::time::Duration;
use tokio_stream::StreamExt;

#[macros::mcp_tool(
    name = "enable_proxy_auth",
    description = "Activates proxy authentication interception via Fetch domain, supplying credentials for authenticated proxies. Side effects: starts background event listener that auto-terminates after 30s inactivity; initiates pre-warming navigation. Auth requirements: username and password credentials required. Prerequisites: Chrome must be started with proxy-server flag. Rate limits: 30-second timeout per inactivity period. Returns: confirmation of proxy auth activation. Use this after 'restart_chrome' with proxy settings. Alternatives: manual proxy configuration in system settings."
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct EnableProxyAuthTool {
    /// Chrome instance id from open_instance/list_instances. Omit for the default instance.
    pub instance_id: Option<String>,
    /// The Tab ID of the target tab. Omit to use the active tab.
    pub tab_id: Option<String>,
    /// Proxy authentication username. Constraints: non-empty string. If omitted, uses the --proxy-username set at startup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    /// Proxy authentication password. Constraints: non-empty string. If omitted, uses the --proxy-password set at startup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    /// Resource type to intercept. Constraints: 'Document', 'Image', 'Script', 'XHR', etc. (Chrome CDP resource types). Interactions: filters which request types trigger auth handling. Defaults to: "Document".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource_type: Option<String>,
    /// URL to navigate for proxy pre-warming. Constraints: valid URL (http/https). Interactions: navigated after auth setup to trigger proxy auth flow. Defaults to: `"http://api.ipify.org?format=json"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prewarm_url: Option<String>,
}

/// Credentials handed to the browser for proxy (407) challenges only.
#[derive(Clone)]
struct ProxyCredentials {
    username: String,
    password: String,
}

const DEFAULT_PREWARM_URL: &str = "http://api.ipify.org?format=json";

impl EnableProxyAuthTool {
    pub async fn handle(
        params: CallToolRequestParams,
        handler: &McpHandler,
    ) -> Result<CallToolResult, CallToolError> {
        let tool: EnableProxyAuthTool = serde_json::from_value(serde_json::Value::Object(
            params.arguments.unwrap_or_default(),
        ))
        .map_err(|e| CallToolError::from_message(format!("Failed to parse arguments: {}", e)))?;
        // Everything that can fail without the browser is checked first, so a
        // refused call never leaves Fetch enabled with nobody answering it.
        let credentials = tool.credentials(handler)?;
        let prewarm_url = tool
            .prewarm_url
            .clone()
            .unwrap_or_else(|| DEFAULT_PREWARM_URL.to_string());
        if handler.flags.local && !crate::handler::is_local_address(&prewarm_url) {
            return Err(CallToolError::from_message(format!(
                "Navigation to '{prewarm_url}' is blocked: the server runs with --local"
            )));
        }
        let session = handler.session(tool.instance_id.clone()).await?;
        let target = session.target(tool.tab_id.clone()).await?;
        let resource_type = tool.resource_type.as_deref().unwrap_or("Document");
        enable_fetch(&target, resource_type).await?;

        let mut tasks = ListenerHandles::default();
        tasks.push(tokio::spawn(answer_challenges(target.clone(), credentials)));
        tasks.push(tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(500)).await;
            prewarm(&target, &prewarm_url).await;
        }));
        // Replacing the entry drops (aborts) the previous handler of this target.
        session
            .proxy_auth_tasks
            .lock()
            .await
            .insert(tool.tab_id.clone(), tasks);

        Ok(CallToolResult::text_content(vec![
            "Proxy authentication is configured and active (30s timeout). Pre-warming initiated."
                .into(),
        ]))
    }

    /// Tool arguments first, then the command line.
    fn credentials(&self, handler: &McpHandler) -> Result<ProxyCredentials, CallToolError> {
        let username = self
            .username
            .clone()
            .or_else(|| handler.proxy.proxy_username.clone())
            .ok_or_else(|| {
                CallToolError::from_message(
                    "Proxy credentials missing: username required. Provide 'username' in tool arguments or start browser-control-mcp with --proxy-username.",
                )
            })?;
        let password = self
            .password
            .clone()
            .or_else(|| handler.proxy.proxy_password.clone())
            .ok_or_else(|| {
                CallToolError::from_message(
                    "Proxy credentials missing: password required. Provide 'password' in tool arguments or start browser-control-mcp with --proxy-password.",
                )
            })?;
        Ok(ProxyCredentials { username, password })
    }
}

async fn enable_fetch(target: &CdpTarget, resource_type: &str) -> Result<(), CallToolError> {
    let fetch_params = json!({
        "patterns": [{"urlPattern": "*", "resourceType": resource_type, "requestStage": "Request"}],
        "handleAuthRequests": true
    });
    target
        .send_raw_command("Fetch.enable", fetch_params)
        .await
        .map(|_| ())
        .map_err(|e| {
            CallToolError::from_message(format!(
                "Failed to enable Fetch domain: {}",
                error_kind(&e)
            ))
        })
}

/// Continues paused requests and answers auth challenges until 30 s pass
/// without Fetch events, then disables Fetch.
async fn answer_challenges(target: CdpTarget, credentials: ProxyCredentials) {
    tracing::debug!("proxy auth handler started");
    let mut fetch_events = target.on_domain("Fetch");
    loop {
        match tokio::time::timeout(Duration::from_secs(30), fetch_events.next()).await {
            Ok(Some(Ok(event))) => handle_fetch_event(&target, &event, &credentials).await,
            Ok(Some(Err(e))) => {
                tracing::warn!(
                    error = error_kind(&e),
                    "proxy auth: Fetch event stream error"
                );
                break;
            }
            Ok(None) => break,
            Err(_) => {
                tracing::info!("proxy auth handler idle for 30s; disabling Fetch");
                let _ = target.send_raw_command("Fetch.disable", NoParams).await;
                break;
            }
        }
    }
}

async fn handle_fetch_event(
    target: &CdpTarget,
    event: &WsResponse,
    credentials: &ProxyCredentials,
) {
    let Some(params) = event.params.as_ref() else {
        return;
    };
    let Some(request_id) = params.get("requestId").and_then(|v| v.as_str()) else {
        return;
    };
    match event.method.as_deref() {
        Some("Fetch.requestPaused") => {
            let params = json!({"requestId": request_id});
            let _ = target
                .send_raw_command("Fetch.continueRequest", params)
                .await;
        }
        Some("Fetch.authRequired") => {
            let response = auth_challenge_response(params, credentials);
            let params = json!({"requestId": request_id, "authChallengeResponse": response});
            let result = target
                .send_raw_command("Fetch.continueWithAuth", params)
                .await;
            log_auth_result(request_id, &result);
        }
        _ => {}
    }
}

/// Credentials go only to a proxy's challenge (HTTP 407, `source: "Proxy"`);
/// a site's 401 gets the browser's default handling, never our password.
fn auth_challenge_response(
    params: &serde_json::Value,
    credentials: &ProxyCredentials,
) -> serde_json::Value {
    let from_proxy = params
        .pointer("/authChallenge/source")
        .and_then(|v| v.as_str())
        == Some("Proxy");
    if from_proxy {
        json!({
            "response": "ProvideCredentials",
            "username": credentials.username,
            "password": credentials.password
        })
    } else {
        json!({"response": "Default"})
    }
}

/// Logs the outcome by request id and error kind: the reply (or the error
/// text) may echo the credentials.
fn log_auth_result(request_id: &str, result: &CdpResult<WsResponse>) {
    match result {
        Ok(_) => tracing::debug!(request_id, "proxy auth challenge answered"),
        Err(e) => tracing::warn!(
            request_id,
            error = error_kind(e),
            "proxy auth answer failed"
        ),
    }
}

/// Navigates to the pre-warm URL; the log names its host only.
async fn prewarm(target: &CdpTarget, url: &str) {
    tracing::info!(url = %crate::util::url::redacted(url), "proxy auth pre-warm navigation");
    let _ = target
        .send_raw_command("Page.navigate", json!({"url": url}))
        .await;
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
    async fn test_enable_proxy_auth_params_deserialization() {
        let params: Result<CallToolRequestParams, _> = serde_json::from_value(json!({
            "name": "enable_proxy_auth",
            "arguments": {
                "username": "user",
                "password": "pass"
            }
        }));
        assert!(params.is_ok());
    }

    #[tokio::test]
    async fn test_enable_proxy_auth_tool_deserialization_default() {
        let tool: Result<EnableProxyAuthTool, _> = serde_json::from_value(json!({
            "username": "user",
            "password": "pass"
        }));
        assert!(tool.is_ok());
        let tool = tool.unwrap();
        assert_eq!(tool.username, Some("user".to_string()));
        assert_eq!(tool.password, Some("pass".to_string()));
        assert_eq!(tool.resource_type, None);
        assert_eq!(tool.prewarm_url, None);
    }

    #[tokio::test]
    async fn test_enable_proxy_auth_tool_deserialization_with_optionals() {
        let tool: Result<EnableProxyAuthTool, _> = serde_json::from_value(json!({
            "username": "user",
            "password": "pass",
            "resource_type": "Image",
            "prewarm_url": "https://example.com"
        }));
        assert!(tool.is_ok());
        let tool = tool.unwrap();
        assert_eq!(tool.resource_type, Some("Image".to_string()));
        assert_eq!(tool.prewarm_url, Some("https://example.com".to_string()));
    }

    #[tokio::test]
    async fn test_enable_proxy_auth_handle() {
        let port = spawn_mock_chrome_server().await;

        let mut handler = McpHandler::new_test();
        handler.proxy.proxy_server = Some("http://proxy.example.com:8080".into());
        Arc::get_mut(&mut handler.default_session)
            .unwrap()
            .chrome_manager = Arc::new(Mutex::new(MockChromeManager::new(port)));

        let params: CallToolRequestParams = serde_json::from_value(json!({
            "name": "enable_proxy_auth",
            "arguments": {
                "username": "testuser",
                "password": "testpassword"
            }
        }))
        .unwrap();

        let result = EnableProxyAuthTool::handle(params, &handler).await;
        assert!(result.is_ok(), "Handle should succeed: {:?}", result.err());

        let call_result = result.unwrap();
        assert!(!call_result.content.is_empty());
        let content_str = format!("{:?}", call_result.content);
        assert!(
            content_str.contains("Proxy authentication is configured"),
            "Content didn't match: {}",
            content_str
        );
    }

    #[tokio::test]
    async fn test_enable_proxy_auth_handle_with_cli_credentials() {
        let port = spawn_mock_chrome_server().await;

        let mut handler = McpHandler::new_test();
        handler.proxy.proxy_server = Some("http://proxy.example.com:8080".into());
        handler.proxy.proxy_username = Some("cli_user".into());
        handler.proxy.proxy_password = Some("cli_pass".into());
        Arc::get_mut(&mut handler.default_session)
            .unwrap()
            .chrome_manager = Arc::new(Mutex::new(MockChromeManager::new(port)));

        let params: CallToolRequestParams = serde_json::from_value(json!({
            "name": "enable_proxy_auth",
            "arguments": {}
        }))
        .unwrap();

        let result = EnableProxyAuthTool::handle(params, &handler).await;
        assert!(result.is_ok());
    }

    fn dead_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    fn proxy_handler(port: u16) -> McpHandler {
        let mut handler = McpHandler::new_test();
        handler.proxy.proxy_server = Some("http://proxy.example.com:8080".into());
        Arc::get_mut(&mut handler.default_session)
            .unwrap()
            .chrome_manager = Arc::new(Mutex::new(MockChromeManager::new(port)));
        handler
    }

    fn call(arguments: serde_json::Value) -> CallToolRequestParams {
        serde_json::from_value(json!({"name": "enable_proxy_auth", "arguments": arguments}))
            .unwrap()
    }

    fn creds() -> ProxyCredentials {
        ProxyCredentials {
            username: "alice".into(),
            password: "s3cr3t".into(),
        }
    }

    async fn mock_target() -> CdpTarget {
        let port = spawn_mock_chrome_server().await;
        let client = cdp_browser_lite::CdpClient::new(
            &format!("127.0.0.1:{port}"),
            std::time::Duration::from_secs(5),
        )
        .await
        .unwrap();
        CdpTarget::Client(client)
    }

    /// SEC-03: Proxy pre-warm logs only the host.
    #[tokio::test]
    async fn prewarm_log_shows_host_not_query() {
        // Given a captured subscriber and a page target
        let (logs, _guard) = crate::util::log_capture::capture();
        let target = mock_target().await;
        // When the pre-warm navigation runs on a URL carrying a token
        prewarm(&target, "http://api.example.com/?token=abc").await;
        // Then the logs name the host and not the query
        let text = logs.text();
        assert!(text.contains("http://api.example.com"), "{text}");
        assert!(!text.contains("token=abc"), "{text}");
    }

    /// SEC-04: A failed continueWithAuth never logs or returns the password.
    #[test]
    fn failed_continue_with_auth_never_leaks_password() {
        // Given a captured subscriber and an error whose text echoes the params
        let (logs, _guard) = crate::util::log_capture::capture();
        let echo: CdpResult<WsResponse> = Err(cdp_browser_lite::CdpError::ProtocolError {
            code: -32602,
            message: "bad params {\"password\":\"s3cr3t\"}".into(),
        });
        // When the outcome is logged
        log_auth_result("R1", &echo);
        // Then the request id and the error kind are logged, the password is not
        let text = logs.text();
        assert!(
            text.contains("R1") && text.contains("protocol error"),
            "{text}"
        );
        assert!(!text.contains("s3cr3t"), "{text}");
    }

    /// SEC-09: --local also covers the proxy pre-warm URL.
    #[tokio::test]
    async fn local_mode_blocks_prewarm_url() {
        // Given --local and a browser nobody can reach (any command would fail)
        let mut handler = proxy_handler(dead_port());
        handler.flags.local = true;
        // When enable_proxy_auth gets a non-local pre-warm URL
        let args = json!({"username": "u", "password": "p", "prewarm_url": "https://example.com"});
        let result = EnableProxyAuthTool::handle(call(args), &handler).await;
        // Then it fails with the --local error before touching the browser
        let message = format!("{:?}", result.err().unwrap());
        assert!(message.contains("--local"), "{message}");
    }

    /// SEC-10: Missing proxy credentials fail before Fetch is enabled.
    #[tokio::test]
    async fn missing_credentials_fail_before_fetch_enable() {
        // Given --proxy-server set, no credentials anywhere, and an unreachable browser
        let handler = proxy_handler(dead_port());
        // When enable_proxy_auth is called
        let result = EnableProxyAuthTool::handle(call(json!({})), &handler).await;
        // Then it fails on the credentials, i.e. before any command could be sent
        let message = format!("{:?}", result.err().unwrap());
        assert!(message.contains("Proxy credentials missing"), "{message}");
    }

    /// SEC-11: Credentials are only given to a proxy (407).
    #[test]
    fn credentials_only_answer_proxy_challenges() {
        // Given proxy auth with user alice
        let creds = creds();
        // When a site's challenge arrives
        let site = json!({"requestId": "R1", "authChallenge": {"source": "Server", "origin": "https://a"}});
        let answer = auth_challenge_response(&site, &creds);
        // Then the browser's default handling is used and no credentials are sent
        assert_eq!(answer, json!({"response": "Default"}));
        // And a proxy's challenge gets the credentials
        let proxy =
            json!({"requestId": "R2", "authChallenge": {"source": "Proxy", "origin": "http://p"}});
        let answer = auth_challenge_response(&proxy, &creds);
        assert_eq!(answer["response"], "ProvideCredentials");
        assert_eq!(answer["username"], "alice");
    }

    /// SEC-16: Enabling proxy auth twice keeps a single handler.
    #[tokio::test]
    async fn second_enable_replaces_proxy_handler() {
        // Given proxy auth enabled once on the session page
        let handler = proxy_handler(spawn_mock_chrome_server().await);
        let args = json!({"username": "u", "password": "p"});
        EnableProxyAuthTool::handle(call(args.clone()), &handler)
            .await
            .unwrap();
        let tasks = handler.default_session.proxy_auth_tasks.clone();
        let first = tasks.lock().await.get(&None).unwrap().abort_handles();
        // When it is enabled again on the same target
        EnableProxyAuthTool::handle(call(args), &handler)
            .await
            .unwrap();
        tokio::task::yield_now().await;
        // Then the first handler tasks were aborted and only one entry remains
        assert!(
            first.iter().all(|h| h.is_finished()),
            "first handler still running"
        );
        assert_eq!(tasks.lock().await.len(), 1);
    }
}
