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
    description = "Executes arbitrary JavaScript code in the page context, returning evaluated results as JSON-serializable values. Side effects: may modify DOM, state, or trigger network requests. Prerequisites: requires an active Chrome tab; script context must allow execution. Returns: JSON {value, type, subtype?, description?}: `type` is JavaScript's typeof, `value` the JSON form (null when there is none: functions, DOM nodes, symbols, NaN, bigints, maps...; then `description` says what it was). A value over 100000 characters is cut (strings) or only described (objects, measured in the page), with `truncated: true`. A thrown exception or rejected promise fails the call with what was thrown and its line and column. A script running over 8 s (an endless loop) is stopped on Chrome; Firefox cannot stop it, and a frozen tab is closed with close_tab. Use this for dynamic inspection, DOM manipulation, complex interactions. Alternatives: 'inspect_dom' for read-only DOM queries, 'click_element' for UI interactions."
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
        let params = json!({"expression": args.expression, "awaitPromise": true});
        let evaluated = evaluate_remote(
            &target,
            "Runtime.evaluate",
            params,
            evaluation::cdp_exception,
        )
        .await;
        evaluated
            .map(|evaluation| evaluation.reply())
            .map_err(CallToolError::from_message)
    }
}

/// Runs an evaluation command (`Runtime.evaluate`,
/// `Debugger.evaluateOnCallFrame`) by reference, so functions, nodes and
/// symbols keep their description, then reads objects that have a JSON form
/// by value; the remote objects are released afterwards. A thrown exception
/// is an error worded by `thrown`. Errors are text: the tool error type is
/// not `Send` across the release.
pub(crate) async fn evaluate_remote(
    target: &CdpTarget,
    method: &str,
    mut params: Value,
    thrown: fn(&Value) -> String,
) -> Result<Evaluation, String> {
    params["returnByValue"] = json!(false);
    params["objectGroup"] = json!(GROUP);
    params["timeout"] = json!(evaluation::EVAL_TIMEOUT_MS);
    let started = tokio::time::Instant::now();
    let outcome = match target.send_raw_command(method, params).await {
        Ok(reply) => read_outcome(target, reply.result.unwrap_or_default(), thrown).await,
        // A dialog blocks the page: releasing would wait for a timeout.
        Err(e) if crate::handler::dialogs::interrupted(&e) => {
            return Err(crate::handler::cdp_failure("CDP Error", &e));
        }
        Err(e) => Err(evaluation_failure(&e, started.elapsed())),
    };
    let release = json!({"objectGroup": GROUP});
    let _ = target
        .send_raw_command("Runtime.releaseObjectGroup", release)
        .await;
    outcome
}

/// A failed evaluation command (after `elapsed`) as the agent should read
/// it: a script Chrome stopped after `EVAL_TIMEOUT_MS`, or no result in
/// time, say so. Chrome reports a stopped script as "Execution was
/// terminated", or as a bare "Internal error" when the expression awaited a
/// promise (Chrome 156), hence the elapsed time.
fn evaluation_failure(error: &cdp_browser_lite::CdpError, elapsed: std::time::Duration) -> String {
    let limit = std::time::Duration::from_millis(evaluation::EVAL_TIMEOUT_MS);
    match error {
        cdp_browser_lite::CdpError::ProtocolError { message, .. }
            if message.contains("Execution was terminated") || elapsed >= limit =>
        {
            evaluation::STOPPED.to_string()
        }
        cdp_browser_lite::CdpError::Timeout { .. } => evaluation::NO_RESULT.to_string(),
        other => crate::handler::cdp_failure("CDP Error", other),
    }
}

/// The evaluation of a reply, or its exception as an error.
async fn read_outcome(
    target: &CdpTarget,
    result: Value,
    thrown: fn(&Value) -> String,
) -> Result<Evaluation, String> {
    if let Some(details) = result.get("exceptionDetails") {
        return Err(thrown(details));
    }
    let remote = &result["result"];
    let by_value = match remote.get("objectId").and_then(Value::as_str) {
        Some(id) if evaluation::has_json_form(remote) => by_value(target, id).await,
        _ => None,
    };
    if let Some(chars) = by_value.as_ref().and_then(evaluation::measured_length) {
        let text = |key: &str| remote.get(key).and_then(Value::as_str);
        let kind = text("type").unwrap_or("object");
        return Ok(Evaluation::too_large(kind, text("subtype"), chars));
    }
    Ok(evaluation::from_cdp(remote, by_value))
}

/// The by-value form of a remote object, or `None` when Chrome cannot
/// serialise it (e.g. `window`: "Object reference chain is too long"). The
/// object is measured in the page first ([`evaluation::MEASURE`]).
async fn by_value(target: &CdpTarget, object_id: &str) -> Option<Value> {
    let params = json!({
        "objectId": object_id,
        "functionDeclaration": evaluation::MEASURE,
        "arguments": [{"value": evaluation::EVAL_MAX_CHARS}],
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

    /// AUD-EV-06: Chrome stops a script that runs too long (an endless loop
    /// left the tab dead), and the error says what happened.
    #[tokio::test]
    async fn long_running_scripts_are_stopped() {
        // Given a page where the expression never ends
        let mock = spawn_recording_mock().await;
        mock.respond_with(
            "Runtime.evaluate",
            |_| json!({"error": {"code": -32000, "message": "Execution was terminated"}}),
        );
        let handler = mock_handler(mock.http_port);
        // When evaluate_js runs it
        let args = json!({"expression": "while (true) {}"});
        let error = EvaluateJsTool::handle(call("evaluate_js", args), &handler)
            .await
            .unwrap_err()
            .to_string();
        // Then Chrome was told to stop it before the command timeout
        let sent = &mock.commands_named("Runtime.evaluate")[0].params;
        let timeout = crate::handler::evaluation::EVAL_TIMEOUT_MS;
        assert_eq!(sent["timeout"], timeout);
        assert!(timeout < 10_000);
        // And the error says it was stopped and the page still works
        assert!(error.contains("ran for more than 8 s"), "{error}");
        assert!(error.contains("the page keeps working"), "{error}");
    }

    /// AUD-EV-06: with `awaitPromise`, Chrome reports a stopped script as
    /// a bare "Internal error" once the time limit passed.
    #[test]
    fn stopped_awaited_script_is_explained() {
        let internal = cdp_browser_lite::CdpError::ProtocolError {
            code: -32603,
            message: "Internal error".into(),
        };
        let late = std::time::Duration::from_millis(crate::handler::evaluation::EVAL_TIMEOUT_MS);
        assert_eq!(
            evaluation_failure(&internal, late),
            crate::handler::evaluation::STOPPED
        );
        let early = evaluation_failure(&internal, std::time::Duration::from_millis(5));
        assert!(early.contains("Internal error"), "{early}");
    }

    /// AUD-EV-12: the by-value read measures the object in the page; one
    /// over the limit is described, never sent whole.
    #[tokio::test]
    async fn large_objects_are_measured_in_the_page() {
        let mock = spawn_recording_mock().await;
        mock.respond_with("Runtime.evaluate", |_| {
            json!({"result": {"result": {"type": "object", "className": "Object", "description": "Object", "objectId": "o1"}}})
        });
        mock.respond_with("Runtime.callFunctionOn", |_| {
            json!({"result": {"result": {"type": "object", "value": {"__bcm_too_large__": 25_000_000}}}})
        });
        let handler = mock_handler(mock.http_port);
        let args = json!({"expression": "big"});
        let reply = EvaluateJsTool::handle(call("evaluate_js", args), &handler)
            .await
            .unwrap();
        let body: Value = serde_json::from_str(&reply_text(&reply)).unwrap();
        assert_eq!(body["value"], Value::Null);
        assert_eq!(body["truncated"], true);
        assert!(
            body["description"].as_str().unwrap().contains("25000000"),
            "{body}"
        );
        let read = &mock.commands_named("Runtime.callFunctionOn")[0].params;
        assert!(
            read["functionDeclaration"]
                .as_str()
                .unwrap()
                .contains("JSON.stringify")
        );
        assert_eq!(
            read["arguments"][0]["value"],
            crate::handler::evaluation::EVAL_MAX_CHARS
        );
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
