//! Test helpers: a Firefox handler attached to the lifecycle crate's mocks,
//! so tool tests assert the exact BiDi and RDP traffic without a browser.

use crate::backend::BrowserKind;
use crate::handler::McpHandler;
use bidi_browser_lite::test_support::MockFirefox;
use rust_mcp_sdk::schema::{CallToolRequestParams, CallToolResult};
use serde_json::{Value, json};

/// How long a test waits for traffic that follows a call asynchronously.
pub(crate) const WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// A handler whose default instance is Firefox, attached to `mock` (BiDi,
/// plus RDP when the mock has it).
pub(crate) fn handler(mock: &MockFirefox) -> McpHandler {
    let mut handler = crate::handler::test_handler();
    handler.default_browser = BrowserKind::Firefox;
    handler.firefox_spec.firefox.firefox_port = Some(mock.bidi_port());
    handler.firefox_spec.firefox.firefox_rdp_port = mock.rdp_port();
    handler.firefox_spec.firefox.firefox_path = Some("/nonexistent/firefox".into());
    handler
}

/// `browserId` of the one tab the RDP mock lists by default.
pub(crate) const FIRST_BROWSER_ID: u64 = 2;

/// A mock Firefox with its debugger port, whose first BiDi context (`ctx-0`)
/// is the tab RDP lists (Firefox's Remote Agent lookup maps it), so the
/// session attaches its debugger as on a real Firefox.
pub(crate) async fn firefox_with_debugger() -> MockFirefox {
    let mock = MockFirefox::start(true).await.unwrap();
    if let Some(rdp) = mock.rdp() {
        rdp.map_context("ctx-0", FIRST_BROWSER_ID);
    }
    mock
}

/// A tool call named `name` with `arguments`.
pub(crate) fn call(name: &str, arguments: Value) -> CallToolRequestParams {
    serde_json::from_value(json!({"name": name, "arguments": arguments})).unwrap()
}

/// The text of the first content block.
pub(crate) fn text(result: &CallToolResult) -> String {
    result.content[0].as_text_content().unwrap().text.clone()
}

/// The JSON object a reply carries after its first line (or the whole
/// text when it is JSON).
pub(crate) fn json_of(result: &CallToolResult) -> Value {
    let text = text(result);
    let start = text.find('{').unwrap_or(0);
    serde_json::from_str(&text[start..]).unwrap()
}

/// A full `script.evaluate` success message for `MockBidi::respond_with`
/// (a responder reply with a `type` is sent as the whole message).
pub(crate) fn evaluate_success(remote: Value) -> Value {
    json!({"type": "success", "result": {"type": "success", "result": remote, "realm": "r1"}})
}

/// `value` as a BiDi remote value, so a mock can answer a `util::js`
/// snippet with the plain JSON the page would return.
pub(crate) fn remote(value: &Value) -> Value {
    match value {
        Value::Null => json!({"type": "null"}),
        Value::Bool(b) => json!({"type": "boolean", "value": b}),
        Value::Number(n) => json!({"type": "number", "value": n}),
        Value::String(s) => json!({"type": "string", "value": s}),
        Value::Array(items) => {
            json!({"type": "array", "value": items.iter().map(remote).collect::<Vec<_>>()})
        }
        Value::Object(map) => {
            let entries: Vec<Value> = map.iter().map(|(k, v)| json!([k, remote(v)])).collect();
            json!({"type": "object", "value": entries})
        }
    }
}

/// Stops the handler's default Firefox session (one BiDi session per mock).
pub(crate) async fn stop(handler: &McpHandler) {
    handler.firefox(None).await.unwrap().stop().await.unwrap();
}
