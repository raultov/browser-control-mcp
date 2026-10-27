//! `enable_proxy_auth` on Firefox: `network.authRequired` answered with
//! `network.continueWithAuth`, credentials only for a proxy's 407 (specs
//! D18). Untested against a real authenticating proxy (spike H14).

use crate::handler::cdp_domains::event_pump::ListenerHandles;
#[cfg(test)]
use crate::handler::cdp_domains::fetch::enable_proxy_auth::MAX_AUTH_ATTEMPTS;
use crate::handler::cdp_domains::fetch::enable_proxy_auth::{
    AuthAttempts, DEFAULT_PREWARM_URL, EnableProxyAuthTool, ProxyCredentials,
};
use crate::handler::{McpHandler, parse_args};
use bidi_browser_lite::{BidiClient, BidiResult, WsResponse};
use rust_mcp_sdk::schema::{CallToolError, CallToolRequestParams, CallToolResult};
use serde_json::{Value, json};
use tokio::sync::broadcast::error::RecvError;

/// The `network.continueWithAuth` params for an `authRequired` event: the
/// credentials only when the response is a proxy's 407, and a cancel once
/// the request was challenged more than `MAX_AUTH_ATTEMPTS` times (the
/// proxy rejected them).
pub(crate) fn auth_answer(
    params: &Value,
    credentials: &ProxyCredentials,
    attempts: &mut AuthAttempts,
) -> Option<Value> {
    let request = params.pointer("/request/request")?.as_str()?;
    let status = params.pointer("/response/status").and_then(Value::as_u64);
    if status == Some(407) && !attempts.allow(request) {
        tracing::warn!(
            request,
            "Firefox proxy rejected the credentials; challenge cancelled"
        );
        return Some(json!({"request": request, "action": "cancel"}));
    }
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

/// Answers `network.authRequired` events from `events`, a receiver created
/// before the intercept was added (an event sent before the task subscribed
/// would leave its request blocked).
async fn answer_challenges(
    client: BidiClient,
    mut events: tokio::sync::broadcast::Receiver<WsResponse>,
    credentials: ProxyCredentials,
) {
    let mut attempts = AuthAttempts::default();
    loop {
        let event = match events.recv().await {
            Ok(event) => event,
            Err(RecvError::Lagged(skipped)) => {
                // Their requests stay blocked until Firefox times them out.
                tracing::warn!(
                    skipped,
                    "Firefox proxy auth events lagged; requests may stall"
                );
                continue;
            }
            Err(RecvError::Closed) => break,
        };
        if event.method.as_deref() != Some("network.authRequired") {
            continue;
        }
        let Some(params) = event.params.as_ref() else {
            continue;
        };
        if let Some(answer) = auth_answer(params, &credentials, &mut attempts) {
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
    let events = client.subscribe();
    client
        .subscribe_events(&["network.authRequired"], None)
        .await
        .map_err(protocol("session.subscribe"))?;
    // One intercept per session: the previous call's goes (each call added
    // one more, all answered by the latest handler).
    if let Some(previous) = session.proxy_intercept.lock().await.take() {
        let params = json!({"intercept": previous});
        let _ = client
            .send_raw_command("network.removeIntercept", params)
            .await;
    }
    let intercept = json!({"phases": ["authRequired"]});
    let added = client
        .send_raw_command("network.addIntercept", intercept)
        .await
        .map_err(protocol("network.addIntercept"))?;
    *session.proxy_intercept.lock().await = added
        .result
        .as_ref()
        .and_then(|r| r.get("intercept"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let mut tasks = ListenerHandles::default();
    tasks.push(tokio::spawn(answer_challenges(client, events, credentials)));
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
        let mut attempts = AuthAttempts::default();
        let proxy = auth_answer(&event(407), &creds(), &mut attempts).unwrap();
        assert_eq!(proxy["action"], "provideCredentials");
        assert_eq!(proxy["credentials"]["username"], "alice");
        let site = auth_answer(&event(401), &creds(), &mut attempts).unwrap();
        assert_eq!(site, json!({"request": "R1", "action": "default"}));
    }

    /// AUD-RW-06 (Firefox): a request the proxy keeps challenging is
    /// cancelled after MAX_AUTH_ATTEMPTS answers (Firefox re-raised it
    /// forever: 479 challenges in 10 s).
    #[test]
    fn rejected_credentials_are_not_resent_forever() {
        let event = json!({"request": {"request": "R1"}, "response": {"status": 407}});
        let mut attempts = AuthAttempts::default();
        for _ in 0..MAX_AUTH_ATTEMPTS {
            let answer = auth_answer(&event, &creds(), &mut attempts).unwrap();
            assert_eq!(answer["action"], "provideCredentials");
        }
        let answer = auth_answer(&event, &creds(), &mut attempts).unwrap();
        assert_eq!(answer, json!({"request": "R1", "action": "cancel"}));
    }

    /// AUD-RW-07: enabling proxy auth again removes the previous intercept
    /// (each call added one more).
    #[tokio::test]
    async fn second_enable_removes_the_previous_intercept() {
        use super::super::test_support as ts;
        let mock = bidi_browser_lite::test_support::MockFirefox::start(false)
            .await
            .unwrap();
        let next = std::sync::atomic::AtomicU32::new(0);
        mock.bidi().respond_with("network.addIntercept", move |_| {
            let n = next.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            json!({"intercept": format!("i{n}")})
        });
        let mut handler = ts::handler(&mock);
        handler.proxy.proxy_server = Some("http://127.0.0.1:1".into());
        let call = || {
            serde_json::from_value(json!({"name": "enable_proxy_auth", "arguments": {
                "username": "u", "password": "p", "prewarm_url": "http://127.0.0.1:1/"}}))
            .unwrap()
        };
        enable_proxy_auth(call(), &handler).await.unwrap();
        assert!(
            mock.bidi()
                .commands_named("network.removeIntercept")
                .is_empty()
        );
        enable_proxy_auth(call(), &handler).await.unwrap();
        let removed = mock.bidi().commands_named("network.removeIntercept");
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].params, json!({"intercept": "i1"}));
        ts::stop(&handler).await;
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
