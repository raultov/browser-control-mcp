//! The JavaScript debugger of Firefox tabs, over RDP (`bidi_lite::rdp`).
//!
//! One `Debugger` per tab, attached on the first debugger tool (the tab is
//! activated through BiDi, then `attach_selected`, spike H15). A background
//! task follows its thread state, so the tools know whether and where the
//! page is paused (each pause is announced twice: tracked once).

use super::session::FirefoxSession;
use crate::backend::error::BackendError;
use crate::handler::cdp_domains::debugger::evaluate_on_call_frame::EvaluateOnCallFrameTool;
use crate::handler::cdp_domains::debugger::remove_breakpoint::RemoveBreakpointTool;
use crate::handler::cdp_domains::debugger::search_scripts::{
    ScriptText, SearchReport, SearchScriptsTool,
};
use crate::handler::cdp_domains::debugger::set_breakpoint::SetBreakpointTool;
use crate::handler::cdp_domains::event_pump::ListenerHandles;
use crate::handler::{McpHandler, TargetArgs, parse_args};
use bidi_browser_lite::Debugger;
use bidi_browser_lite::bidi_lite::rdp::debugger::{
    BreakpointLocation, ThreadState, parse_thread_states,
};
use rust_mcp_sdk::schema::{CallToolError, CallToolRequestParams, CallToolResult};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tokio_stream::StreamExt;

type ToolResult = Result<CallToolResult, CallToolError>;

/// Where the thread is stopped.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PauseInfo {
    /// Frame actor of the top frame.
    pub(crate) frame: Option<String>,
    /// Why (`breakpoint`, `resumeLimit`, `debuggerStatement`...).
    pub(crate) why: String,
    /// 1-based line and 0-based column of the top frame.
    pub(crate) location: Option<(u32, u32)>,
}

/// A debugger attached to one tab.
pub(crate) struct TabDebugger {
    pub(crate) debugger: Debugger,
    /// MCP breakpoint ids → locations.
    pub(crate) breakpoints: BTreeMap<String, BreakpointLocation>,
    counter: usize,
    /// Current pause, kept by the state task.
    pub(crate) pause: Arc<StdMutex<Option<PauseInfo>>>,
    _task: ListenerHandles,
}

/// Applies thread states to the tracked pause (latest wins: after a step
/// `resumed` and the new `paused` arrive in one packet).
pub(crate) fn apply_states(pause: &StdMutex<Option<PauseInfo>>, states: Vec<ThreadState>) {
    let mut slot = pause
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for state in states {
        match state {
            ThreadState::Paused { why, frame } => {
                *slot = Some(PauseInfo {
                    frame: frame.as_ref().map(|f| f.actor.clone()),
                    why: why
                        .get("type")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    location: frame.and_then(|f| f.location).map(|l| (l.line, l.column)),
                });
            }
            ThreadState::Resumed => *slot = None,
        }
    }
}

fn spawn_state_task(
    debugger: &Debugger,
    pause: Arc<StdMutex<Option<PauseInfo>>>,
) -> ListenerHandles {
    let mut states = debugger.on_thread_state();
    let mut handles = ListenerHandles::default();
    handles.push(tokio::spawn(async move {
        while let Some(packet) = states.next().await {
            if let Ok(packet) = packet {
                apply_states(&pause, parse_thread_states(&packet));
            }
        }
    }));
    handles
}

/// Runs `f` with the tab's debugger, attaching it first if needed.
async fn with_debugger<T>(
    session: &FirefoxSession,
    tab_id: Option<&str>,
    f: impl AsyncFnOnce(&mut TabDebugger) -> Result<T, BackendError>,
) -> Result<T, BackendError> {
    let (id, tab) = session.tab(tab_id).await?;
    let mut debuggers = session.debuggers.lock().await;
    if !debuggers.contains_key(&id) {
        let browser = session.browser().await?;
        let rdp = browser.rdp_client().await.map_err(super::lifecycle_error)?;
        tab.tab
            .activate()
            .await
            .map_err(|e| super::protocol_error("browsingContext.activate", &e))?;
        let debugger = Debugger::attach_selected(&rdp)
            .await
            .map_err(|e| rdp_error("attach", &e))?;
        let pause = Arc::new(StdMutex::new(None));
        let task = spawn_state_task(&debugger, pause.clone());
        let entry = TabDebugger {
            debugger,
            breakpoints: BTreeMap::new(),
            counter: 0,
            pause,
            _task: task,
        };
        debuggers.insert(id.clone(), entry);
    }
    match debuggers.get_mut(&id) {
        Some(entry) => f(entry).await,
        None => Err(BackendError::InvalidArgument(
            "debugger not attached".into(),
        )),
    }
}

fn rdp_error(request: &str, error: &bidi_browser_lite::BidiError) -> BackendError {
    super::protocol_error(&format!("RDP {request}"), error)
}

fn current_pause(entry: &TabDebugger) -> Option<PauseInfo> {
    entry
        .pause
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

fn not_paused() -> BackendError {
    BackendError::InvalidArgument("the debugger is not paused".into())
}

/// `set_breakpoint`: Firefox breakpoints are set by URL (spike H17).
/// `line_number` is 0-based as on Chrome.
pub(crate) async fn set_breakpoint(
    params: CallToolRequestParams,
    handler: &McpHandler,
) -> ToolResult {
    let args: SetBreakpointTool = parse_args(params)?;
    let Some(url) = args.url.clone() else {
        return Err(CallToolError::from_message(
            "On Firefox set breakpoints by `url` (script_id and script_hash are Chrome concepts)",
        ));
    };
    let location = BreakpointLocation {
        source_url: url.clone(),
        line: args.line_number + 1,
        column: args.column_number,
    };
    let session = handler.firefox(args.instance_id.as_deref()).await?;
    let id = with_debugger(&session, args.tab_id.as_deref(), async |entry| {
        entry
            .debugger
            .set_breakpoint(&location, None)
            .await
            .map_err(|e| rdp_error("setBreakpoint", &e))?;
        entry.counter += 1;
        let id = format!("bp-{}", entry.counter);
        entry.breakpoints.insert(id.clone(), location.clone());
        Ok(id)
    })
    .await?;
    let reply = json!({"breakpointId": id, "url": url, "lineNumber": args.line_number, "columnNumber": args.column_number});
    Ok(text(format!(
        "Breakpoint set successfully.\n{}",
        pretty(&reply)
    )))
}

/// `remove_breakpoint` by MCP id.
pub(crate) async fn remove_breakpoint(
    params: CallToolRequestParams,
    handler: &McpHandler,
) -> ToolResult {
    let args: RemoveBreakpointTool = parse_args(params)?;
    let session = handler.firefox(args.instance_id.as_deref()).await?;
    with_debugger(&session, args.tab_id.as_deref(), async |entry| {
        let Some(location) = entry.breakpoints.remove(&args.breakpoint_id) else {
            let ids: Vec<_> = entry.breakpoints.keys().cloned().collect();
            return Err(BackendError::InvalidArgument(format!(
                "No breakpoint '{}'. Breakpoints: {}",
                args.breakpoint_id,
                if ids.is_empty() {
                    "none".to_string()
                } else {
                    ids.join(", ")
                }
            )));
        };
        entry
            .debugger
            .remove_breakpoint(&location)
            .await
            .map_err(|e| rdp_error("removeBreakpoint", &e))
    })
    .await?;
    Ok(text(format!(
        "Breakpoint removed.\n{}",
        pretty(&json!({"removed": args.breakpoint_id}))
    )))
}

/// `resume`.
pub(crate) async fn resume(params: CallToolRequestParams, handler: &McpHandler) -> ToolResult {
    let ids: TargetArgs = parse_args(params)?;
    let session = handler.firefox(ids.instance_id.as_deref()).await?;
    with_debugger(&session, ids.tab_id.as_deref(), async |entry| {
        entry
            .debugger
            .resume()
            .await
            .map_err(|e| rdp_error("resume", &e))
    })
    .await?;
    Ok(text("Debugger execution resumed."))
}

/// `step_over`: steps, then reports where the thread stopped.
pub(crate) async fn step_over(params: CallToolRequestParams, handler: &McpHandler) -> ToolResult {
    let ids: TargetArgs = parse_args(params)?;
    let session = handler.firefox(ids.instance_id.as_deref()).await?;
    let pause = with_debugger(&session, ids.tab_id.as_deref(), async |entry| {
        let before = current_pause(entry).ok_or_else(not_paused)?;
        entry
            .debugger
            .step_over()
            .await
            .map_err(|e| rdp_error("resume next", &e))?;
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            if let Some(now) = current_pause(entry).filter(|p| *p != before) {
                return Ok(Some(now));
            }
        }
        Ok(None)
    })
    .await?;
    let location = pause
        .and_then(|p| p.location)
        .map(|(line, column)| json!({"line": line, "column": column}));
    let reply = json!({"paused": location.is_some(), "location": location});
    Ok(text(format!(
        "Stepped over to the next expression in the debugger.\n{}",
        pretty(&reply)
    )))
}

/// `evaluate_on_call_frame`: in the top frame of the current pause.
pub(crate) async fn evaluate_on_call_frame(
    params: CallToolRequestParams,
    handler: &McpHandler,
) -> ToolResult {
    let args: EvaluateOnCallFrameTool = parse_args(params)?;
    let session = handler.firefox(args.instance_id.as_deref()).await?;
    let value = with_debugger(&session, args.tab_id.as_deref(), async |entry| {
        current_pause(entry).ok_or_else(not_paused)?;
        let frames = entry
            .debugger
            .frames(0, 1)
            .await
            .map_err(|e| rdp_error("frames", &e))?;
        let top = frames.first().ok_or_else(not_paused)?;
        let evaluation = entry
            .debugger
            .evaluate_on_frame(&top.actor, &args.expression)
            .await
            .map_err(|e| rdp_error("evaluateJSAsync", &e))?;
        match evaluation.exception {
            Some(exception) => Err(BackendError::InvalidArgument(format!(
                "Evaluation threw: {}",
                super::grip::to_json(&exception)
            ))),
            None => Ok(super::grip::to_json(&evaluation.result)),
        }
    })
    .await?;
    Ok(text(pretty(&value)))
}

/// `search_scripts`: same query language and output as on Chrome; scripts
/// are identified by their source actor, with their URL as `scriptHash`.
pub(crate) async fn search_scripts(
    params: CallToolRequestParams,
    handler: &McpHandler,
) -> ToolResult {
    let args: SearchScriptsTool = parse_args(params)?;
    let session = handler.firefox(args.instance_id.as_deref()).await?;
    let report = with_debugger(&session, args.tab_id.as_deref(), async |entry| {
        let sources: Vec<_> = entry
            .debugger
            .sources()
            .into_iter()
            .filter(|s| s.url.is_some())
            .collect();
        if args.query.is_empty() {
            return Ok(format!("Total cached scripts: {}", sources.len()));
        }
        let mut report = SearchReport::default();
        for source in sources {
            match entry.debugger.source_text(&source.actor).await {
                Ok(code) => {
                    let url = source.url.clone().unwrap_or_default();
                    report.add_source(
                        &args.query,
                        &ScriptText {
                            id: &source.actor,
                            hash: &url,
                            source: &code,
                        },
                    );
                }
                Err(e) => report.add_error(&source.actor, e.to_string()),
            }
        }
        Ok(report.text(&args.query))
    })
    .await?;
    Ok(text(report))
}

fn text(value: impl Into<String>) -> CallToolResult {
    CallToolResult::text_content(vec![value.into().into()])
}

fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_default()
}

/// Detaches every tab's debugger (`detach` removes breakpoints first).
pub(crate) async fn detach_all(session: &FirefoxSession) {
    let debuggers: Vec<_> = session.debuggers.lock().await.drain().collect();
    for (_, tab) in debuggers {
        let _ = tab.debugger.detach().await;
    }
}

/// Detaches the debugger of one tab, if any.
pub(crate) async fn detach_tab(session: &FirefoxSession, tab_id: &str) {
    let debugger = session.debuggers.lock().await.remove(tab_id);
    if let Some(tab) = debugger {
        let _ = tab.debugger.detach().await;
    }
}

/// An RDP actor id, with the shortcuts `thread`, `console` (the tab's
/// debugger, attached if needed) and `root`.
pub(crate) async fn resolve_actor(
    session: &FirefoxSession,
    tab_id: Option<&str>,
    to: &str,
) -> Result<String, BackendError> {
    match to {
        "thread" => {
            with_debugger(session, tab_id, async |entry| {
                Ok(entry.debugger.thread_actor())
            })
            .await
        }
        "console" => {
            with_debugger(session, tab_id, async |entry| {
                Ok(entry.debugger.console_actor())
            })
            .await
        }
        other => Ok(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bidi_browser_lite::bidi_lite::rdp::protocol::RdpPacket;

    fn packet(text: &str) -> RdpPacket {
        RdpPacket::parse(text).unwrap()
    }

    /// RDP-05: A pause is tracked once even though Firefox announces it twice.
    #[test]
    fn double_pause_announcement_tracked_once() {
        let pause = StdMutex::new(None);
        let resource = packet(include_str!(
            "../../../tests/fixtures/rdp/resources-thread-state-paused.json"
        ));
        apply_states(&pause, parse_thread_states(&resource));
        let first = pause.lock().unwrap().clone();
        apply_states(&pause, parse_thread_states(&resource));
        assert!(first.is_some(), "the pause is tracked");
        assert_eq!(
            *pause.lock().unwrap(),
            first,
            "a second announcement changes nothing"
        );
        let resumed = packet(include_str!(
            "../../../tests/fixtures/rdp/resources-thread-state-resumed.json"
        ));
        apply_states(&pause, parse_thread_states(&resumed));
        assert!(pause.lock().unwrap().is_none());
    }

    /// RDP-07 / RDP-03 (argument half): no pause, no evaluation; URL required.
    #[tokio::test]
    async fn firefox_breakpoints_by_url() {
        let handler = crate::handler::test_handler();
        let params = serde_json::from_value(
            json!({"name": "set_breakpoint", "arguments": {"script_id": "12", "line_number": 3}}),
        )
        .unwrap();
        let err = set_breakpoint(params, &handler).await.unwrap_err();
        assert!(format!("{err:?}").contains("by `url`"));
    }
}
