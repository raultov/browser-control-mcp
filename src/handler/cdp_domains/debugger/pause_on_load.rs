//! `pause_on_load` on Chrome: a `debugger;` statement injected into every
//! new document (`Page.addScriptToEvaluateOnNewDocument`), then a reload.
//! The statement runs before the page's own scripts, so the page stops
//! before any of them; the tool waits for that pause and answers it.

use crate::handler::McpHandler;
use crate::handler::debugging::{self, PauseLocation};
use rust_mcp_sdk::{
    macros,
    schema::{CallToolError, CallToolRequestParams, CallToolResult},
};
use serde_json::json;

/// The URL pause_on_load's injected script names itself with (its
/// sourceURL), so its pause says whose it is (ISSUE-84).
pub(crate) const PAUSE_ON_LOAD_URL: &str = "browser-control-mcp://pause_on_load";

/// How long pause_on_load waits for the reloaded page to pause: the
/// statement runs when the new document starts, after its response.
#[cfg(not(test))]
const LOAD_PAUSE_WAIT: std::time::Duration = std::time::Duration::from_secs(5);
#[cfg(test)]
const LOAD_PAUSE_WAIT: std::time::Duration = std::time::Duration::from_millis(300);

#[macros::mcp_tool(
    name = "pause_on_load",
    description = "Reloads the tab and pauses it before any of its scripts runs, to debug startup: at a `debugger;` this tool injects (call_stack shows url browser-control-mcp://pause_on_load, reason pause_on_load). Then set_breakpoint (by url they bind as scripts load) and continue with resume. Answers where it paused, or that it did not within 5 s. Chrome only (not available on Firefox)."
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct PauseOnLoadTool {
    /// Default: 'default'.
    pub instance_id: Option<String>,
    /// Default: the active tab.
    pub tab_id: Option<String>,
}

impl PauseOnLoadTool {
    pub async fn handle(
        params: CallToolRequestParams,
        handler: &McpHandler,
    ) -> Result<CallToolResult, CallToolError> {
        use crate::handler::send_or;
        let args: PauseOnLoadTool = crate::handler::parse_args(params)?;
        let session = handler.session(args.instance_id.clone()).await?;
        let target = session.target(args.tab_id.clone()).await?;

        let debugger_state = session.debugger_state(args.tab_id.clone())?;
        install_script(&target, &debugger_state).await?;

        // Subscribed first: the pause can come before the reload's reply.
        let (_, recorded, mut events) =
            super::frames::paused_with_events(&target, &debugger_state).await;
        send_or(&target, "Page.reload", json!({}), "CDP Page.reload error").await?;
        let paused = match load_pause(&mut events).await {
            Some(at) => {
                // Answered once the tab's state holds it, for a call_stack
                // right after (ISSUE-94).
                super::recorded_or_warn(&debugger_state, recorded).await;
                Some(super::with_script_url(at, &*debugger_state.lock().await))
            }
            None => None,
        };
        Ok(reply(paused.as_ref()))
    }
}

/// Enables the debugger and injects pause_on_load's `debugger;` into the
/// tab's new documents, replacing an earlier one (it would pause every
/// later load), with the pauses kept even without breakpoints.
async fn install_script(
    target: &super::CdpTarget,
    state: &std::sync::Arc<tokio::sync::Mutex<crate::handler::DebuggerState>>,
) -> Result<(), CallToolError> {
    use crate::handler::send_or;
    let failure = "CDP Debugger.enable error";
    send_or(target, "Debugger.enable", json!({}), failure).await?;
    let source = format!("debugger;\n//# sourceURL={PAUSE_ON_LOAD_URL}");
    let script = json!({ "source": source });
    let method = "Page.addScriptToEvaluateOnNewDocument";
    let added = send_or(target, method, script, "CDP Page.addScript... error").await?;
    let identifier = crate::handler::extract_from_value(&added.result, "identifier");
    let previous = std::mem::replace(
        &mut state.lock().await.pause_on_load_script,
        identifier.map(str::to_string),
    );
    if let Some(identifier) = previous {
        let params = json!({ "identifier": identifier });
        let _ = target
            .send_raw_command("Page.removeScriptToEvaluateOnNewDocument", params)
            .await;
    }
    super::sync_pauses(target, state).await;
    Ok(())
}

/// The reloaded page's first pause within [`LOAD_PAUSE_WAIT`], named as
/// pause_on_load's when it is in the injected script: its
/// `Debugger.scriptParsed` (with [`PAUSE_ON_LOAD_URL`]) comes on `events`
/// first, so this does not depend on the listener having recorded it.
async fn load_pause(
    events: &mut (
             impl tokio_stream::Stream<Item = cdp_browser_lite::CdpResult<cdp_browser_lite::WsResponse>>
             + Unpin
         ),
) -> Option<PauseLocation> {
    use tokio_stream::StreamExt;
    let waiting = async {
        let mut injected = Vec::new();
        while let Some(event) = events.next().await {
            let Ok(event) = event else { continue };
            let params = event.params.as_ref();
            match event.method.as_deref() {
                Some("Debugger.scriptParsed")
                    if params.and_then(|p| p["url"].as_str()) == Some(PAUSE_ON_LOAD_URL) =>
                {
                    let id = params.and_then(|p| p["scriptId"].as_str());
                    injected.extend(id.map(str::to_string));
                }
                Some("Debugger.paused") => {
                    let mut at = params.and_then(super::pause_location)?;
                    if at
                        .script_id
                        .as_ref()
                        .is_some_and(|id| injected.contains(id))
                    {
                        at.url = Some(PAUSE_ON_LOAD_URL.to_string());
                    }
                    return Some(at);
                }
                _ => {}
            }
        }
        None
    };
    tokio::time::timeout(LOAD_PAUSE_WAIT, waiting)
        .await
        .ok()
        .flatten()
}

/// pause_on_load's reply: where the page paused (before its scripts, at
/// the injected statement), or that it has not paused yet.
fn reply(paused: Option<&PauseLocation>) -> CallToolResult {
    let first = match paused {
        Some(at) if at.url.as_deref() == Some(PAUSE_ON_LOAD_URL) => "Page reloaded and paused before its first script runs, at pause_on_load's own `debugger;` statement: set breakpoints in the page's scripts now (set_breakpoint by url binds once they load), then resume.".to_string(),
        Some(at) => format!("Page reloaded and paused{}: inspect it with call_stack, then resume.", at.at()),
        None => "Page reloaded, but it has not paused yet (within 5 s): the pause comes when the new document starts running; call_stack shows it, and resume continues it.".to_string(),
    };
    let body = serde_json::to_string_pretty(&debugging::pause_json(paused)).unwrap_or_default();
    CallToolResult::text_content(vec![format!("{first}\n{body}").into()])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::McpHandler;
    use crate::handler::cdp_domains::tests::spawn_mock_chrome_server;
    use crate::handler::chrome_instance::MockChromeManager;
    use rust_mcp_sdk::schema::CallToolRequestParams;
    use serde_json::json;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    #[tokio::test]
    async fn test_pause_on_load_params_deserialization() {
        let params: Result<CallToolRequestParams, _> = serde_json::from_value(json!({
            "name": "pause_on_load",
            "arguments": {}
        }));
        assert!(params.is_ok());
    }

    #[tokio::test]
    async fn test_pause_on_load_tool_deserialization() {
        let tool: Result<PauseOnLoadTool, _> = serde_json::from_value(json!({}));
        assert!(tool.is_ok());
    }

    #[tokio::test]
    async fn test_pause_on_load_handle() {
        let port = spawn_mock_chrome_server().await;

        // Create a handler with a MockChromeManager that returns our mock server's port
        let mut handler = McpHandler::new_test();
        Arc::get_mut(&mut handler.default_session)
            .unwrap()
            .chrome_manager = Arc::new(Mutex::new(MockChromeManager::new(port)));

        let params: CallToolRequestParams = serde_json::from_value(json!({
            "name": "pause_on_load",
            "arguments": {}
        }))
        .unwrap();

        let result = PauseOnLoadTool::handle(params, &handler).await;
        assert!(result.is_ok(), "Handle should succeed: {:?}", result.err());

        // ISSUE-84: a page that does not pause in time is not reported as
        // paused (the reply used to say "paused" at once).
        let text = crate::handler::cdp_domains::tests::reply_text(&result.unwrap());
        assert!(
            text.starts_with("Page reloaded, but it has not paused yet"),
            "{text}"
        );
        assert!(text.contains("\"paused\": false"), "{text}");
    }

    /// ISSUE-84 (Chrome): pause_on_load waits for the pause and answers
    /// where it is: before the page's first script, in its own script,
    /// named by its sourceURL.
    #[tokio::test]
    async fn pause_on_load_answers_where_it_paused() {
        use crate::handler::cdp_domains::tests::{
            call, fire_events_after, mock_handler, reply_text, spawn_recording_mock,
        };
        // Given a page that pauses in the injected script once reloaded
        let mock = spawn_recording_mock().await;
        mock.respond_with(
            "Page.addScriptToEvaluateOnNewDocument",
            |_| json!({"identifier": "1"}),
        );
        let parsed = json!({"scriptId": "4", "hash": "h", "url": PAUSE_ON_LOAD_URL});
        let frame = json!({"callFrameId": "f0", "functionName": "", "url": "", "location": {"scriptId": "4", "lineNumber": 0, "columnNumber": 0}});
        let paused = json!({"reason": "other", "callFrames": [frame]});
        fire_events_after(
            mock,
            "Page.reload",
            vec![
                ("Debugger.scriptParsed", parsed),
                ("Debugger.paused", paused),
            ],
        );
        let handler = mock_handler(mock.http_port);
        // When pause_on_load runs
        let reply = PauseOnLoadTool::handle(call("pause_on_load", json!({})), &handler)
            .await
            .unwrap();
        // Then it answers the pause, before the page's scripts
        let text = reply_text(&reply);
        assert!(
            text.starts_with("Page reloaded and paused before"),
            "{text}"
        );
        let body: serde_json::Value =
            serde_json::from_str(&text[text.find('{').unwrap()..]).unwrap();
        assert_eq!(body["paused"], true);
        assert_eq!(body["location"]["url"], PAUSE_ON_LOAD_URL);
        // And the injected script names itself
        let added = mock.commands_named("Page.addScriptToEvaluateOnNewDocument");
        let source = added[0].params["source"].as_str().unwrap();
        assert!(
            source.contains(&format!("//# sourceURL={PAUSE_ON_LOAD_URL}")),
            "{source}"
        );
    }

    /// AUD-DB-05: calling pause_on_load again replaces its script (the
    /// first one used to stay injected, so every later load paused).
    #[tokio::test]
    async fn second_pause_on_load_replaces_the_first_script() {
        use crate::handler::cdp_domains::tests::{call, mock_handler, spawn_recording_mock};
        let mock = spawn_recording_mock().await;
        let next = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        mock.respond_with("Page.addScriptToEvaluateOnNewDocument", move |_| {
            let n = next.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            json!({"identifier": n.to_string()})
        });
        let handler = mock_handler(mock.http_port);
        for _ in 0..2 {
            PauseOnLoadTool::handle(call("pause_on_load", json!({})), &handler)
                .await
                .unwrap();
        }
        let removed = mock.commands_named("Page.removeScriptToEvaluateOnNewDocument");
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].params["identifier"], "1");
        let session = handler.session(None).await.unwrap();
        let script = session
            .debugger_state
            .lock()
            .await
            .pause_on_load_script
            .clone();
        assert_eq!(script.as_deref(), Some("2"));
    }
}
