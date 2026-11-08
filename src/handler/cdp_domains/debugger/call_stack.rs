//! `call_stack` and `pause_on_exceptions` on Chrome (ISSUE-67): the stack
//! of the kept pause, read from its `Debugger.paused`, and when the tab
//! pauses on exceptions (`Debugger.setPauseOnExceptions`, sent with the
//! other pause settings to the tab and its frames of another origin).

use crate::handler::McpHandler;
use crate::handler::debugging::{self, PauseOnExceptions};
use rust_mcp_sdk::{
    macros,
    schema::{CallToolError, CallToolRequestParams, CallToolResult},
};

#[macros::mcp_tool(
    name = "call_stack",
    description = "Lists the paused stack, innermost first: each frame's index (evaluate_on_call_frame's frame_index), function_name, url, 0-based line_number and column_number; why it paused (breakpoint, step, exception...) and what was thrown; on Chrome each frame's scopes (index for evaluate_on_call_frame's scope; type: local, closure, block, script, global...; name) and its async callers (setTimeout, promises). At most 50 frames.",
    read_only_hint = true
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct CallStackTool {
    /// Default: 'default'.
    pub instance_id: Option<String>,
    /// Default: the active tab.
    pub tab_id: Option<String>,
}

impl CallStackTool {
    pub async fn handle(
        params: CallToolRequestParams,
        handler: &McpHandler,
    ) -> Result<CallToolResult, CallToolError> {
        let ids: crate::handler::TargetArgs = crate::handler::parse_args(params)?;
        let session = handler.session(ids.instance_id.clone()).await?;
        let state = session.debugger_state(ids.tab_id.clone())?;
        let state = state.lock().await;
        let Some(mut stack) = state.paused_stack.clone() else {
            return Err(CallToolError::from_message(debugging::NOT_PAUSED));
        };
        // Chrome leaves a frame's URL empty: it comes from its parsed script.
        let frames = stack.frames.iter_mut().chain(
            stack
                .async_parents
                .iter_mut()
                .flat_map(|segment| segment.frames.iter_mut()),
        );
        for frame in frames {
            frame.location = super::with_script_url(frame.location.clone(), &state);
        }
        Ok(stack.reply())
    }
}

#[macros::mcp_tool(
    name = "pause_on_exceptions",
    description = "Sets when the tab's debugger pauses on exceptions: uncaught (and unhandled promise rejections), all (caught ones too) or none. Inspect the pause with call_stack (it names what was thrown) and evaluate_on_call_frame; continue with resume or a step. Per tab, kept across navigations.",
    destructive_hint = false
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct PauseOnExceptionsTool {
    /// Default: 'default'.
    pub instance_id: Option<String>,
    /// Default: the active tab.
    pub tab_id: Option<String>,
    /// 'none', 'uncaught' or 'all'.
    pub state: PauseOnExceptions,
}

impl PauseOnExceptionsTool {
    pub async fn handle(
        params: CallToolRequestParams,
        handler: &McpHandler,
    ) -> Result<CallToolResult, CallToolError> {
        let args: Self = crate::handler::parse_args(params)?;
        let session = handler.session(args.instance_id.clone()).await?;
        let target = session.target(args.tab_id.clone()).await?;
        let state = session.debugger_state(args.tab_id.clone())?;
        state.lock().await.pause_on_exceptions = args.state;
        super::sync_pauses(&target, &state).await;
        Ok(args.state.reply())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::cdp_domains::tests::{
        call, mock_handler, reply_text, spawn_recording_mock,
    };
    use crate::handler::debugging::{CallStack, PauseLocation, StackFrame};
    use serde_json::json;

    /// ISSUE-67 (Chrome): call_stack lists the kept pause's frames with
    /// their script's URL; without a pause it fails as the other debugger
    /// tools do.
    #[tokio::test]
    async fn call_stack_lists_the_kept_pause() {
        let mock = spawn_recording_mock().await;
        let handler = mock_handler(mock.http_port);
        let read = || CallStackTool::handle(call("call_stack", json!({})), &handler);
        let error = read().await.unwrap_err().to_string();
        assert_eq!(error, debugging::NOT_PAUSED);
        let session = handler.session(None).await.unwrap();
        let state = session.debugger_state(None).unwrap();
        {
            let mut state = state.lock().await;
            let script = crate::handler::ScriptInfo {
                hash: "h".into(),
                start_line: 0,
                start_column: 0,
                end_line: 9,
                url: "http://a/cart.js".into(),
            };
            state.scripts.insert("3".into(), script);
            let frame = StackFrame {
                function_name: "total".into(),
                scopes: Vec::new(),
                location: PauseLocation {
                    url: None,
                    script_id: Some("3".into()),
                    line: 5,
                    column: 1,
                },
            };
            state.paused_stack = Some(CallStack {
                reason: "breakpoint".into(),
                frames: vec![frame],
                ..Default::default()
            });
        }
        let text = reply_text(&read().await.unwrap());
        assert!(
            text.starts_with("Paused (breakpoint): 1 frame(s)"),
            "{text}"
        );
        assert!(text.contains("\"url\": \"http://a/cart.js\""), "{text}");
    }

    /// ISSUE-67 (Chrome): pause_on_exceptions tells Chrome the mode and
    /// stops skipping the tab's pauses.
    #[tokio::test]
    async fn pause_on_exceptions_tells_chrome() {
        let mock = spawn_recording_mock().await;
        let handler = mock_handler(mock.http_port);
        let args = json!({"state": "uncaught"});
        let reply = PauseOnExceptionsTool::handle(call("pause_on_exceptions", args), &handler)
            .await
            .unwrap();
        assert!(reply_text(&reply).contains("uncaught exceptions"));
        let set = mock.commands_named("Debugger.setPauseOnExceptions");
        assert_eq!(set.last().unwrap().params, json!({"state": "uncaught"}));
        let skip = mock.commands_named("Debugger.setSkipAllPauses");
        assert_eq!(skip.last().unwrap().params, json!({"skip": false}));
        let session = handler.session(None).await.unwrap();
        let state = session.debugger_state(None).unwrap();
        assert_eq!(
            state.lock().await.pause_on_exceptions,
            PauseOnExceptions::Uncaught
        );
    }
}
