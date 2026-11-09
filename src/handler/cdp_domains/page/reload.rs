use crate::handler::McpHandler;
use rust_mcp_sdk::{
    macros,
    schema::{CallToolError, CallToolRequestParams, CallToolResult},
};
use serde_json::json;

#[macros::mcp_tool(
    name = "reload",
    description = "Reloads the tab and waits for load (at most 30 s; a page that pauses at a breakpoint or opens a dialog while loading fails at once saying so). Returns the same shape as 'navigate'."
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct ReloadTool {
    /// Default: 'default'.
    pub instance_id: Option<String>,
    /// Default: the active tab.
    pub tab_id: Option<String>,
}

impl ReloadTool {
    pub async fn handle(
        params: CallToolRequestParams,
        handler: &McpHandler,
    ) -> Result<CallToolResult, CallToolError> {
        let ids: crate::handler::TargetArgs = crate::handler::parse_args(params)?;
        let session = handler.session(ids.instance_id.clone()).await?;
        let target = session.target_to_load(ids.tab_id.clone()).await?;
        // Subscribed first: `load` can fire before the reply is read.
        let mut page_events = target.on_domain("Page");
        let reload = async {
            // Right after a navigation Chrome may still be committing the
            // new document and refuse with "Not attached to an active
            // page": retry once after a short pause.
            let mut reply = target.send_raw_command("Page.reload", json!({})).await;
            if reply.as_ref().is_err_and(|e| is_transient(&e.to_string())) {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                reply = target.send_raw_command("Page.reload", json!({})).await;
            }
            let reply =
                reply.map_err(|e| crate::handler::cdp_failure("Failed to reload page", &e))?;
            let loaded = super::navigate::wait_for_load(&mut page_events).await;
            Ok::<_, String>((reply, loaded))
        };
        // A page paused in the debugger (or showing a dialog) while it
        // loads never fires `load`: the call ends saying so, as on Firefox.
        // Listened for before the command is sent: the pause can come
        // right after its reply.
        let (resp, loaded) = target
            .race_interruptions("reload", reload)
            .await
            .map_err(|stopped| stopped.during("reload"))
            .and_then(|reloaded| reloaded)
            .map_err(CallToolError::from_message)?;
        let browser = resp.result.unwrap_or_else(|| json!({}));
        let network = session.network_state(ids.tab_id.clone())?;
        let outcome = super::navigate::finish(&target, "", browser, loaded).await;
        let outcome = super::navigate::with_status(outcome, &target, &network).await;
        // The page's server may now redirect elsewhere.
        super::navigate::stay_local(&target, handler.flags.local, &outcome.url).await?;
        let tab_id = session.tab_id_of(ids.tab_id.as_deref());
        Ok(outcome.reply("Page reloaded.", tab_id.as_deref()))
    }
}

/// Whether a `Page.reload` error is the transient one Chrome returns while
/// it is still committing a navigation, worth one retry.
fn is_transient(error: &str) -> bool {
    error.contains("Not attached to an active page")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::cdp_domains::tests::{
        call, committed_url, fire_load_after, mock_handler, reply_text, spawn_recording_mock,
    };
    use rust_mcp_sdk::schema::CallToolRequestParams;
    use serde_json::json;

    #[tokio::test]
    async fn test_reload_params_deserialization() {
        let params: Result<CallToolRequestParams, _> = serde_json::from_value(json!({
            "name": "reload",
            "arguments": {}
        }));
        assert!(params.is_ok());
    }

    #[tokio::test]
    async fn test_reload_tool_deserialization() {
        let tool: Result<ReloadTool, _> = serde_json::from_value(json!({}));
        assert!(tool.is_ok());
    }

    /// NAV-05: reload answers the same shape as navigate, after `load`.
    #[tokio::test]
    async fn reload_reports_url_after_load() {
        // Given a Chrome showing /page
        let mock = spawn_recording_mock().await;
        committed_url(mock, "http://127.0.0.1:8/page");
        fire_load_after(mock, "Page.reload");
        let handler = mock_handler(mock.http_port);
        // When reload runs
        let reply = ReloadTool::handle(call("reload", json!({})), &handler)
            .await
            .unwrap();
        // Then it waited for load and reports the URL in the shared shape
        let text = reply_text(&reply);
        assert!(text.starts_with("Page reloaded."), "{text}");
        let body: serde_json::Value =
            serde_json::from_str(&text[text.find('{').unwrap()..]).unwrap();
        assert_eq!(body["url"], "http://127.0.0.1:8/page");
        assert!(body.get("tab_id").is_some() && body.get("browser").is_some());
        assert!(body.get("notes").is_none(), "load fired: {body}");
    }

    /// NAV-07 (#105): with --local, a reload that ends (after a redirect the
    /// server now answers) on an address that is not local fails naming
    /// --local, and the tab is sent back to about:blank.
    #[tokio::test]
    async fn local_mode_refuses_a_reload_that_ends_remote() {
        // Given a --local Chrome whose reload lands on a remote page
        let mock = spawn_recording_mock().await;
        committed_url(mock, "http://127.0.0.2:8/m.html");
        fire_load_after(mock, "Page.reload");
        let mut handler = mock_handler(mock.http_port);
        handler.flags.local = true;
        // When reload runs
        let error = ReloadTool::handle(call("reload", json!({})), &handler)
            .await
            .unwrap_err()
            .to_string();
        // Then it fails naming --local, and the tab is sent to about:blank
        assert!(
            error.contains("--local") && error.ends_with("(http://127.0.0.2:8)"),
            "{error}"
        );
        let sent = mock.commands_named("Page.navigate");
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].params["url"], "about:blank");
    }

    /// A pause announced right after Chrome answers `Page.reload`, before
    /// reload listens for interruptions (the order a slow machine showed),
    /// also ends the wait: reload listens before it sends the command, as
    /// navigate does. The tab's debugger state is held meanwhile, as the
    /// listening starts by reading it.
    #[tokio::test]
    async fn a_pause_right_after_the_reload_reply_ends_the_wait() {
        let mock = spawn_recording_mock().await;
        let handler = mock_handler(mock.http_port);
        let state = handler
            .session(None)
            .await
            .unwrap()
            .debugger_state(None)
            .unwrap();
        let location = json!({"scriptId": "7", "lineNumber": 2, "columnNumber": 4});
        let frame =
            json!({"callFrameId": "c1", "url": "http://127.0.0.1:8/app.js", "location": location});
        let paused = json!({"reason": "other", "callFrames": [frame]});
        // (answered, then the state held while the pause is announced)
        mock.respond_with("Page.reload", move |_| {
            let held = state.clone().try_lock_owned();
            mock.emit("Debugger.paused", paused.clone(), None);
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                drop(held);
            });
            json!({})
        });
        let started = std::time::Instant::now();
        let error = ReloadTool::handle(call("reload", json!({})), &handler)
            .await
            .unwrap_err()
            .to_string();
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        assert!(error.contains("while running reload"), "{error}");
    }

    /// ISSUE-45: a page that pauses in the debugger while it reloads makes
    /// reload fail at once naming where.
    #[tokio::test]
    async fn a_pause_while_reloading_ends_the_wait() {
        let mock = spawn_recording_mock().await;
        crate::handler::cdp_domains::page::navigate::tests::pause_after(mock, "Page.reload");
        let handler = mock_handler(mock.http_port);
        let started = std::time::Instant::now();
        let error = ReloadTool::handle(call("reload", json!({})), &handler)
            .await
            .unwrap_err()
            .to_string();
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        assert!(error.contains("while running reload"), "{error}");
    }

    /// PAG-RL1: reload right after navigate retries the transient
    /// "Not attached to an active page" error, and only that one.
    #[test]
    fn reload_retries_only_the_not_attached_error() {
        // Given the error Chrome returns mid-navigation and an unrelated one
        let transient = r#"Protocol { code: -32000, message: "Not attached to an active page" }"#;
        let other = r#"Protocol { code: -32601, message: "'Page.reload' wasn't found" }"#;
        // When / Then only the first is retried
        assert!(is_transient(transient));
        assert!(!is_transient(other));
    }
}
