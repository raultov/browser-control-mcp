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
    description = "Evaluates JavaScript in the page (promises awaited). Returns {value, type, subtype?, description?}: value is the JSON.stringify form, null when there is none (functions, nodes; description says what it was), cut over 100000 characters with truncated: true. An exception fails with its message, line and column. Chrome stops a script after 8 s; Firefox cannot (close a frozen tab with 'close_tab')."
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct EvaluateJsTool {
    /// Instance id; omit for the default instance.
    pub instance_id: Option<String>,
    /// Tab id; omit for the active tab.
    pub tab_id: Option<String>,
    /// A JavaScript expression or IIFE.
    pub expression: String,
}

/// The object group of `evaluate_js`'s remote objects, released after each call.
const GROUP: &str = "browser-control-evaluate";

/// Why a promise evaluated while the page is paused has no value.
const PENDING_WHILE_PAUSED: &str = "the expression returned a promise that cannot settle while the page is paused in the debugger: call 'resume' first, or read the paused frame with 'evaluate_on_call_frame'";

impl EvaluateJsTool {
    pub async fn handle(
        params: CallToolRequestParams,
        handler: &McpHandler,
    ) -> Result<CallToolResult, CallToolError> {
        let args: EvaluateJsTool = crate::handler::parse_args(params)?;
        let target = crate::handler::resolve_target(handler, args.instance_id, args.tab_id).await?;
        // Chrome cannot await a promise while the page is paused in the
        // debugger (no microtask runs): the call would wait for the timeout.
        let paused = target.is_paused().await;
        let params = json!({"expression": args.expression, "awaitPromise": !paused});
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
/// is an error worded by `thrown`. A promise not awaited (`awaitPromise:
/// false`, the page being paused) is read from its state. Errors are text:
/// the tool error type is not `Send` across the release.
pub(crate) async fn evaluate_remote(
    target: &CdpTarget,
    method: &str,
    mut params: Value,
    thrown: fn(&Value) -> String,
) -> Result<Evaluation, String> {
    let unawaited = params["awaitPromise"] == json!(false);
    params["returnByValue"] = json!(false);
    params["objectGroup"] = json!(GROUP);
    params["timeout"] = json!(evaluation::EVAL_TIMEOUT_MS);
    let started = tokio::time::Instant::now();
    let outcome = match target.send_raw_command(method, params).await {
        Ok(reply) => {
            let result = reply.result.unwrap_or_default();
            read_outcome(target, result, thrown, unawaited).await
        }
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

/// The evaluation of a reply, or its exception as an error; an `unawaited`
/// promise stands for its value.
async fn read_outcome(
    target: &CdpTarget,
    mut result: Value,
    thrown: fn(&Value) -> String,
    unawaited: bool,
) -> Result<Evaluation, String> {
    if let Some(details) = result.get("exceptionDetails") {
        return Err(thrown(details));
    }
    let mut remote = result["result"].take();
    if unawaited && remote["subtype"] == "promise" {
        remote = settled(target, &remote).await?;
    }
    let remote = &remote;
    let by_value = match remote.get("objectId").and_then(Value::as_str) {
        Some(id) if evaluation::has_json_form(remote) => by_value(target, id).await,
        _ => None,
    };
    let Some(read) = by_value else {
        return Ok(evaluation::from_cdp(remote, None));
    };
    let text = |key: &str| remote.get(key).and_then(Value::as_str);
    let kind = text("type").unwrap_or("object");
    Ok(evaluation::from_measured(kind, text("subtype"), read))
}

/// The value of a promise that was not awaited, read from its internal
/// state: a rejection fails with its reason, and a pending promise fails
/// because it cannot settle while the page is paused.
async fn settled(target: &CdpTarget, promise: &Value) -> Result<Value, String> {
    let params = json!({"objectId": promise["objectId"], "ownProperties": true});
    let reply = target
        .send_raw_command("Runtime.getProperties", params)
        .await
        .map_err(|e| crate::handler::cdp_failure("CDP Error", &e))?
        .result
        .unwrap_or_default();
    let internal = |name: &str| {
        reply["internalProperties"]
            .as_array()
            .and_then(|slots| slots.iter().find(|slot| slot["name"] == name))
            .map(|slot| slot["value"].clone())
            .unwrap_or_default()
    };
    let value = internal("[[PromiseResult]]");
    match internal("[[PromiseState]]")["value"].as_str() {
        Some("fulfilled") => Ok(value),
        Some("rejected") => {
            let reason = evaluation::cdp_thrown(&json!({"exception": value}));
            Err(format!("{reason} (the promise rejected)"))
        }
        _ => Err(PENDING_WHILE_PAUSED.to_string()),
    }
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
        assert_eq!(evaluate["awaitPromise"], true);
        assert_eq!(evaluate["objectGroup"], GROUP);
        let read = &mock.commands_named("Runtime.callFunctionOn")[0].params;
        assert_eq!(
            (read["objectId"].as_str(), read["returnByValue"].as_bool()),
            (Some("o1"), Some(true))
        );
        let released = &mock.commands_named("Runtime.releaseObjectGroup")[0].params;
        assert_eq!(released["objectGroup"], GROUP);
    }

    /// The promise states Chrome 156 reported while paused (fixture).
    fn promise_states() -> Value {
        serde_json::from_str(include_str!(
            "../../../../tests/fixtures/cdp/runtime-promise-states.json"
        ))
        .unwrap()
    }

    /// A mock tab paused in the debugger whose `Runtime.evaluate` returns
    /// the promise `state` of the fixture.
    async fn paused_on(state: &str) -> (crate::handler::McpHandler, impl Fn(&str) -> Vec<Value>) {
        let mock = spawn_recording_mock().await;
        let case = promise_states()[state].clone();
        let evaluated = json!({"result": case["evaluate"].clone()});
        mock.respond_with("Runtime.evaluate", move |_| evaluated.clone());
        let properties = json!({"result": case["getProperties"].clone()});
        mock.respond_with("Runtime.getProperties", move |_| properties.clone());
        mock.respond_with(
            "Runtime.callFunctionOn",
            |_| json!({"result": {"result": {"type": "object", "value": {"a": 5}}}}),
        );
        let handler = mock_handler(mock.http_port);
        let session = handler.session(None).await.unwrap();
        let debugger = session.debugger_state(None).unwrap();
        debugger.lock().await.paused_call_frame_id = Some("f1".into());
        let sent = move |method: &str| {
            mock.commands_named(method)
                .into_iter()
                .map(|c| c.params)
                .collect()
        };
        (handler, sent)
    }

    /// ISSUE-16: while the page is paused in the debugger Chrome cannot
    /// await a promise (no microtask runs), so the evaluation does not
    /// await, and a settled promise is read from its state.
    #[tokio::test]
    async fn paused_page_evaluates_without_awaiting() {
        // Given a tab paused in the debugger, and an expression giving a fulfilled promise
        let (handler, sent) = paused_on("fulfilled").await;
        // When evaluate_js runs it
        let args = json!({"expression": "Promise.resolve({a: 5})"});
        let reply = EvaluateJsTool::handle(call("evaluate_js", args), &handler)
            .await
            .unwrap();
        // Then it was not awaited, and the reply is the promise's value
        assert_eq!(sent("Runtime.evaluate")[0]["awaitPromise"], false);
        let body: Value = serde_json::from_str(&reply_text(&reply)).unwrap();
        assert_eq!(body, json!({"value": {"a": 5}, "type": "object"}));
        // And the value was read from the promise's result
        let read = &sent("Runtime.callFunctionOn")[0];
        let result = &promise_states()["fulfilled"]["getProperties"]["internalProperties"][2];
        assert_eq!(read["objectId"], result["value"]["objectId"]);
    }

    /// ISSUE-16: while paused, a rejected promise fails with its reason and
    /// a pending one fails at once saying why it cannot settle.
    #[tokio::test]
    async fn paused_page_explains_unsettled_promises() {
        // Given a tab paused in the debugger, and a rejected promise
        let (handler, _) = paused_on("rejected").await;
        // When evaluate_js runs it, then it fails with the rejection
        let args = json!({"expression": "Promise.reject(new Error('no'))"});
        let error = EvaluateJsTool::handle(call("evaluate_js", args), &handler)
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "Error: no (the promise rejected)");
        // Given a pending promise instead
        let (handler, _) = paused_on("pending").await;
        // When evaluate_js runs it, then it fails at once saying why
        let args = json!({"expression": "new Promise(() => {})"});
        let run = EvaluateJsTool::handle(call("evaluate_js", args), &handler);
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(2), run).await;
        let error = outcome.expect("answers at once").unwrap_err().to_string();
        assert!(error.contains("paused in the debugger"), "{error}");
        assert!(error.contains("'resume'"), "{error}");
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

    /// AUD-EV-02: an object that is not a plain one is named by its class.
    #[tokio::test]
    async fn class_instances_are_named() {
        let mock = spawn_recording_mock().await;
        mock.respond_with("Runtime.evaluate", |_| {
            json!({"result": {"result": {"type": "object", "className": "Navigator", "description": "Navigator", "objectId": "o1"}}})
        });
        mock.respond_with("Runtime.callFunctionOn", |_| {
            json!({"result": {"result": {"type": "object", "value": {"__bcm_class__": "Navigator", "__bcm_value__": {}}}}})
        });
        let handler = mock_handler(mock.http_port);
        let reply = EvaluateJsTool::handle(
            call("evaluate_js", json!({"expression": "navigator"})),
            &handler,
        )
        .await
        .unwrap();
        let body: Value = serde_json::from_str(&reply_text(&reply)).unwrap();
        assert_eq!(
            body,
            json!({"value": {}, "type": "object", "description": "Navigator"})
        );
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
