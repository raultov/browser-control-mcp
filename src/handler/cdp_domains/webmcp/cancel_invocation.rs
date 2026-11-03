use crate::handler::McpHandler;
use rust_mcp_sdk::{
    macros,
    schema::{CallToolError, CallToolRequestParams, CallToolResult},
};

#[macros::mcp_tool(
    name = "webmcp_cancel_invocation",
    description = "Cancels a pending WebMCP invocation (e.g. one waiting for the user's consent on the page). Returns its status. Chrome only (not available on Firefox)."
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct CancelWebmcpInvocationTool {
    /// Instance id; omit for the default instance.
    pub instance_id: Option<String>,
    /// Tab id; omit for the active tab.
    pub tab_id: Option<String>,
    /// Id from webmcp_invoke_tool or webmcp_list_invocations.
    #[serde(rename = "invocationId")]
    pub invocation_id: String,
}

impl CancelWebmcpInvocationTool {
    pub async fn handle(
        params: CallToolRequestParams,
        handler: &McpHandler,
    ) -> Result<CallToolResult, CallToolError> {
        let tool: CancelWebmcpInvocationTool = crate::handler::parse_args(params)?;
        let session = handler.session(tool.instance_id.clone()).await?;
        let webmcp_state = session.webmcp_state(tool.tab_id.clone())?;
        let known = webmcp_state
            .lock()
            .await
            .invocations
            .contains_key(&tool.invocation_id);
        if !known {
            return Err(CallToolError::from_message(format!(
                "No WebMCP invocation found with id '{}': use webmcp_list_invocations to see the known invocations.",
                tool.invocation_id
            )));
        }
        let target = session.target(tool.tab_id.clone()).await?;
        let cancel = serde_json::json!({"invocationId": tool.invocation_id});
        // As text: the error type is not `Send` across the next await.
        let sent = target
            .send_raw_command("WebMCP.cancelInvocation", cancel)
            .await
            .map_err(|e| e.to_string());
        let mut st = webmcp_state.lock().await;
        let Some(invocation) = st.invocations.get_mut(&tool.invocation_id) else {
            return Err(CallToolError::from_message(
                "The invocation was forgotten meanwhile.",
            ));
        };
        let note = canceled(invocation, sent);
        let status = invocation
            .status
            .clone()
            .unwrap_or_else(|| "Pending".into());
        let reply = serde_json::json!({
            "invocationId": invocation.invocation_id,
            "toolName": invocation.tool_name,
            "status": status,
        });
        let body = serde_json::to_string_pretty(&reply).unwrap_or_default();
        Ok(CallToolResult::text_content(vec![
            format!("{note}\n{body}").into(),
        ]))
    }
}

/// What a cancel did to `invocation`, given Chrome's answer `sent`: a
/// pending one Chrome no longer knows (its page is gone) can never
/// complete, so it is marked canceled here.
fn canceled(
    invocation: &mut super::WebmcpInvocation,
    sent: Result<cdp_browser_lite::WsResponse, String>,
) -> String {
    let pending = invocation.status.is_none();
    match (sent, pending) {
        // Chrome answers with a toolResponded (Canceled) shortly.
        (Ok(_), true) => "Cancel sent to the page.".to_string(),
        (_, false) => "It had already finished: nothing to cancel.".to_string(),
        // Chrome no longer knows it (its page is gone): it can never
        // complete, so it is canceled here.
        (Err(error), true) => {
            invocation.status = Some("Canceled".into());
            invocation.note = None;
            format!("Chrome no longer knows it ({error}): marked canceled.")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::cdp_domains::tests::{
        call, mock_handler, reply_text, spawn_recording_mock,
    };
    use crate::handler::cdp_domains::webmcp::WebmcpInvocation;
    use serde_json::json;

    /// A mock tab with the pending invocation `inv-1`, answering
    /// `WebMCP.cancelInvocation` with `reply`.
    async fn pending_on(
        reply: serde_json::Value,
    ) -> (
        &'static cdp_browser_lite::test_support::mock_devtools::MockDevTools,
        McpHandler,
    ) {
        let mock = spawn_recording_mock().await;
        mock.respond_with("WebMCP.cancelInvocation", move |_| reply.clone());
        let handler = mock_handler(mock.http_port);
        let invocation = WebmcpInvocation {
            tool_name: "copy".into(),
            frame_id: "F1".into(),
            invocation_id: "inv-1".into(),
            ..Default::default()
        };
        let session = handler.session(None).await.unwrap();
        session
            .webmcp_state
            .lock()
            .await
            .invocations
            .insert("inv-1".into(), invocation);
        (mock, handler)
    }

    /// ISSUE-73: a pending invocation is canceled with
    /// `WebMCP.cancelInvocation`, and the reply gives its status.
    #[tokio::test]
    async fn a_pending_invocation_is_canceled() {
        // Given a pending invocation Chrome cancels
        let (mock, handler) = pending_on(json!({})).await;
        // When the agent cancels it
        let args = json!({"invocationId": "inv-1"});
        let reply =
            CancelWebmcpInvocationTool::handle(call("webmcp_cancel_invocation", args), &handler)
                .await
                .unwrap();
        // Then Chrome was asked, and the reply names it
        let sent = mock.commands_named("WebMCP.cancelInvocation");
        assert_eq!(sent[0].params, json!({"invocationId": "inv-1"}));
        assert!(reply_text(&reply).contains("inv-1"));
    }

    /// ISSUE-73: an invocation Chrome no longer knows (its page is gone) is
    /// marked canceled here, so it stops showing as pending.
    #[tokio::test]
    async fn an_invocation_chrome_forgot_is_marked_canceled() {
        // Given a pending invocation Chrome no longer knows
        let error = json!({"error": {"code": -32602, "message": "Invocation not found"}});
        let (_mock, handler) = pending_on(error).await;
        // When the agent cancels it
        let args = json!({"invocationId": "inv-1"});
        let reply =
            CancelWebmcpInvocationTool::handle(call("webmcp_cancel_invocation", args), &handler)
                .await
                .unwrap();
        // Then it is canceled here, saying why
        assert!(
            reply_text(&reply).contains("\"status\": \"Canceled\""),
            "{}",
            reply_text(&reply)
        );
        let session = handler.session(None).await.unwrap();
        let state = session.webmcp_state.lock().await;
        assert_eq!(
            state.invocations.get("inv-1").unwrap().status.as_deref(),
            Some("Canceled")
        );
    }

    /// ISSUE-73: an unknown id fails at once, nothing sent.
    #[tokio::test]
    async fn an_unknown_invocation_is_refused() {
        let (mock, handler) = pending_on(json!({})).await;
        let args = json!({"invocationId": "nope"});
        let error =
            CancelWebmcpInvocationTool::handle(call("webmcp_cancel_invocation", args), &handler)
                .await
                .unwrap_err()
                .to_string();
        assert!(error.contains("webmcp_list_invocations"), "{error}");
        assert!(mock.commands_named("WebMCP.cancelInvocation").is_empty());
    }
}
