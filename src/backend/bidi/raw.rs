//! Raw protocol access on Firefox: `send_bidi_command` and `send_rdp_request`.

use super::session::FirefoxSession;
use crate::handler::cdp_domains::event_pump::ListenerHandles;
use crate::handler::{CustomState, McpHandler, parse_args};
use crate::util::sync::{read, write};
use bidi_browser_lite::bidi_lite::rdp::client::RdpClient;
use rust_mcp_sdk::macros;
use rust_mcp_sdk::schema::{CallToolError, CallToolRequestParams, CallToolResult};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio_stream::StreamExt;

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
    description = "Sends a raw WebDriver BiDi command, for what other tools do not cover, and returns its result; params go as given (include the context, e.g. {\"context\": \"<target_id from list_tabs>\"}). Modules subscribed with session.subscribe feed get_custom_events. Refuses session.new, session.end and browser.close, and with --local non-local navigations. Firefox only (not available on Chrome)."
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct SendBidiCommandTool {
    /// Default: the default instance.
    pub instance_id: Option<String>,
    /// Default: the active tab.
    pub tab_id: Option<String>,
    /// e.g. 'browsingContext.getTree'.
    pub method: String,
    /// JSON object as a string. Default: '{}'.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<String>,
}

#[macros::mcp_tool(
    name = "send_rdp_request",
    description = "Sends a raw Firefox Remote Debugging Protocol request, for what other tools do not cover, and returns the reply; the actor's later event packets (e.g. evaluationResult) go to get_custom_events as rdp.<type>. With --local, non-local navigateTo URLs are refused. Needs the Firefox debugger port. Firefox only (not available on Chrome)."
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct SendRdpRequestTool {
    /// Default: the default instance.
    pub instance_id: Option<String>,
    /// Default: the active tab.
    pub tab_id: Option<String>,
    /// Actor id, or 'thread', 'console' (the tab's debugger) or 'root'.
    pub to: String,
    /// RDP request type, e.g. 'frames' or 'sources'.
    pub request_type: String,
    /// JSON object as a string. Default: '{}'.
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
        let pump = super::pump::EVENTS
            .iter()
            .chain(super::pump::OPTIONAL_EVENTS);
        pump.clone().any(|e| pump_event_matches(e, name))
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
    // Watched before the request: its events can come with the reply.
    let tab = session.tab(args.tab_id.as_deref()).await.ok();
    if let Some((_, tab)) = tab {
        forward_rdp_events(&session, &rdp, &to, &tab.custom).await;
    }
    let reply = rdp
        .request(&to, &args.request_type, rdp_params)
        .await
        .map_err(|e| super::rdp_protocol_error(&args.request_type, &e))?;
    Ok(CallToolResult::text_content(vec![
        serde_json::to_string_pretty(&reply.body)
            .unwrap_or_default()
            .into(),
    ]))
}

/// Most actors whose event packets are forwarded at once.
const MAX_RDP_FORWARDS: usize = 16;

/// Copies the event packets `actor` sends from now on (those with a
/// `type`; replies have none) into `custom`, the addressed tab's custom
/// events, as `rdp.<type>`: an agent's request can start events no other
/// tool reads (`evaluationResult` after `evaluateJSAsync`, `pageError`
/// after `startListeners`; ISSUE-71). One task per actor, the oldest
/// dropped past [`MAX_RDP_FORWARDS`]; it holds the buffer weakly.
async fn forward_rdp_events(
    session: &FirefoxSession,
    rdp: &RdpClient,
    actor: &str,
    custom: &Arc<tokio::sync::Mutex<CustomState>>,
) {
    let mut forwards = session.rdp_forwards.lock().await;
    if forwards.iter().any(|(known, _)| known == actor) {
        return;
    }
    if forwards.len() >= MAX_RDP_FORWARDS {
        forwards.remove(0);
    }
    let mut packets = rdp.on_actor(actor);
    let custom = Arc::downgrade(custom);
    let mut handles = ListenerHandles::default();
    handles.push(tokio::spawn(async move {
        while let Some(packet) = packets.next().await {
            let Ok(packet) = packet else { continue };
            let Some(kind) = packet.r#type.as_deref() else {
                continue;
            };
            let Some(custom) = custom.upgrade() else {
                break;
            };
            let method = format!("rdp.{kind}");
            crate::handler::cdp_domains::custom::push_event(
                &mut *custom.lock().await,
                &method,
                &packet.body,
            );
        }
    }));
    forwards.push((actor.to_string(), handles));
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

    /// ISSUE-71 (Firefox): the event packets an actor sends after an
    /// agent's request to it (`evaluationResult` after `evaluateJSAsync`)
    /// reach get_custom_events as `rdp.<type>`.
    #[tokio::test]
    async fn rdp_events_after_a_request_reach_custom_events() {
        use super::super::test_support as ts;
        // Given a Firefox tab with its debugger port
        let mock = ts::firefox_with_debugger().await;
        let handler = ts::handler(&mock);
        // When the agent asks the tab's console for an async evaluation
        let params = json!({"to": "console", "request_type": "evaluateJSAsync", "params": "{\"text\": \"6*7\"}"});
        let reply = send_rdp_request(ts::call("send_rdp_request", params), &handler)
            .await
            .unwrap();
        assert!(ts::text(&reply).contains("resultID"));
        // Then its evaluationResult is among the tab's custom events
        let mut found = String::new();
        for _ in 0..100 {
            let args = json!({"filter_method": "rdp.evaluationResult"});
            let events =
                super::super::tools::custom_events(ts::call("get_custom_events", args), &handler);
            found = ts::text(&events.await.unwrap());
            if found.contains("\"total\": 1") {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(found.contains("\"total\": 1"), "{found}");
        assert!(found.contains("6*7"), "{found}");
        ts::stop(&handler).await;
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
