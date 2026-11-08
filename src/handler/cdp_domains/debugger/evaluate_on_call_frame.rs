use crate::handler::McpHandler;
use crate::handler::{debugging, evaluation};
use rust_mcp_sdk::{
    macros,
    schema::{CallToolError, CallToolRequestParams, CallToolResult},
};
use serde_json::json;

#[macros::mcp_tool(
    name = "evaluate_on_call_frame",
    description = "Evaluates JavaScript in a paused frame (the top one, or one from call_stack) with its locals and closures in scope; outside a pause use evaluate_js. On Chrome 156+ 'scope' picks one scope of the chain (as call_stack lists them), to read a variable an inner one hides; Chrome drops unused closure variables, so a closure scope may be missing; Firefox evaluates only in the innermost scope. Returns the evaluate_js shape; an exception fails with 'Evaluation threw: <what>'."
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct EvaluateOnCallFrameTool {
    /// Default: 'default'.
    pub instance_id: Option<String>,
    /// Default: the active tab.
    pub tab_id: Option<String>,
    /// A JavaScript expression.
    pub expression: String,
    /// The frame, as call_stack numbers them. Default: 0, the innermost.
    pub frame_index: Option<u32>,
    /// Chrome 156+: the scope of the frame's chain, as call_stack lists them. Default: 0, the innermost.
    pub scope: Option<u32>,
}

impl EvaluateOnCallFrameTool {
    /// The frame index asked for (0 by default).
    pub(crate) fn index(&self) -> usize {
        self.frame_index
            .map_or(0, |i| usize::try_from(i).unwrap_or(usize::MAX))
    }
}

/// The `scopeNumber` to send for `scope` in a frame whose chain is
/// `scopes` (empty: unknown; at [`debugging::MAX_SCOPES`], cut: Chrome
/// decides past it) on a Chrome of `major` version: none for the innermost;
/// an error past the chain, or before Chrome 156 or with no version known,
/// as an older Chrome ignores the parameter and would answer from the
/// innermost scope without a word (ISSUE-100).
pub(crate) fn scope_number(
    scope: Option<u32>,
    scopes: &[debugging::Scope],
    major: Option<u32>,
) -> Result<Option<u32>, String> {
    let Some(scope) = scope.filter(|s| *s > 0) else {
        return Ok(None);
    };
    match major {
        Some(major) if major >= 156 => {}
        Some(major) => {
            return Err(format!(
                "'scope' needs Chrome 156 or later (this one is {major}): an older Chrome evaluates in the innermost scope whatever it is told"
            ));
        }
        None => {
            return Err("'scope' needs Chrome 156 or later, and this browser's version could not be read: omit it, or pass 0".into());
        }
    }
    let past = usize::try_from(scope).map_or(true, |s| s >= scopes.len());
    let cut = scopes.len() >= debugging::MAX_SCOPES;
    if !scopes.is_empty() && past && !cut {
        let listed: Vec<String> = scopes
            .iter()
            .enumerate()
            .map(|(index, s)| format!("{index} {}", s.kind))
            .collect();
        return Err(format!(
            "scope {scope} is past the frame's chain, which has {} scopes ({})",
            scopes.len(),
            listed.join(", ")
        ));
    }
    Ok(Some(scope))
}

/// The error of a `frame_index` past the paused stack's `count` frames.
pub(crate) fn no_such_frame(index: usize, count: usize) -> String {
    format!(
        "frame_index {index} is past the paused stack, which has {count} frame(s): call_stack lists them"
    )
}

impl EvaluateOnCallFrameTool {
    pub async fn handle(
        params: CallToolRequestParams,
        handler: &McpHandler,
    ) -> Result<CallToolResult, CallToolError> {
        let args_value = serde_json::Value::Object(params.arguments.unwrap_or_default());
        let args: EvaluateOnCallFrameTool = serde_json::from_value(args_value)
            .map_err(|e| CallToolError::from_message(e.to_string()))?;
        let session = handler.session(args.instance_id.clone()).await?;
        let debugger_state = session.debugger_state(args.tab_id.clone())?;

        // Validation: check if we have a paused call frame ID FIRST. Reading
        // the per-tab state needs no connection, so this stays cheap.
        let (call_frame_id, scopes) = {
            let state = debugger_state.lock().await;
            let top = state
                .paused_call_frame_id
                .clone()
                .ok_or_else(|| CallToolError::from_message(debugging::NOT_PAUSED))?;
            let id = match args.index() {
                0 => top,
                index => {
                    let ids = &state.paused_frame_ids;
                    let id = ids.get(index).cloned();
                    id.ok_or_else(|| CallToolError::from_message(no_such_frame(index, ids.len())))?
                }
            };
            let stack = state.paused_stack.as_ref();
            let frame = stack.and_then(|stack| stack.frames.get(args.index()));
            (id, frame.map(|f| f.scopes.clone()).unwrap_or_default())
        };

        // Connected first: the browser's version is read on connecting.
        let target = session.target(args.tab_id.clone()).await?;
        let major = session.browser_version.lock().await.as_ref().and_then(|v| {
            v.product
                .as_deref()
                .and_then(crate::handler::parse_chrome_major)
        });
        let scope =
            scope_number(args.scope, &scopes, major).map_err(CallToolError::from_message)?;
        let paused = debugger_state.lock().await.paused_target(&target);
        let mut params = json!({"callFrameId": call_frame_id, "expression": args.expression});
        if let Some(scope) = scope {
            params["scopeNumber"] = json!(scope);
        }
        let evaluated = crate::handler::cdp_domains::runtime::evaluate_js::evaluate_remote(
            &paused,
            "Debugger.evaluateOnCallFrame",
            params,
            |details| debugging::frame_exception(&evaluation::cdp_thrown(details)),
        )
        .await;
        evaluated
            .map(|evaluation| evaluation.reply())
            .map_err(CallToolError::from_message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_mcp_sdk::schema::CallToolRequestParams;

    #[tokio::test]
    async fn test_evaluate_on_call_frame_no_frame_error() {
        let handler = McpHandler::new_test();
        let params: CallToolRequestParams = serde_json::from_value(json!({
            "name": "evaluate_on_call_frame",
            "arguments": {
                "expression": "1 + 1"
            }
        }))
        .unwrap();

        let result = EvaluateOnCallFrameTool::handle(params, &handler).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.to_string(), crate::handler::debugging::NOT_PAUSED);
    }

    /// Registers a real tab over the mock DevTools server so that explicit
    /// `tab_id` resolution routes state reads to the tab entry.
    async fn register_mock_tab(handler: &crate::handler::McpHandler) -> String {
        use cdp_browser_lite::BrowserClient;
        use std::time::Duration;

        let session = handler.session(None).await.expect("default session");
        let port = crate::handler::cdp_domains::tests::spawn_mock_chrome_server().await;
        let browser =
            BrowserClient::connect(&format!("127.0.0.1:{}", port), Duration::from_secs(5))
                .await
                .expect("BrowserClient connect to mock");
        let tab = browser.attach("T-page-1").await.expect("attach to mock");
        session
            .tabs
            .write()
            .unwrap()
            .register_tab(
                tab,
                crate::handler::chrome_instance::tab_registry::NewTab::new(
                    None,
                    "https://example.test",
                ),
                false,
            )
            .expect("register tab")
    }

    #[tokio::test]
    async fn test_evaluate_on_call_frame_reads_tab_state_not_session_state() {
        let handler = McpHandler::new_test();
        let session = handler.session(None).await.unwrap();

        // Paused frame only in the SESSION state (would satisfy the old code).
        session
            .debugger_state(None)
            .unwrap()
            .lock()
            .await
            .paused_call_frame_id = Some("SESSION-CF".to_string());

        // Tab registered but its debugger state has NO paused frame.
        let tab_id = register_mock_tab(&handler).await;

        let params: CallToolRequestParams = serde_json::from_value(json!({
            "name": "evaluate_on_call_frame",
            "arguments": { "expression": "1 + 1", "tab_id": tab_id }
        }))
        .unwrap();

        let err = EvaluateOnCallFrameTool::handle(params, &handler)
            .await
            .unwrap_err();
        assert!(
            err.to_string() == crate::handler::debugging::NOT_PAUSED,
            "must read the TAB state (which has no paused frame), got: {err}"
        );
    }

    #[tokio::test]
    async fn test_evaluate_on_call_frame_unknown_tab_errors() {
        let handler = McpHandler::new_test();
        let params: CallToolRequestParams = serde_json::from_value(json!({
            "name": "evaluate_on_call_frame",
            "arguments": { "expression": "1 + 1", "tab_id": "tab-nope" }
        }))
        .unwrap();
        let err = EvaluateOnCallFrameTool::handle(params, &handler)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("not found"),
            "expected tab-not-found error, got: {err}"
        );
    }

    /// ISSUE-67 (Chrome): `frame_index` evaluates in a caller's frame (its
    /// call frame id from the pause); one past the stack fails naming it.
    #[tokio::test]
    async fn frame_index_picks_a_frame_of_the_stack() {
        use crate::handler::cdp_domains::tests::{call, mock_handler, spawn_recording_mock};
        // Given a pause two frames deep
        let mock = spawn_recording_mock().await;
        let handler = mock_handler(mock.http_port);
        let session = handler.session(None).await.unwrap();
        {
            let state = session.debugger_state(None).unwrap();
            let mut state = state.lock().await;
            state.paused_call_frame_id = Some("f0".into());
            state.paused_frame_ids = vec!["f0".into(), "f1".into()];
        }
        // When the caller's frame is asked for, Then its id is sent
        let args = json!({"expression": "cart", "frame_index": 1});
        let _ =
            EvaluateOnCallFrameTool::handle(call("evaluate_on_call_frame", args), &handler).await;
        let sent = mock.commands_named("Debugger.evaluateOnCallFrame");
        assert_eq!(sent[0].params["callFrameId"], "f1");
        // And one past the stack fails naming its size
        let args = json!({"expression": "cart", "frame_index": 5});
        let error = EvaluateOnCallFrameTool::handle(call("evaluate_on_call_frame", args), &handler)
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(error, no_such_frame(5, 2));
    }

    /// ISSUE-100: the scope asked for is checked against the frame's chain
    /// and the Chrome version: none is sent for the innermost; one past the
    /// chain fails naming its scopes; an older Chrome (which ignores it)
    /// refuses it, and so does a browser whose version is unknown; an
    /// unknown chain, or one cut at MAX_SCOPES, lets Chrome decide.
    #[test]
    fn scopes_are_checked_against_the_chain_and_the_version() {
        use crate::handler::debugging::Scope;
        let scope = |kind: &str| Scope {
            kind: kind.into(),
            name: None,
        };
        let chain = [scope("local"), scope("closure"), scope("global")];
        assert_eq!(scope_number(None, &chain, Some(156)), Ok(None));
        assert_eq!(scope_number(Some(0), &chain, Some(155)), Ok(None));
        assert_eq!(scope_number(Some(1), &chain, Some(156)), Ok(Some(1)));
        let past = scope_number(Some(3), &chain, Some(156)).unwrap_err();
        assert!(
            past.contains("3 scopes (0 local, 1 closure, 2 global)"),
            "{past}"
        );
        let old = scope_number(Some(1), &chain, Some(155)).unwrap_err();
        assert!(old.contains("Chrome 156"), "{old}");
        // An unknown chain lets Chrome decide; an unknown version refuses
        assert_eq!(scope_number(Some(4), &[], Some(156)), Ok(Some(4)));
        let unknown = scope_number(Some(1), &chain, None).unwrap_err();
        assert!(unknown.contains("could not be read"), "{unknown}");
        // Past a chain cut at MAX_SCOPES, Chrome decides
        let long = vec![scope("block"); crate::handler::debugging::MAX_SCOPES];
        assert_eq!(scope_number(Some(40), &long, Some(156)), Ok(Some(40)));
    }

    /// ISSUE-100 (Chrome): `scope` reaches Chrome as `scopeNumber` for the
    /// frame's chain in the pause the tab keeps.
    #[tokio::test]
    async fn scope_is_sent_as_the_scope_number() {
        use crate::handler::cdp_domains::tests::{call, mock_handler, spawn_recording_mock};
        use crate::handler::debugging::{CallStack, PauseLocation, Scope, StackFrame};
        // Given a pause in a frame with a local and a closure scope
        let mock = spawn_recording_mock().await;
        let handler = mock_handler(mock.http_port);
        let session = handler.session(None).await.unwrap();
        {
            let state = session.debugger_state(None).unwrap();
            let mut state = state.lock().await;
            state.paused_call_frame_id = Some("f0".into());
            let scopes = ["local", "closure"].map(|kind| Scope {
                kind: kind.into(),
                name: None,
            });
            let frame = StackFrame {
                function_name: "g".into(),
                location: PauseLocation {
                    url: None,
                    script_id: None,
                    line: 1,
                    column: 0,
                },
                scopes: scopes.to_vec(),
            };
            state.paused_stack = Some(CallStack {
                frames: vec![frame],
                ..Default::default()
            });
        }
        // (the version is read on connecting: set once connected)
        session.target(None).await.unwrap();
        let version = crate::handler::BrowserVersion {
            product: Some("Chrome/156.0.8078.47".into()),
            ..Default::default()
        };
        *session.browser_version.lock().await = Some(version);
        // When the closure's scope is asked for, then nothing
        for args in [
            json!({"expression": "v", "scope": 1}),
            json!({"expression": "v"}),
        ] {
            let _ = EvaluateOnCallFrameTool::handle(call("evaluate_on_call_frame", args), &handler)
                .await;
        }
        // Then its number is sent the first time only
        let sent = mock.commands_named("Debugger.evaluateOnCallFrame");
        assert_eq!(sent[0].params["scopeNumber"], 1);
        assert!(sent[1].params.get("scopeNumber").is_none());
    }

    /// DBG-05 (Chrome): an exception in the frame is a tool error worded as
    /// on Firefox; a value comes in the evaluate_js shape.
    #[tokio::test]
    async fn frame_exception_is_a_tool_error() {
        use crate::handler::cdp_domains::tests::{
            call, mock_handler, reply_text, spawn_recording_mock,
        };
        // Given a paused Chrome
        let mock = spawn_recording_mock().await;
        let handler = mock_handler(mock.http_port);
        let session = handler.session(None).await.unwrap();
        session
            .debugger_state(None)
            .unwrap()
            .lock()
            .await
            .paused_call_frame_id = Some("f1".into());
        // When the expression throws
        mock.respond_with("Debugger.evaluateOnCallFrame", |_| {
            json!({"result": {"result": {"type": "object", "subtype": "error", "className": "ReferenceError", "description": "ReferenceError: y is not defined\n    at eval"},
                "exceptionDetails": {"text": "Uncaught", "lineNumber": 0, "columnNumber": 0, "exception": {"type": "object", "subtype": "error", "className": "ReferenceError", "description": "ReferenceError: y is not defined\n    at eval"}}}})
        });
        let args = json!({"expression": "y"});
        let error = EvaluateOnCallFrameTool::handle(call("evaluate_on_call_frame", args), &handler)
            .await
            .unwrap_err();
        // Then the error says what was thrown
        assert_eq!(
            error.to_string(),
            "Evaluation threw: ReferenceError: y is not defined"
        );
        let sent = &mock.commands_named("Debugger.evaluateOnCallFrame")[0].params;
        assert_eq!(sent["callFrameId"], "f1");
        // And a number comes in the shared shape
        mock.respond_with(
            "Debugger.evaluateOnCallFrame",
            |_| json!({"result": {"result": {"type": "number", "value": 35, "description": "35"}}}),
        );
        let args = json!({"expression": "x * 7"});
        let reply = EvaluateOnCallFrameTool::handle(call("evaluate_on_call_frame", args), &handler)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_str(&reply_text(&reply)).unwrap();
        assert_eq!(body, json!({"value": 35, "type": "number"}));
    }
}
