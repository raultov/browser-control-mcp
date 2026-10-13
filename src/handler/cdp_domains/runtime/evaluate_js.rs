use crate::handler::McpHandler;
use crate::handler::cdp_domains::cdp_target::CdpTarget;
use crate::handler::evaluation::{self, Evaluation};
use rust_mcp_sdk::{
    macros,
    schema::{CallToolError, CallToolRequestParams, CallToolResult},
};
use serde_json::{Value, json};

#[macros::mcp_tool(
    name = "evaluate_js",
    description = "Executes arbitrary JavaScript code in the page context, returning evaluated results as JSON-serializable values. Side effects: may modify DOM, state, or trigger network requests. Prerequisites: requires an active Chrome tab; script context must allow execution. Returns: JSON {value, type, subtype?, description?}: `type` is JavaScript's typeof, `value` the JSON form (null when there is none: functions, DOM nodes, symbols, NaN, bigints, maps...; then `description` says what it was). A thrown exception or rejected promise fails the call with what was thrown and its line and column. Use this for dynamic inspection, DOM manipulation, complex interactions. Alternatives: 'inspect_dom' for read-only DOM queries, 'click_element' for UI interactions."
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct EvaluateJsTool {
    /// Chrome instance id from open_instance/list_instances. Omit for the default instance.
    pub instance_id: Option<String>,
    /// The Tab ID of the target tab. Omit to use the active tab.
    pub tab_id: Option<String>,
    /// JavaScript code expression to execute. Constraints: valid JavaScript (single expression or IIFE). Interactions: automatically awaits promises. Defaults to: None (required).
    pub expression: String,
}

/// The object group of `evaluate_js`'s remote objects, released after each call.
const GROUP: &str = "browser-control-evaluate";

impl EvaluateJsTool {
    pub async fn handle(
        params: CallToolRequestParams,
        handler: &McpHandler,
    ) -> Result<CallToolResult, CallToolError> {
        let args: EvaluateJsTool = crate::handler::parse_args(params)?;
        let target = crate::handler::resolve_target(handler, args.instance_id, args.tab_id).await?;
        // As text: the error type is not `Send` across the release below.
        let outcome = evaluate(&target, &args.expression)
            .await
            .map_err(|e| e.to_string());
        let release = json!({"objectGroup": GROUP});
        let _ = target
            .send_raw_command("Runtime.releaseObjectGroup", release)
            .await;
        outcome
            .map(|evaluation| evaluation.reply())
            .map_err(CallToolError::from_message)
    }
}

/// Evaluates by reference (so functions, nodes and symbols keep their
/// description), then reads objects that have a JSON form by value.
async fn evaluate(target: &CdpTarget, expression: &str) -> Result<Evaluation, CallToolError> {
    let params = json!({
        "expression": expression,
        "returnByValue": false,
        "awaitPromise": true,
        "objectGroup": GROUP,
    });
    let reply = crate::handler::send_or(target, "Runtime.evaluate", params, "CDP Error").await?;
    let result = reply.result.unwrap_or_default();
    if let Some(details) = result.get("exceptionDetails") {
        return Err(CallToolError::from_message(evaluation::cdp_exception(
            details,
        )));
    }
    let remote = &result["result"];
    let by_value = match remote.get("objectId").and_then(Value::as_str) {
        Some(id) if evaluation::has_json_form(remote["subtype"].as_str()) => {
            by_value(target, id).await
        }
        _ => None,
    };
    Ok(evaluation::from_cdp(remote, by_value))
}

/// The by-value form of a remote object, or `None` when Chrome cannot
/// serialise it (e.g. `window`: "Object reference chain is too long").
async fn by_value(target: &CdpTarget, object_id: &str) -> Option<Value> {
    let params = json!({
        "objectId": object_id,
        "functionDeclaration": "function () { return this; }",
        "returnByValue": true,
    });
    let reply = target
        .send_raw_command("Runtime.callFunctionOn", params)
        .await
        .ok()?
        .result?;
    if reply.get("exceptionDetails").is_some() {
        return None;
    }
    reply.get("result")?.get("value").cloned()
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
    async fn test_evaluate_js_params_deserialization() {
        let params: Result<CallToolRequestParams, _> = serde_json::from_value(json!({
            "name": "evaluate_js",
            "arguments": {
                "expression": "console.log('test')"
            }
        }));
        assert!(params.is_ok());
    }

    #[tokio::test]
    async fn test_evaluate_js_tool_deserialization() {
        let tool: Result<EvaluateJsTool, _> = serde_json::from_value(json!({
            "expression": "2 + 2"
        }));
        assert!(tool.is_ok());
        assert_eq!(tool.unwrap().expression, "2 + 2");
    }

    #[tokio::test]
    async fn test_evaluate_js_missing_expression_fails() {
        let handler = McpHandler::new_test();
        let params: CallToolRequestParams = serde_json::from_value(json!({
            "name": "evaluate_js",
            "arguments": {}
        }))
        .unwrap();

        let result = EvaluateJsTool::handle(params, &handler).await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("missing field `expression`")
        );
    }

    #[tokio::test]
    async fn test_evaluate_js_handle() {
        let port = spawn_mock_chrome_server().await;

        let mut handler = McpHandler::new_test();
        Arc::get_mut(&mut handler.default_session)
            .unwrap()
            .chrome_manager = Arc::new(Mutex::new(MockChromeManager::new(port)));

        let params: CallToolRequestParams = serde_json::from_value(json!({
            "name": "evaluate_js",
            "arguments": {
                "expression": "2 + 2"
            }
        }))
        .unwrap();

        let result = EvaluateJsTool::handle(params, &handler).await;
        assert!(result.is_ok(), "Handle should succeed: {:?}", result.err());

        let call_result = result.unwrap();
        assert!(!call_result.content.is_empty());
        // TOL-08: the reply is JSON, not the Rust Debug of the protocol message
        let text = call_result.content[0]
            .as_text_content()
            .unwrap()
            .text
            .clone();
        assert!(!text.contains("WsResponse"), "{text}");
        assert!(
            serde_json::from_str::<serde_json::Value>(&text).is_ok(),
            "{text}"
        );
    }

    // A responder reply with a "result" key is sent as the whole message,
    // hence the double "result" below.
    use crate::handler::cdp_domains::tests::{
        call, mock_handler, reply_text, spawn_recording_mock,
    };

    /// EVL-02: an object is evaluated by reference, read by value, and the
    /// remote objects are released.
    #[tokio::test]
    async fn object_result_is_read_by_value_and_released() {
        // Given a page where the expression is a plain object
        let mock = spawn_recording_mock().await;
        mock.respond_with("Runtime.evaluate", |_| {
            json!({"result": {"result": {"type": "object", "className": "Object", "description": "Object", "objectId": "o1"}}})
        });
        mock.respond_with(
            "Runtime.callFunctionOn",
            |_| json!({"result": {"result": {"type": "object", "value": {"a": 1}}}}),
        );
        let handler = mock_handler(mock.http_port);
        // When evaluate_js runs
        let reply = EvaluateJsTool::handle(
            call("evaluate_js", json!({"expression": "({a: 1})"})),
            &handler,
        )
        .await
        .unwrap();
        // Then the reply has the value in the shared shape
        let body: Value = serde_json::from_str(&reply_text(&reply)).unwrap();
        assert_eq!(body, json!({"value": {"a": 1}, "type": "object"}));
        // And the wire params were by reference, then by value, then released
        let evaluate = &mock.commands_named("Runtime.evaluate")[0].params;
        assert_eq!(evaluate["returnByValue"], false);
        assert_eq!(evaluate["objectGroup"], GROUP);
        let read = &mock.commands_named("Runtime.callFunctionOn")[0].params;
        assert_eq!(
            (read["objectId"].as_str(), read["returnByValue"].as_bool()),
            (Some("o1"), Some(true))
        );
        let released = &mock.commands_named("Runtime.releaseObjectGroup")[0].params;
        assert_eq!(released["objectGroup"], GROUP);
    }

    /// EVL-03 / EVL-04: a node is described without a by-value read; a
    /// thrown error fails with what was thrown and still releases.
    #[tokio::test]
    async fn nodes_are_described_and_exceptions_fail() {
        // Given a page answering a DOM node
        let mock = spawn_recording_mock().await;
        let objects: Value = serde_json::from_str(include_str!(
            "../../../../tests/fixtures/cdp/runtime-evaluate-remote-objects.json"
        ))
        .unwrap();
        let node = objects["node"].clone();
        mock.respond_with(
            "Runtime.evaluate",
            move |_| json!({"result": {"result": node.clone()}}),
        );
        let handler = mock_handler(mock.http_port);
        // When evaluate_js returns it
        let args = json!({"expression": "document.getElementById('t')"});
        let reply = EvaluateJsTool::handle(call("evaluate_js", args), &handler)
            .await
            .unwrap();
        // Then it is described and not read by value
        let body: Value = serde_json::from_str(&reply_text(&reply)).unwrap();
        assert_eq!(body["description"], "div#t.a.b");
        assert!(mock.commands_named("Runtime.callFunctionOn").is_empty());
        // And a thrown error is a tool error
        let thrown: Value = serde_json::from_str(include_str!(
            "../../../../tests/fixtures/cdp/runtime-evaluate-exception.json"
        ))
        .unwrap();
        let reply = json!({"result": thrown["result"].clone()});
        mock.respond_with("Runtime.evaluate", move |_| reply.clone());
        let args = json!({"expression": "x_not_defined"});
        let error = EvaluateJsTool::handle(call("evaluate_js", args), &handler)
            .await
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "ReferenceError: x_not_defined is not defined (line 1, column 1)"
        );
        assert_eq!(mock.commands_named("Runtime.releaseObjectGroup").len(), 2);
    }
}
