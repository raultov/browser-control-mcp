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
    // Tabs the page or the user closed leave their debugger behind.
    let open: Vec<String> = crate::util::sync::read(&session.tabs)
        .tabs
        .keys()
        .cloned()
        .collect();
    debuggers.retain(|tab_id, _| open.contains(tab_id));
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
    // RDP lines are 1-based.
    let Some(line) = args.line_number.checked_add(1) else {
        return Err(CallToolError::from_message("line_number is out of range"));
    };
    let location = BreakpointLocation {
        source_url: url.clone(),
        line,
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

    /// REV-15: a line number at the top of the range is an argument error,
    /// not an overflow, and nothing reaches the browser.
    #[tokio::test]
    async fn huge_line_number_is_an_argument_error() {
        // Given a Firefox default instance that was never launched
        let mut handler = crate::handler::test_handler();
        handler.default_browser = crate::backend::BrowserKind::Firefox;
        // When set_breakpoint asks for line u32::MAX
        let args = serde_json::json!({"url": "http://127.0.0.1/app.js", "line_number": u32::MAX});
        let call = serde_json::from_value(
            serde_json::json!({"name": "set_breakpoint", "arguments": args}),
        )
        .unwrap();
        let error = set_breakpoint(call, &handler).await.unwrap_err();
        // Then the error names the argument
        assert!(error.to_string().contains("line_number"), "{error}");
    }

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

    use super::super::test_support::{self as ts, WAIT};
    use bidi_browser_lite::test_support::MockFirefox;

    const SCRIPT: &str = "https://example.com/test.html";

    /// `set_breakpoint` at 0-based `line` (column given, so no source lookup).
    fn breakpoint(line: u32, tab: Option<&str>) -> CallToolRequestParams {
        let mut args = json!({"url": SCRIPT, "line_number": line, "column_number": 4});
        if let Some(tab) = tab {
            args["tab_id"] = json!(tab);
        }
        ts::call("set_breakpoint", args)
    }

    /// Waits until the state task of `tab` tracked a pause.
    async fn wait_paused(session: &FirefoxSession, tab: &str) {
        for _ in 0..100 {
            if let Some(entry) = session.debuggers.lock().await.get(tab)
                && current_pause(entry).is_some()
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("no pause tracked on {tab}");
    }

    /// RDP-02: The debugger attaches lazily to the requested tab, once.
    #[tokio::test]
    async fn debugger_attaches_lazily_to_requested_tab() {
        // Given tabs A (active) and B on a Firefox instance over the mocks
        let mock = MockFirefox::start(true).await.unwrap();
        let rdp = mock.rdp().unwrap();
        let handler = ts::handler(&mock);
        crate::backend::bidi::tools::open_tab(ts::call("open_tab", json!({})), &handler)
            .await
            .unwrap();
        assert_eq!(rdp.requests_to("root").len(), 0, "nothing attached yet");
        // When set_breakpoint addresses B
        set_breakpoint(breakpoint(1, Some("tab-2")), &handler)
            .await
            .unwrap();
        // Then B was activated through BiDi and the debugger attached
        let activated = mock.bidi().commands_named("browsingContext.activate");
        assert_eq!(activated.len(), 1);
        assert_eq!(activated[0].params["context"], "ctx-1");
        assert_eq!(
            rdp.requests()
                .iter()
                .filter(|r| r.request_type == "getWatcher")
                .count(),
            1
        );
        // And a second debugger tool on B does not attach again
        set_breakpoint(breakpoint(2, Some("tab-2")), &handler)
            .await
            .unwrap();
        assert_eq!(
            rdp.requests()
                .iter()
                .filter(|r| r.request_type == "getWatcher")
                .count(),
            1
        );
        assert_eq!(mock.bidi().count("browsingContext.activate"), 1);
        ts::stop(&handler).await;
    }

    /// RDP-03 / RDP-04: breakpoints get MCP ids; remove_breakpoint uses them.
    #[tokio::test]
    async fn remove_breakpoint_uses_mcp_id() {
        // Given breakpoint bp-1
        let mock = MockFirefox::start(true).await.unwrap();
        let rdp = mock.rdp().unwrap();
        let handler = ts::handler(&mock);
        let set = set_breakpoint(breakpoint(3, None), &handler).await.unwrap();
        assert!(ts::text(&set).contains("bp-1"), "{}", ts::text(&set));
        let sent = rdp.wait_for_request("setBreakpoint", WAIT).await.unwrap();
        assert_eq!(sent.params["location"]["line"], 4, "RDP lines are 1-based");
        // When remove_breakpoint "bp-1" runs twice
        let remove = || ts::call("remove_breakpoint", json!({"breakpoint_id": "bp-1"}));
        remove_breakpoint(remove(), &handler).await.unwrap();
        let second = remove_breakpoint(remove(), &handler).await.unwrap_err();
        // Then the first sent the removal, the second lists no ids
        let removal = rdp
            .wait_for_request("removeBreakpoint", WAIT)
            .await
            .unwrap();
        assert_eq!(removal.params["location"], sent.params["location"]);
        assert!(second.to_string().contains("Breakpoints: none"), "{second}");
        ts::stop(&handler).await;
    }

    /// RDP-06: step_over reports the new location from the combined packet.
    #[tokio::test]
    async fn step_over_reads_combined_packet() {
        // Given a debugger paused on line 7 (1-based, RDP)
        let mock = MockFirefox::start(true).await.unwrap();
        let rdp = mock.rdp().unwrap();
        let handler = ts::handler(&mock);
        set_breakpoint(breakpoint(0, None), &handler).await.unwrap();
        rdp.pause("breakpoint", None);
        let session = handler.firefox(None).await.unwrap();
        wait_paused(&session, "tab-1").await;
        // When step_over runs (answered by resumed + paused in one packet)
        let reply = step_over(ts::call("step_over", json!({})), &handler)
            .await
            .unwrap();
        // Then it reports the new location
        let reply = ts::json_of(&reply);
        assert_eq!(reply["paused"], true);
        assert_eq!(reply["location"]["line"], 8);
        let step = rdp.wait_for_request("resume", WAIT).await.unwrap();
        assert_eq!(step.params["resumeLimit"]["type"], "next");
        ts::stop(&handler).await;
    }

    /// RDP-09: search_scripts keeps Chrome's query language.
    #[tokio::test]
    async fn search_scripts_keeps_query_language() {
        // Given two sources with URLs plus an eval source without one
        let mock = MockFirefox::start(true).await.unwrap();
        let rdp = mock.rdp().unwrap();
        let handler = ts::handler(&mock);
        let search = |query: &str| ts::call("search_scripts", json!({"query": query}));
        search_scripts(search(""), &handler).await.unwrap();
        rdp.emit_resources(
            "source",
            vec![
                json!({"actor": "server1.conn1.watcher2.process7//source30", "url": "https://example.com/app.js"}),
                json!({"actor": "server1.conn1.watcher2.process7//source31", "url": null, "introductionType": "debugger eval"}),
            ],
        );
        let session = handler.firefox(None).await.unwrap();
        for _ in 0..100 {
            let sources = session.debuggers.lock().await["tab-1"].debugger.sources();
            if sources.iter().any(|s| s.actor.ends_with("source30")) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        // When search_scripts runs with "", "@source" and a needle
        let count = ts::text(&search_scripts(search(""), &handler).await.unwrap());
        let sources = ts::text(&search_scripts(search("@source"), &handler).await.unwrap());
        let needle = ts::text(&search_scripts(search("return"), &handler).await.unwrap());
        // Then the eval source is left out and the formats are Chrome's
        assert_eq!(count, "Total cached scripts: 2");
        assert!(
            sources.contains("source30") && !sources.contains("source31"),
            "{sources}"
        );
        assert!(sources.contains("function test()"), "{sources}");
        assert!(
            needle.contains("source30") && needle.contains("source18"),
            "{needle}"
        );
        assert!(needle.contains("\"lineNumber\": 0"), "{needle}");
        ts::stop(&handler).await;
    }

    /// RDP-10: Detaching removes breakpoints first.
    #[tokio::test]
    async fn detach_removes_breakpoints_first() {
        // Given tab-2 with breakpoints bp-1 and bp-2
        let mock = MockFirefox::start(true).await.unwrap();
        let rdp = mock.rdp().unwrap();
        let handler = ts::handler(&mock);
        crate::backend::bidi::tools::open_tab(ts::call("open_tab", json!({})), &handler)
            .await
            .unwrap();
        set_breakpoint(breakpoint(1, Some("tab-2")), &handler)
            .await
            .unwrap();
        set_breakpoint(breakpoint(2, Some("tab-2")), &handler)
            .await
            .unwrap();
        // When the tab is closed
        let close = ts::call("close_tab", json!({"tab_id": "tab-2"}));
        crate::backend::bidi::tools::close_tab(close, &handler)
            .await
            .unwrap();
        rdp.wait_for_request("unwatchTargets", WAIT).await.unwrap();
        // Then both removals were sent before the watcher was unwatched
        let kinds: Vec<String> = rdp
            .requests()
            .into_iter()
            .map(|r| r.request_type)
            .filter(|t| t == "removeBreakpoint" || t.starts_with("unwatch"))
            .collect();
        assert_eq!(kinds[..2], ["removeBreakpoint", "removeBreakpoint"]);
        assert!(
            kinds[2..].iter().all(|t| t.starts_with("unwatch")),
            "{kinds:?}"
        );
        ts::stop(&handler).await;
    }

    /// RDP-12: send_rdp_request resolves actor shortcuts.
    #[tokio::test]
    async fn raw_rdp_resolves_actor_shortcuts() {
        // Given a Firefox instance whose tab thread actor is the mock's thread1
        let mock = MockFirefox::start(true).await.unwrap();
        let rdp = mock.rdp().unwrap();
        let handler = ts::handler(&mock);
        // When send_rdp_request goes to "thread" with request_type "frames"
        let args = json!({"to": "thread", "request_type": "frames", "params": "{}"});
        let reply =
            super::super::raw::send_rdp_request(ts::call("send_rdp_request", args), &handler)
                .await
                .unwrap();
        // Then the mock got it on the thread actor and the reply is JSON
        let thread = "server1.conn1.watcher2.process7//thread1";
        assert_eq!(rdp.requests_to(thread)[0].request_type, "frames");
        assert!(ts::json_of(&reply)["frames"].is_array());
        ts::stop(&handler).await;
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
