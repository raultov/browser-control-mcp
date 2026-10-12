//! `enable_proxy_auth` on Firefox: `network.authRequired` answered with
//! `network.continueWithAuth`, credentials only for a proxy's 407 (specs
//! D18). Untested against a real authenticating proxy (spike H14).

use crate::handler::cdp_domains::event_pump::ListenerHandles;
use crate::handler::cdp_domains::fetch::enable_proxy_auth::{
    DEFAULT_PREWARM_URL, EnableProxyAuthTool, ProxyCredentials,
};
use crate::handler::{McpHandler, parse_args};
use bidi_browser_lite::{BidiClient, BidiResult, WsResponse};
use rust_mcp_sdk::schema::{CallToolError, CallToolRequestParams, CallToolResult};
use serde_json::{Value, json};
use tokio::sync::broadcast::error::RecvError;

/// The `network.continueWithAuth` params for an `authRequired` event: the
/// credentials only when the response is a proxy's 407.
pub(crate) fn auth_answer(params: &Value, credentials: &ProxyCredentials) -> Option<Value> {
    let request = params.pointer("/request/request")?.as_str()?;
    let status = params.pointer("/response/status").and_then(Value::as_u64);
    Some(if status == Some(407) {
        json!({
            "request": request,
            "action": "provideCredentials",
            "credentials": {"type": "password", "username": credentials.username, "password": credentials.password}
        })
    } else {
        json!({"request": request, "action": "default"})
    })
}

/// Logs the outcome by request id and kind only (the error could echo the credentials).
fn log_answer(request: &str, result: &BidiResult<WsResponse>) {
    match result {
        Ok(_) => tracing::debug!(request, "Firefox proxy auth challenge answered"),
        Err(_) => tracing::warn!(request, "Firefox proxy auth answer failed"),
    }
}

async fn answer_challenges(client: BidiClient, credentials: ProxyCredentials) {
    let mut events = client.subscribe();
    loop {
        let event = match events.recv().await {
            Ok(event) => event,
            Err(RecvError::Lagged(_)) => continue,
            Err(RecvError::Closed) => break,
        };
        if event.method.as_deref() != Some("network.authRequired") {
            continue;
        }
        let Some(params) = event.params.as_ref() else {
            continue;
        };
        if let Some(answer) = auth_answer(params, &credentials) {
            let request = answer["request"].as_str().unwrap_or_default().to_string();
            let result = client
                .send_raw_command("network.continueWithAuth", answer)
                .await;
            log_answer(&request, &result);
        }
    }
}

/// `enable_proxy_auth` on a Firefox instance.
pub(crate) async fn enable_proxy_auth(
    params: CallToolRequestParams,
    handler: &McpHandler,
) -> Result<CallToolResult, CallToolError> {
    let args: EnableProxyAuthTool = parse_args(params)?;
    // Checked first, so a refused call changes nothing in the browser.
    let credentials = args.credentials(handler)?;
    let prewarm = args
        .prewarm_url
        .clone()
        .unwrap_or_else(|| DEFAULT_PREWARM_URL.to_string());
    if handler.flags.local && !crate::handler::is_local_address(&prewarm) {
        return Err(CallToolError::from_message(format!(
            "Navigation to '{prewarm}' is blocked: the server runs with --local"
        )));
    }
    let session = handler.firefox(args.instance_id.as_deref()).await?;
    let (_, tab) = session.tab(args.tab_id.as_deref()).await?;
    let client = tab.tab.client().clone();
    let protocol = |method: &'static str| move |e| super::protocol_error(method, &e);
    client
        .subscribe_events(&["network.authRequired"], None)
        .await
        .map_err(protocol("session.subscribe"))?;
    let intercept = json!({"phases": ["authRequired"]});
    client
        .send_raw_command("network.addIntercept", intercept)
        .await
        .map_err(protocol("network.addIntercept"))?;
    let mut tasks = ListenerHandles::default();
    tasks.push(tokio::spawn(answer_challenges(client, credentials)));
    // Replacing the previous handler aborts it (no stacked answers).
    *session.proxy_task.lock().await = tasks;
    tracing::info!(url = %crate::util::url::redacted(&prewarm), "Firefox proxy auth pre-warm navigation");
    let _ = tab
        .tab
        .navigate(&prewarm, bidi_browser_lite::ReadinessState::None)
        .await;
    Ok(CallToolResult::text_content(vec![
        "Proxy authentication is configured and active (proxy 407 challenges only). Pre-warming initiated.".into(),
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn creds() -> ProxyCredentials {
        ProxyCredentials {
            username: "alice".into(),
            password: "s3cr3t".into(),
        }
    }

    /// BID-26: Proxy credentials only answer 407.
    #[test]
    fn firefox_credentials_only_for_407() {
        let event =
            |status: u64| json!({"request": {"request": "R1"}, "response": {"status": status}});
        let proxy = auth_answer(&event(407), &creds()).unwrap();
        assert_eq!(proxy["action"], "provideCredentials");
        assert_eq!(proxy["credentials"]["username"], "alice");
        let site = auth_answer(&event(401), &creds()).unwrap();
        assert_eq!(site, json!({"request": "R1", "action": "default"}));
    }

    /// BID-27: Missing proxy credentials fail before any intercept is added.
    #[tokio::test]
    async fn firefox_missing_credentials_fail_first() {
        let mut handler = crate::handler::test_handler();
        handler.proxy.proxy_server = Some("http://p:1".into());
        handler.default_browser = crate::backend::BrowserKind::Firefox;
        let params =
            serde_json::from_value(json!({"name": "enable_proxy_auth", "arguments": {}})).unwrap();
        let err = enable_proxy_auth(params, &handler).await.unwrap_err();
        assert!(format!("{err:?}").contains("Proxy credentials missing"));
        assert!(
            handler.registry.get_firefox("default").is_none(),
            "nothing was launched"
        );
    }

    /// BID-28: Proxy auth secrets never reach the logs.
    #[test]
    fn firefox_proxy_secrets_never_logged() {
        let (logs, _guard) = crate::util::log_capture::capture();
        let echo: BidiResult<WsResponse> = Err(bidi_browser_lite::BidiError::InternalError(
            "bad {\"password\":\"s3cr3t\"}".into(),
        ));
        log_answer("R1", &echo);
        let text = logs.text();
        assert!(text.contains("R1"), "{text}");
        assert!(!text.contains("s3cr3t"), "{text}");
    }
}
