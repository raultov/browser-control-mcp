use crate::handler::McpHandler;
use crate::handler::dialogs::{self, NO_DIALOG};
use rust_mcp_sdk::{
    macros,
    schema::{CallToolError, CallToolRequestParams, CallToolResult},
};
use serde_json::json;

#[macros::mcp_tool(
    name = "handle_dialog",
    description = "Closes the JavaScript dialog (alert, confirm, prompt or beforeunload) open on a tab, accepting or dismissing it. An open dialog blocks its page: while one is open, the tools that run page code fail naming it (its type and message) and telling to call this tool. Side effects: the page continues; accepting a beforeunload dialog lets the page navigate away, a confirm returns true (accept) or false, a prompt returns 'prompt_text' (accept) or null. Returns: a first line ('Dialog accepted.' or 'Dialog dismissed.') and JSON {accepted, dialog: {type, message, default_prompt?} | null}; fails when no dialog is open. Alternatives: 'evaluate_js' to change the page so it does not open dialogs (e.g. replace window.alert)."
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct HandleDialogTool {
    /// Instance id from open_instance/list_instances. Omit for the default instance.
    pub instance_id: Option<String>,
    /// The Tab ID of the target tab. Omit to use the active tab.
    pub tab_id: Option<String>,
    /// true to accept the dialog (OK, Leave), false to dismiss it (Cancel, Stay). Required.
    pub accept: bool,
    /// The answer to a prompt dialog when accepting it. Ignored by other dialogs. Defaults to: the prompt's default answer.
    pub prompt_text: Option<String>,
}

impl HandleDialogTool {
    pub async fn handle(
        params: CallToolRequestParams,
        handler: &McpHandler,
    ) -> Result<CallToolResult, CallToolError> {
        let args: HandleDialogTool = crate::handler::parse_args(params)?;
        let session = handler.session(args.instance_id.clone()).await?;
        let target = session.target_ignoring_dialogs(args.tab_id.clone()).await?;
        let state = session.dialog_state(args.tab_id.clone())?;
        let mut params = json!({"accept": args.accept});
        if let Some(text) = &args.prompt_text {
            params["promptText"] = json!(text);
        }
        // Read first: closing it makes Chrome announce it closed.
        let dialog = state.lock().await.open.clone();
        let reply = target
            .send_raw_command("Page.handleJavaScriptDialog", params)
            .await;
        // Closed or not, the tracked dialog is stale once Chrome answered.
        state.lock().await.open = None;
        match reply {
            Ok(_) => Ok(dialogs::handled(args.accept, dialog.as_ref())),
            Err(cdp_browser_lite::CdpError::ProtocolError { message, .. })
                if message.contains("No dialog is showing") =>
            {
                Err(CallToolError::from_message(NO_DIALOG))
            }
            Err(e) => Err(CallToolError::from_message(crate::handler::cdp_failure(
                "Failed to close the dialog",
                &e,
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::cdp_domains::runtime::evaluate_js::EvaluateJsTool;
    use crate::handler::cdp_domains::tests::{
        call, mock_handler, reply_text, spawn_recording_mock,
    };

    /// AUD-EV-05: while a dialog is open, page tools fail at once naming it
    /// (nothing is sent); handle_dialog closes it and they work again.
    #[tokio::test]
    async fn open_dialog_blocks_tools_until_handled() {
        // Given a page that opened a prompt
        let mock = spawn_recording_mock().await;
        let handler = mock_handler(mock.http_port);
        let session = handler.session(None).await.unwrap();
        session.target(None).await.unwrap();
        let prompt = json!({"type": "prompt", "message": "Name?", "defaultPrompt": "Ann"});
        mock.emit("Page.javascriptDialogOpening", prompt, None);
        for _ in 0..100 {
            if session.dialog_state.lock().await.open.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        // When evaluate_js runs
        let evaluate = || call("evaluate_js", json!({"expression": "1"}));
        let before = mock.commands_named("Runtime.evaluate").len();
        let error = EvaluateJsTool::handle(evaluate(), &handler)
            .await
            .unwrap_err()
            .to_string();
        // Then it names the dialog and sent nothing
        assert!(
            error.starts_with("A JavaScript prompt dialog is open on this tab (\"Name?\")"),
            "{error}"
        );
        assert_eq!(mock.commands_named("Runtime.evaluate").len(), before);
        // And handle_dialog accepts it with the answer
        let args = json!({"accept": true, "prompt_text": "Bob"});
        let reply = HandleDialogTool::handle(call("handle_dialog", args), &handler)
            .await
            .unwrap();
        let text = reply_text(&reply);
        assert!(text.starts_with("Dialog accepted."), "{text}");
        assert!(text.contains("\"default_prompt\": \"Ann\""), "{text}");
        let sent = &mock.commands_named("Page.handleJavaScriptDialog")[0].params;
        assert_eq!(sent, &json!({"accept": true, "promptText": "Bob"}));
        // And the page tools work again
        EvaluateJsTool::handle(evaluate(), &handler).await.unwrap();
    }

    /// AUD-EV-05: with no dialog open, Chrome's refusal is a clear error.
    #[tokio::test]
    async fn no_open_dialog_is_an_error() {
        let mock = spawn_recording_mock().await;
        mock.respond_with(
            "Page.handleJavaScriptDialog",
            |_| json!({"error": {"code": -32000, "message": "No dialog is showing"}}),
        );
        let handler = mock_handler(mock.http_port);
        let args = json!({"accept": false});
        let error = HandleDialogTool::handle(call("handle_dialog", args), &handler)
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), NO_DIALOG);
    }
}
