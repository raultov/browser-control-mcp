//! Raw protocol access on Firefox: `send_bidi_command` and `send_rdp_request`.

use super::session::FirefoxSession;
use crate::handler::{McpHandler, parse_args};
use crate::util::sync::{read, write};
use rust_mcp_sdk::macros;
use rust_mcp_sdk::schema::{CallToolError, CallToolRequestParams, CallToolResult};
use serde_json::{Value, json};

/// BiDi commands that would break the instance's single shared session.
const REFUSED: &[(&str, &str)] = &[
    (
        "session.new",
        "the instance already owns Firefox's only session",
    ),
    ("session.end", "use close_instance or stop_browser"),
    ("browser.close", "use close_instance or stop_browser"),
];

#[macros::mcp_tool(
    name = "send_bidi_command",
    description = "Sends a raw WebDriver BiDi command to a Firefox instance and returns its JSON result (params are sent untouched: put the browsing context where the command expects it, e.g. {\"context\": \"<target_id from list_tabs>\"}). Events of modules subscribed here with session.subscribe are captured per tab for get_custom_events. Refuses session.new, session.end and browser.close (use close_instance/stop_browser) and, with --local, non-local browsingContext.navigate URLs. Firefox only (not available on Chrome)."
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct SendBidiCommandTool {
    /// Instance id from open_instance/list_instances. Omit for the default instance.
    pub instance_id: Option<String>,
    /// The Tab ID of the target tab. Omit to use the active tab.
    pub tab_id: Option<String>,
    /// BiDi method name, e.g. 'browsingContext.getTree' or 'script.callFunction'.
    pub method: String,
    /// Parameters as a JSON object string. Defaults to {}.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<String>,
}

#[macros::mcp_tool(
    name = "send_rdp_request",
    description = "Sends a raw Firefox Remote Debugging Protocol request (to an actor) and returns the reply packet as JSON. 'to' may be an actor id or one of the shortcuts 'thread', 'console' (resolved for the tab's debugger, attaching it if needed) and 'root'. With --local, non-local navigateTo URLs are refused. Needs the Firefox debugger port. Firefox only (not available on Chrome)."
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct SendRdpRequestTool {
    /// Instance id from open_instance/list_instances. Omit for the default instance.
    pub instance_id: Option<String>,
    /// The Tab ID of the target tab. Omit to use the active tab.
    pub tab_id: Option<String>,
    /// Actor id, or 'thread', 'console' or 'root'.
    pub to: String,
    /// The RDP request `type`, e.g. 'frames' or 'sources'.
    pub request_type: String,
    /// Parameters as a JSON object string. Defaults to {}.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<String>,
}

fn json_params(raw: &Option<String>) -> Result<Value, CallToolError> {
    match raw.as_deref().map(str::trim) {
        None | Some("") => Ok(json!({})),
        Some(text) => serde_json::from_str(text)
            .map_err(|e| CallToolError::from_message(format!("Invalid JSON in params: {e}"))),
    }
}

/// Why a raw BiDi command is refused, if it is.
pub(crate) fn refusal(session: &FirefoxSession, method: &str, params: &Value) -> Option<String> {
    if let Some((_, why)) = REFUSED.iter().find(|(m, _)| *m == method) {
        return Some(format!("`{method}` is refused: {why}"));
    }
    if method != "session.unsubscribe" {
        return None;
    }
    let own = read(&session.own_subscription).clone();
    let removes_own = own.is_some_and(|own| {
        params["subscriptions"]
            .as_array()
            .is_some_and(|ids| ids.contains(&json!(own)))
    });
    // By name, BiDi strips the events from every subscription, ours too.
    let names = params["events"].as_array().cloned().unwrap_or_default();
    let removes_pump_events = names.iter().filter_map(Value::as_str).any(|name| {
        super::pump::EVENTS
            .iter()
            .any(|e| pump_event_matches(e, name))
    });
    (removes_own || removes_pump_events).then(|| {
        "`session.unsubscribe` of the server's own subscription or events is refused".to_string()
    })
}

/// Whether the subscription name `name` (an event or a module) covers the
/// pump event `event`.
fn pump_event_matches(event: &str, name: &str) -> bool {
    event == name || event.split('.').next() == Some(name)
}

/// Remembers the modules/events an agent subscribes to, for `get_custom_events`.
fn record_subscription(session: &FirefoxSession, method: &str, params: &Value) {
    if method != "session.subscribe" {
        return;
    }
    let names = params["events"].as_array().cloned().unwrap_or_default();
    let mut modules = write(&session.custom_modules);
    for name in names.iter().filter_map(Value::as_str) {
        if !modules.iter().any(|m| m == name) {
            modules.push(name.to_string());
        }
    }
}

/// `send_bidi_command`.
pub(crate) async fn send_bidi_command(
    params: CallToolRequestParams,
    handler: &McpHandler,
) -> Result<CallToolResult, CallToolError> {
    let args: SendBidiCommandTool = parse_args(params)?;
    let bidi_params = json_params(&args.params)?;
    if handler.flags.local
        && let Some(url) = crate::util::url::raw_navigation_url(&args.method, &bidi_params)
        && !crate::handler::is_local_address(url)
    {
        return Err(CallToolError::from_message(format!(
            "Navigation to '{url}' via raw BiDi command is blocked: the server runs with --local"
        )));
    }
    let session = handler.firefox(args.instance_id.as_deref()).await?;
    if let Some(why) = refusal(&session, &args.method, &bidi_params) {
        return Err(CallToolError::from_message(why));
    }
    let (_, tab) = session.tab(args.tab_id.as_deref()).await?;
    record_subscription(&session, &args.method, &bidi_params);
    let result = super::page::command(&tab.tab, &args.method, bidi_params).await?;
    Ok(CallToolResult::text_content(vec![
        format!(
            "Command '{}' executed successfully.\n{}",
            args.method,
            serde_json::to_string_pretty(&result).unwrap_or_default()
        )
        .into(),
    ]))
}

/// `send_rdp_request`.
pub(crate) async fn send_rdp_request(
    params: CallToolRequestParams,
    handler: &McpHandler,
) -> Result<CallToolResult, CallToolError> {
    let args: SendRdpRequestTool = parse_args(params)?;
    let rdp_params = json_params(&args.params)?;
    if handler.flags.local
        && let Some(url) = crate::util::url::raw_navigation_url(&args.request_type, &rdp_params)
        && !crate::handler::is_local_address(url)
    {
        return Err(CallToolError::from_message(format!(
            "Navigation to '{url}' via raw RDP request is blocked: the server runs with --local"
        )));
    }
    let session = handler.firefox(args.instance_id.as_deref()).await?;
    let to = super::debugger::resolve_actor(&session, args.tab_id.as_deref(), &args.to).await?;
    let browser = session.browser().await?;
    let rdp = browser.rdp_client().await.map_err(super::lifecycle_error)?;
    let reply = rdp
        .request(&to, &args.request_type, rdp_params)
        .await
        .map_err(|e| super::protocol_error(&format!("RDP {}", args.request_type), &e))?;
    Ok(CallToolResult::text_content(vec![
        serde_json::to_string_pretty(&reply.body)
            .unwrap_or_default()
            .into(),
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bidi_browser_lite::test_support::{MockBidi, MockBidiBehavior};

    /// TOL-07: send_bidi_command refuses session-breaking methods.
    #[test]
    fn bidi_raw_refuses_session_breaking_methods() {
        let session = FirefoxSession::new(Default::default());
        for method in ["session.end", "session.new", "browser.close"] {
            assert!(refusal(&session, method, &json!({})).is_some(), "{method}");
        }
        assert!(refusal(&session, "browsingContext.getTree", &json!({})).is_none());
    }

    /// BID-25: The backend's own subscription cannot be removed.
    #[test]
    fn own_subscription_cannot_be_removed() {
        let session = FirefoxSession::new(Default::default());
        *write(&session.own_subscription) = Some("S1".into());
        let own = json!({"subscriptions": ["S1"]});
        assert!(refusal(&session, "session.unsubscribe", &own).is_some());
        assert!(
            refusal(
                &session,
                "session.unsubscribe",
                &json!({"subscriptions": ["S2"]})
            )
            .is_none()
        );
    }

    /// REV-08: unsubscribing the server's events by name is refused too.
    #[test]
    fn unsubscribing_the_pump_events_by_name_is_refused() {
        // Given a session (its pump listens to log and network events)
        let session = FirefoxSession::new(Default::default());
        // When an agent unsubscribes by event or module name
        let by_event = json!({"events": ["log.entryAdded"]});
        let by_module = json!({"events": ["network"]});
        let other = json!({"events": ["script.realmCreated"]});
        // Then only the names the pump needs are refused
        assert!(refusal(&session, "session.unsubscribe", &by_event).is_some());
        assert!(refusal(&session, "session.unsubscribe", &by_module).is_some());
        assert!(refusal(&session, "session.unsubscribe", &other).is_none());
    }

    /// REV-17: `--local` also covers RDP `navigateTo`, before any browser use.
    #[tokio::test]
    async fn rdp_navigate_to_honours_local() {
        // Given a server started with --local
        let mut handler = crate::handler::test_handler();
        handler.flags.local = true;
        let params = json!({"to": "thread", "request_type": "navigateTo", "params": "{\"url\": \"https://example.com/\"}"});
        let call = serde_json::from_value(json!({"name": "send_rdp_request", "arguments": params}))
            .unwrap();
        // When send_rdp_request navigates to a remote URL
        let error = send_rdp_request(call, &handler).await.unwrap_err();
        // Then it is refused
        assert!(error.to_string().contains("--local"), "{error}");
    }

    /// BID-23 / BID-24: raw commands are sent untouched; agent subscriptions are remembered.
    #[tokio::test]
    async fn raw_bidi_sends_untouched() {
        let mock = MockBidi::start(MockBidiBehavior::Ready).await.unwrap();
        let mut handler = crate::handler::test_handler();
        handler.default_browser = crate::backend::BrowserKind::Firefox;
        handler.firefox_spec.firefox.firefox_port = Some(mock.port());
        let call = |method: &str, params: &str| {
            serde_json::from_value(json!({"name": "send_bidi_command", "arguments": {"method": method, "params": params}})).unwrap()
        };
        let reply = send_bidi_command(
            call("browsingContext.getTree", "{\"maxDepth\":0}"),
            &handler,
        )
        .await
        .unwrap();
        let text = reply.content[0].as_text_content().unwrap().text.clone();
        assert!(
            text.starts_with("Command 'browsingContext.getTree' executed successfully."),
            "{text}"
        );
        assert!(text.contains("contexts"), "{text}");
        send_bidi_command(
            call(
                "session.subscribe",
                "{\"events\":[\"script.realmCreated\"]}",
            ),
            &handler,
        )
        .await
        .unwrap();
        let session = handler.firefox(None).await.unwrap();
        assert!(read(&session.custom_modules).contains(&"script.realmCreated".to_string()));
        session.stop().await.unwrap();
    }
}
