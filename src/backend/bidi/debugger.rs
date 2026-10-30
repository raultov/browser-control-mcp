//! The JavaScript debugger of Firefox tabs, over RDP (`bidi_lite::rdp`).
//!
//! One `Debugger` per tab. It is attached when the tab is registered (so
//! its WebSocket frames and page errors are watched from the start, see
//! `watch.rs`) through Firefox's own mapping of the BiDi context to its RDP
//! tab (`Debugger::attach_to_context`), never activating it. A background
//! task follows its thread state, so the tools know whether and where the
//! page is paused (each pause is announced twice: tracked once).

use super::session::{FirefoxSession, FirefoxTab};
use crate::backend::error::BackendError;
use crate::handler::cdp_domains::debugger::evaluate_on_call_frame::EvaluateOnCallFrameTool;
use crate::handler::cdp_domains::debugger::remove_breakpoint::RemoveBreakpointTool;
use crate::handler::cdp_domains::debugger::search_scripts::{
    ScriptText, SearchReport, SearchScriptsTool,
};
use crate::handler::cdp_domains::debugger::set_breakpoint::SetBreakpointTool;
use crate::handler::cdp_domains::event_pump::ListenerHandles;
use crate::handler::debugging::{self, BreakpointIds, BreakpointSet, PauseLocation};
use crate::handler::interruption::Interruption;
use crate::handler::{McpHandler, TargetArgs, parse_args};
use bidi_browser_lite::bidi_lite::error::codes;
use bidi_browser_lite::bidi_lite::rdp::debugger::{
    BreakpointLocation, ThreadState, parse_thread_states,
};
use bidi_browser_lite::{BidiError, Browser, Debugger};
use rust_mcp_sdk::schema::{CallToolError, CallToolRequestParams, CallToolResult};
use serde_json::Value;
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
    /// Source actor, 1-based line and 0-based column of the top frame (RDP's).
    pub(crate) location: Option<(String, u32, u32)>,
}

/// A debugger attached to one tab.
pub(crate) struct TabDebugger {
    pub(crate) debugger: Debugger,
    /// MCP breakpoint ids → locations.
    pub(crate) breakpoints: BreakpointIds<BreakpointLocation>,
    /// Current pause, kept by the state task.
    pub(crate) pause: Arc<StdMutex<Option<PauseInfo>>>,
    _task: ListenerHandles,
    /// The resource watch (WebSocket frames, page errors), see `watch.rs`.
    _watch: ListenerHandles,
}

/// A tab's debugger, locked on its own: the session's map of debuggers is
/// only locked to look one up, so a long call on one tab does not block
/// the other tabs' tools.
pub(crate) type SharedDebugger = Arc<tokio::sync::Mutex<TabDebugger>>;

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
                    location: frame
                        .and_then(|f| f.location)
                        .map(|l| (l.actor, l.line, l.column)),
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

/// Attaches a debugger to `tab` and starts its pause tracking and its
/// resource watch (WebSocket frames, page errors). The tab's RDP descriptor
/// is found from its BiDi context through Firefox's Remote Agent
/// (`Debugger::attach_to_context`, bidi-lite 0.1.3): no activation, no focus
/// change, same-URL tabs told apart.
async fn attach(browser: &Browser, tab: &FirefoxTab) -> Result<TabDebugger, BackendError> {
    let rdp = browser.rdp_client().await.map_err(super::lifecycle_error)?;
    let debugger = Debugger::attach_to_context(&rdp, tab.tab.context_id())
        .await
        .map_err(|e| attach_error(&e))?;
    let pause = Arc::new(StdMutex::new(None));
    let task = spawn_state_task(&debugger, pause.clone());
    let buffers = super::watch::TabBuffers {
        network: tab.network.clone(),
        log: tab.log.clone(),
    };
    // Without the watch the debugger still works: only frames are missing.
    let watch = super::watch::watch(&debugger, &buffers)
        .await
        .inspect_err(|e| tracing::debug!(error = %e, "watchResources failed"))
        .unwrap_or_default();
    let entry = TabDebugger {
        debugger,
        breakpoints: BreakpointIds::default(),
        pause,
        _task: task,
        _watch: watch,
    };
    sync_pauses(&entry).await;
    Ok(entry)
}

/// Pauses on the page's `debugger;` statements only while the tab has a
/// breakpoint: the debugger is always attached, and anti-debugging pages
/// loop on `debugger;` to freeze every tool (thread configuration
/// `shouldPauseOnDebuggerStatement`; breakpoints and steps are unaffected).
/// Best effort: an older Firefox without the option keeps pausing.
async fn sync_pauses(entry: &TabDebugger) {
    let pause = !entry.breakpoints.is_empty();
    let params = serde_json::json!({
        "configuration": { "shouldPauseOnDebuggerStatement": pause }
    });
    let debugger = &entry.debugger;
    let actor = debugger.thread_configuration_actor();
    let _ = debugger
        .client()
        .request(actor, "updateConfiguration", params)
        .await;
}

/// An attach failure as the agent should read it.
fn attach_error(error: &BidiError) -> BackendError {
    match error {
        BidiError::ProtocolError { error, .. } if error == codes::NO_SUCH_FRAME => {
            BackendError::InvalidArgument(
                "Firefox's debugger does not know this tab (closed, or still opening): try again, or call list_tabs".into(),
            )
        }
        other => rdp_error("attach", other),
    }
}

/// Attaches the debugger of tab `id` in the background, so its WebSocket frames and page errors are captured from now on.
/// `true` when the tab is watched (it may have been already). The attach
/// runs without the debuggers lock (it takes several RDP round trips); a
/// debugger another call attached meanwhile wins, and this one is detached.
pub(crate) async fn watch_tab(
    session: &FirefoxSession,
    browser: &Browser,
    id: &str,
    tab: &FirefoxTab,
) -> bool {
    if prune_and_check(session, id).await {
        return true;
    }
    let Ok(entry) = attach(browser, tab).await else {
        return false;
    };
    insert_or_detach(session, id, entry).await;
    true
}

/// Keeps `entry` as tab `id`'s debugger, unless another call attached one
/// meanwhile (that one wins, this one is detached); the tab's debugger.
async fn insert_or_detach(
    session: &FirefoxSession,
    id: &str,
    entry: TabDebugger,
) -> SharedDebugger {
    let mut debuggers = session.debuggers.lock().await;
    if let Some(existing) = debuggers.get(id) {
        let existing = existing.clone();
        drop(debuggers);
        let _ = entry.debugger.detach().await;
        return existing;
    }
    let shared: SharedDebugger = Arc::new(tokio::sync::Mutex::new(entry));
    debuggers.insert(id.to_string(), shared.clone());
    shared
}

/// Forgets the debuggers of tabs that are gone (the page or the user closed
/// them); whether tab `id` has one.
async fn prune_and_check(session: &FirefoxSession, id: &str) -> bool {
    let open: Vec<String> = crate::util::sync::read(&session.tabs)
        .tabs
        .keys()
        .cloned()
        .collect();
    let mut debuggers = session.debuggers.lock().await;
    let gone: Vec<String> = debuggers
        .keys()
        .filter(|tab_id| !open.contains(tab_id))
        .cloned()
        .collect();
    let detached: Vec<SharedDebugger> = gone.iter().filter_map(|g| debuggers.remove(g)).collect();
    let watched = debuggers.contains_key(id);
    drop(debuggers);
    for entry in detached {
        let debugger = entry.lock().await.debugger.clone();
        let _ = debugger.detach().await;
    }
    watched
}

/// [`watch_tab`] for a session's tab, when the session has a debugger port.
pub(crate) async fn watch_session_tab(
    session: &FirefoxSession,
    id: &str,
    tab: &FirefoxTab,
) -> bool {
    if !session.spec.lock().await.has_debugger() {
        return false;
    }
    let Ok(browser) = session.browser().await else {
        return false;
    };
    watch_tab(session, &browser, id, tab).await
}

/// Runs `f` with the tab's debugger, attaching it first if needed. Lock
/// order: the browser (session slot), then the map of debuggers (only to
/// look the tab's up), then the tab's debugger, held while `f` runs.
async fn with_debugger<T>(
    session: &FirefoxSession,
    tab_id: Option<&str>,
    f: impl AsyncFnOnce(&mut TabDebugger) -> Result<T, BackendError>,
) -> Result<T, BackendError> {
    let (id, tab) = session.tab(tab_id).await?;
    let browser = session.browser().await?;
    prune_and_check(session, &id).await;
    let found = session.debuggers.lock().await.get(&id).cloned();
    let shared = match found {
        Some(shared) => shared,
        None => insert_or_detach(session, &id, attach(&browser, &tab).await?).await,
    };
    let mut entry = shared.lock().await;
    f(&mut entry).await
}

fn rdp_error(request: &str, error: &bidi_browser_lite::BidiError) -> BackendError {
    super::protocol_error(&format!("RDP {request}"), error)
}

fn current_pause(entry: &TabDebugger) -> Option<PauseInfo> {
    read_pause(&entry.pause)
}

fn read_pause(pause: &StdMutex<Option<PauseInfo>>) -> Option<PauseInfo> {
    pause
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

fn not_paused() -> BackendError {
    BackendError::InvalidArgument(debugging::NOT_PAUSED.into())
}

/// Where a pause stopped, 0-based as every tool reports it, with the URL of
/// its source.
fn pause_location(debugger: &Debugger, pause: &PauseInfo) -> Option<PauseLocation> {
    let (actor, line, column) = pause.location.clone()?;
    let url = debugger
        .sources()
        .into_iter()
        .find(|s| s.actor == actor)
        .and_then(|s| s.url);
    Some(PauseLocation {
        url,
        line: line.saturating_sub(1),
        column,
    })
}

/// Waits up to `limit` for a pause other than `before`.
async fn next_pause(
    entry: &TabDebugger,
    before: Option<&PauseInfo>,
    limit: Duration,
) -> Option<PauseLocation> {
    let deadline = tokio::time::Instant::now() + limit;
    while tokio::time::Instant::now() < deadline {
        tokio::time::sleep(PAUSE_POLL).await;
        if let Some(now) = current_pause(entry).filter(|p| Some(p) != before) {
            return pause_location(&entry.debugger, &now);
        }
    }
    None
}

/// Runs `operation`, a `call` that runs page code on tab `id` (a click, an
/// evaluation, a navigation), unless the tab's debugger pauses first: the
/// page answers nothing until it is resumed, so the call fails at once
/// saying where it stopped, as on Chrome. A tab without a debugger (no RDP
/// port, or not attached yet) just runs it.
pub(crate) async fn unless_paused<T, E: From<BackendError>>(
    session: &FirefoxSession,
    id: &str,
    call: &str,
    operation: impl std::future::Future<Output = Result<T, E>>,
) -> Result<T, E> {
    race_pause(session, id, operation)
        .await
        .unwrap_or_else(|paused| Err(BackendError::InvalidArgument(paused.during(call)).into()))
}

/// Runs `operation` on tab `id` unless the tab's debugger pauses first:
/// then the pause, for input actions whose effect happened anyway (see
/// [`unless_paused`]).
pub(crate) async fn race_pause<T>(
    session: &FirefoxSession,
    id: &str,
    operation: impl std::future::Future<Output = T>,
) -> Result<T, Interruption> {
    let found = session.debuggers.lock().await.get(id).cloned();
    let Some(shared) = found else {
        return Ok(operation.await);
    };
    let (debugger, pause) = {
        let entry = shared.lock().await;
        (entry.debugger.clone(), entry.pause.clone())
    };
    let before = read_pause(&pause);
    let paused = async {
        loop {
            tokio::time::sleep(PAUSE_POLL).await;
            if let Some(now) = read_pause(&pause).filter(|p| Some(p) != before.as_ref()) {
                return pause_location(&debugger, &now);
            }
        }
    };
    tokio::select! {
        biased;
        outcome = operation => Ok(outcome),
        location = paused => Err(Interruption::Paused(location)),
    }
}

/// How often a call racing a pause ([`race_pause`]) reads the thread
/// state the state task keeps.
const PAUSE_POLL: Duration = Duration::from_millis(25);

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
    let session = handler.firefox(args.instance_id.as_deref()).await?;
    let set = with_debugger(&session, args.tab_id.as_deref(), async |entry| {
        let target = BreakpointTarget {
            url: &url,
            line,
            agent_line: args.line_number,
        };
        let column = bound_column(&entry.debugger, &target, args.column_number).await?;
        let location = BreakpointLocation {
            source_url: url.clone(),
            line,
            column: Some(column),
        };
        if let Some(id) = entry.breakpoints.id_of(&location) {
            return Err(BackendError::InvalidArgument(format!(
                "Breakpoint {id} is already set there (line_number {}, column_number {column} of {url}): Firefox keeps one breakpoint per location. Use {id}, or remove it first.",
                args.line_number
            )));
        }
        let bound = entry
            .debugger
            .set_breakpoint_resolved(&location, None)
            .await
            .map_err(|e| breakpoint_error(&e, &url, args.line_number))?;
        let id = entry.breakpoints.add(bound.location.clone());
        sync_pauses(entry).await;
        Ok(BreakpointSet {
            id,
            url: Some(url.clone()),
            line: args.line_number,
            column: bound.location.column,
            resolved: bound.resolved,
        })
    })
    .await?;
    Ok(set.reply())
}

/// A line a breakpoint is asked for.
struct BreakpointTarget<'a> {
    url: &'a str,
    /// 1-based, as RDP counts.
    line: u32,
    /// 0-based, as the agent gave it (for messages).
    agent_line: u32,
}

/// The column a breakpoint binds to. Firefox silently ignores a breakpoint
/// at a column it cannot break on, so once a script with the URL is loaded
/// the column is the first breakable one at or after the asked one (as
/// Chrome does), or the first of the line when none is asked. Without a
/// loaded script a given column is kept (it binds when the script loads).
async fn bound_column(
    debugger: &Debugger,
    target: &BreakpointTarget<'_>,
    asked: Option<u32>,
) -> Result<u32, BackendError> {
    let BreakpointTarget {
        url, agent_line, ..
    } = *target;
    match (breakable_columns(debugger, url, target.line).await?, asked) {
        (None, Some(asked)) => Ok(asked),
        (None, None) => Err(BackendError::InvalidArgument(format!(
            "no loaded script has the URL {url}: give column_number, or set the breakpoint once the script is loaded"
        ))),
        (Some(columns), asked) => {
            let from = asked.unwrap_or(0);
            columns
                .into_iter()
                .filter(|c| *c >= from)
                .min()
                .ok_or_else(|| {
                    let after =
                        asked.map_or(String::new(), |c| format!(" at or after column_number {c}"));
                    BackendError::InvalidArgument(format!(
                        "line_number {agent_line} of {url} has no breakable position{after}"
                    ))
                })
        }
    }
}

/// The breakable columns of `line` (1-based) in every loaded source with
/// `url` (a page has one source per inline script), or `None` when none is
/// loaded.
async fn breakable_columns(
    debugger: &Debugger,
    url: &str,
    line: u32,
) -> Result<Option<Vec<u32>>, BackendError> {
    let sources: Vec<_> = debugger
        .sources()
        .into_iter()
        .filter(|source| source.url.as_deref() == Some(url))
        .collect();
    if sources.is_empty() {
        return Ok(None);
    }
    let query = serde_json::json!({ "query": {
        "start": { "line": line, "column": 0 },
        "end": { "line": line.saturating_add(1), "column": 0 },
    } });
    let mut columns = Vec::new();
    for source in sources {
        let reply = debugger
            .client()
            .request(&source.actor, "getBreakpointPositionsCompressed", &query)
            .await
            .map_err(|e| rdp_error("getBreakpointPositionsCompressed", &e))?;
        let positions = reply.body["positions"][line.to_string()].as_array();
        columns.extend(
            positions
                .into_iter()
                .flatten()
                .filter_map(Value::as_u64)
                .filter_map(|c| u32::try_from(c).ok()),
        );
    }
    Ok(Some(columns))
}

/// A refused breakpoint, worded with the agent's 0-based line (bidi-lite's
/// messages count lines from 1).
fn breakpoint_error(error: &BidiError, url: &str, line: u32) -> BackendError {
    match error {
        BidiError::ProtocolError { error, message, .. } if error == codes::INVALID_ARGUMENT => {
            let reason = if message.contains("no breakable position") {
                format!("line_number {line} of {url} has no breakable position")
            } else if message.contains("no loaded source") {
                format!(
                    "no loaded script has the URL {url}: give column_number, or set the breakpoint once the script is loaded"
                )
            } else {
                message.clone()
            };
            BackendError::InvalidArgument(reason)
        }
        other => rdp_error("setBreakpoint", other),
    }
}

/// `remove_breakpoint` by MCP id.
pub(crate) async fn remove_breakpoint(
    params: CallToolRequestParams,
    handler: &McpHandler,
) -> ToolResult {
    let args: RemoveBreakpointTool = parse_args(params)?;
    let session = handler.firefox(args.instance_id.as_deref()).await?;
    with_debugger(&session, args.tab_id.as_deref(), async |entry| {
        let location = entry
            .breakpoints
            .get(&args.breakpoint_id)
            .map_err(BackendError::InvalidArgument)?
            .clone();
        entry
            .debugger
            .remove_breakpoint(&location)
            .await
            .map_err(|e| rdp_error("removeBreakpoint", &e))?;
        entry.breakpoints.remove(&args.breakpoint_id);
        sync_pauses(entry).await;
        Ok(())
    })
    .await?;
    Ok(debugging::removed(&args.breakpoint_id))
}

/// `resume`: reports a pause that follows right away (another breakpoint).
pub(crate) async fn resume(params: CallToolRequestParams, handler: &McpHandler) -> ToolResult {
    let ids: TargetArgs = parse_args(params)?;
    let session = handler.firefox(ids.instance_id.as_deref()).await?;
    let location = with_debugger(&session, ids.tab_id.as_deref(), async |entry| {
        let before = current_pause(entry);
        entry
            .debugger
            .resume()
            .await
            .map_err(|e| rdp_error("resume", &e))?;
        Ok(next_pause(entry, before.as_ref(), debugging::RESUME_WAIT).await)
    })
    .await?;
    Ok(debugging::resumed(location.as_ref()))
}

/// `step_over`: steps, then reports where the thread stopped (0-based).
pub(crate) async fn step_over(params: CallToolRequestParams, handler: &McpHandler) -> ToolResult {
    let ids: TargetArgs = parse_args(params)?;
    let session = handler.firefox(ids.instance_id.as_deref()).await?;
    let location = with_debugger(&session, ids.tab_id.as_deref(), async |entry| {
        let before = current_pause(entry).ok_or_else(not_paused)?;
        entry
            .debugger
            .step_over()
            .await
            .map_err(|e| rdp_error("resume next", &e))?;
        Ok(next_pause(entry, Some(&before), debugging::STEP_WAIT).await)
    })
    .await?;
    Ok(debugging::stepped(location.as_ref()))
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
            Some(exception) => Err(BackendError::InvalidArgument(debugging::frame_exception(
                &super::grip::thrown(&evaluation.raw, &exception),
            ))),
            None => Ok(super::grip::evaluation(&evaluation.result)),
        }
    })
    .await?;
    Ok(value.reply())
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
                            // Firefox gives the whole document's text.
                            start: (0, 0),
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

/// Detaches every tab's debugger (`detach` removes breakpoints first).
pub(crate) async fn detach_all(session: &FirefoxSession) {
    let debuggers: Vec<_> = session.debuggers.lock().await.drain().collect();
    for (_, tab) in debuggers {
        let debugger = tab.lock().await.debugger.clone();
        let _ = debugger.detach().await;
    }
}

/// Detaches the debugger of one tab, if any.
pub(crate) async fn detach_tab(session: &FirefoxSession, tab_id: &str) {
    let debugger = session.debuggers.lock().await.remove(tab_id);
    if let Some(tab) = debugger {
        let debugger = tab.lock().await.debugger.clone();
        let _ = debugger.detach().await;
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
    use serde_json::json;

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

    const SCRIPT: &str = "https://example.com/test.html";

    /// `set_breakpoint` at 0-based `line`, column 4 (test.html binds it to
    /// its breakable column 10).
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
            let entry = session.debuggers.lock().await.get(tab).cloned();
            if let Some(entry) = entry
                && current_pause(&*entry.lock().await).is_some()
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("no pause tracked on {tab}");
    }

    /// The captured `listTabs` descriptors of three tabs (browserId 2, 11
    /// and 12; the last two on the same URL), with `ctx-1`/`ctx-2` mapped to
    /// the same-URL ones.
    fn three_tabs(rdp: &bidi_browser_lite::test_support::MockRdp) {
        use bidi_browser_lite::test_support::fixtures::RDP_LIST_TABS_TWO_TABS;
        let tabs: Value = serde_json::from_str(RDP_LIST_TABS_TWO_TABS).unwrap();
        rdp.set_tabs(tabs["tabs"].as_array().unwrap().clone());
        rdp.map_context("ctx-1", 11);
        rdp.map_context("ctx-2", 12);
    }

    /// The tab descriptors `getWatcher` was asked of, in order.
    fn watched_descriptors(rdp: &bidi_browser_lite::test_support::MockRdp) -> Vec<String> {
        rdp.requests()
            .into_iter()
            .filter(|r| r.request_type == "getWatcher")
            .map(|r| r.to)
            .collect()
    }

    /// RDP-02 / FWS-06 / DBG-06: every tab attaches to its own RDP
    /// descriptor through Firefox's lookup of its BiDi context (same-URL
    /// tabs told apart), never by activating it, and only once.
    #[tokio::test]
    async fn debugger_attaches_lazily_to_requested_tab() {
        // Given a Firefox with three tabs, the last two on the same URL
        let mock = ts::firefox_with_debugger().await;
        let rdp = mock.rdp().unwrap();
        three_tabs(rdp);
        let handler = ts::handler(&mock);
        // When the session starts and two tabs open
        for _ in 0..2 {
            crate::backend::bidi::tools::open_tab(ts::call("open_tab", json!({})), &handler)
                .await
                .unwrap();
        }
        // Then each tab was attached to its own descriptor, nothing activated
        assert_eq!(
            watched_descriptors(rdp),
            [
                "server1.conn0.tabDescriptor1",
                "server1.conn0.tabDescriptor2",
                "server1.conn0.tabDescriptor3"
            ]
        );
        assert!(rdp.watched_resources().contains(&"websocket".to_string()));
        // And a debugger tool on an attached tab does not attach again
        set_breakpoint(breakpoint(2, Some("tab-3")), &handler)
            .await
            .unwrap();
        assert_eq!(watched_descriptors(rdp).len(), 3);
        assert_eq!(mock.bidi().count("browsingContext.activate"), 0);
        ts::stop(&handler).await;
    }

    /// DBG-07: a tab Firefox's lookup cannot find is neither watched nor
    /// activated; a debugger tool on it fails saying so.
    #[tokio::test]
    async fn unknown_tab_is_not_attached() {
        // Given a second tab RDP does not know (its context is not mapped)
        let mock = ts::firefox_with_debugger().await;
        let rdp = mock.rdp().unwrap();
        let handler = ts::handler(&mock);
        crate::backend::bidi::tools::open_tab(ts::call("open_tab", json!({})), &handler)
            .await
            .unwrap();
        // When set_breakpoint addresses it
        let error = set_breakpoint(breakpoint(1, Some("tab-2")), &handler)
            .await
            .unwrap_err()
            .to_string();
        // Then it fails, only tab-1 was attached and nothing was activated
        assert!(error.contains("debugger does not know this tab"), "{error}");
        assert_eq!(watched_descriptors(rdp).len(), 1);
        assert_eq!(mock.bidi().count("browsingContext.activate"), 0);
        ts::stop(&handler).await;
    }

    /// DBG-08: a `forwardingCancelled` racing a `listTabs` reply (after a
    /// detach) does not break the next attach.
    #[tokio::test]
    async fn attach_survives_forwarding_cancelled() {
        use bidi_browser_lite::test_support::fixtures::RDP_FORWARDING_CANCELLED;
        // Given tab-2 opened, attached and closed (its debugger detached)
        let mock = ts::firefox_with_debugger().await;
        let rdp = mock.rdp().unwrap();
        three_tabs(rdp);
        let handler = ts::handler(&mock);
        crate::backend::bidi::tools::open_tab(ts::call("open_tab", json!({})), &handler)
            .await
            .unwrap();
        let close = ts::call("close_tab", json!({"tab_id": "tab-2"}));
        crate::backend::bidi::tools::close_tab(close, &handler)
            .await
            .unwrap();
        // When the next listTabs reply follows a forwardingCancelled
        rdp.emit_before_reply(
            "listTabs",
            serde_json::from_str(RDP_FORWARDING_CANCELLED).unwrap(),
        );
        crate::backend::bidi::tools::open_tab(ts::call("open_tab", json!({})), &handler)
            .await
            .unwrap();
        // Then the new tab was still attached to its descriptor
        let watched = watched_descriptors(rdp);
        assert_eq!(
            watched.last().unwrap(),
            "server1.conn0.tabDescriptor3",
            "{watched:?}"
        );
        ts::stop(&handler).await;
    }

    /// Holds tab-1's debugger (as a long call would) from `started` until
    /// `held` fires.
    async fn hold_debugger(
        session: Arc<FirefoxSession>,
        started: tokio::sync::oneshot::Sender<()>,
        held: tokio::sync::oneshot::Receiver<()>,
    ) -> Result<(), BackendError> {
        with_debugger(&session, Some("tab-1"), async |_| {
            let _ = started.send(());
            let _ = held.await;
            Ok(())
        })
        .await
    }

    /// AUD-DB-10: a debugger call in progress on one tab does not block
    /// another tab's tools (one lock covered every tab's debugger: a 10 s
    /// evaluate_on_call_frame on tab-1 delayed get_console_logs on tab-2).
    #[tokio::test]
    async fn a_busy_debugger_does_not_block_other_tabs() {
        // Given tab-1's debugger busy in a call that does not end yet
        let mock = ts::firefox_with_debugger().await;
        let handler = ts::handler(&mock);
        let session = handler.firefox(None).await.unwrap();
        session.browser().await.unwrap();
        let (release, held) = tokio::sync::oneshot::channel::<()>();
        let (started, running) = tokio::sync::oneshot::channel::<()>();
        let busy = tokio::spawn(hold_debugger(session.clone(), started, held));
        running.await.unwrap();
        // When another tab's attach check runs
        let other = tokio::time::timeout(WAIT, prune_and_check(&session, "tab-9")).await;
        // Then it answers at once
        assert!(other.is_ok(), "blocked by tab-1's debugger call");
        let _ = release.send(());
        busy.await.unwrap().unwrap();
        ts::stop(&handler).await;
    }

    /// REV-F11-7: the debugger of a tab the page closed is detached at the
    /// next attach check, even if no debugger tool runs.
    #[tokio::test]
    async fn closed_tabs_lose_their_debugger() {
        // Given tab-1 watched since the session started
        let mock = ts::firefox_with_debugger().await;
        let rdp = mock.rdp().unwrap();
        let handler = ts::handler(&mock);
        let session = handler.firefox(None).await.unwrap();
        session.browser().await.unwrap();
        assert!(session.debuggers.lock().await.contains_key("tab-1"));
        // When the page closes it and a log tool checks another tab
        crate::util::sync::write(&session.tabs).remove_context("ctx-0");
        prune_and_check(&session, "tab-9").await;
        // Then its debugger was detached (its targets unwatched)
        assert!(session.debuggers.lock().await.is_empty());
        assert!(rdp.wait_for_request("unwatchTargets", WAIT).await.is_some());
        ts::stop(&handler).await;
    }

    /// RDP-03 / RDP-04: breakpoints get MCP ids; remove_breakpoint uses them.
    #[tokio::test]
    async fn remove_breakpoint_uses_mcp_id() {
        // Given breakpoint bp-1
        let mock = ts::firefox_with_debugger().await;
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

    /// AUD-EV-07 (Firefox): without breakpoints the page's `debugger;`
    /// statements do not pause; the first breakpoint turns them on and
    /// removing the last turns them off.
    #[tokio::test]
    async fn debugger_statements_pause_only_with_breakpoints() {
        let mock = ts::firefox_with_debugger().await;
        let rdp = mock.rdp().unwrap();
        let handler = ts::handler(&mock);
        let flags = || -> Vec<Value> {
            rdp.requests()
                .into_iter()
                .filter(|r| r.request_type == "updateConfiguration")
                .filter_map(|r| {
                    r.params["configuration"]
                        .get("shouldPauseOnDebuggerStatement")
                        .cloned()
                })
                .collect()
        };
        set_breakpoint(breakpoint(3, None), &handler).await.unwrap();
        assert_eq!(flags(), [json!(false), json!(true)]);
        let remove = ts::call("remove_breakpoint", json!({"breakpoint_id": "bp-1"}));
        remove_breakpoint(remove, &handler).await.unwrap();
        assert_eq!(flags(), [json!(false), json!(true), json!(false)]);
        ts::stop(&handler).await;
    }

    /// DBG-01 (Firefox): the reply has the column Firefox binds to and
    /// whether a loaded script has that URL.
    #[tokio::test]
    async fn breakpoint_reply_has_resolved_column() {
        // Given the mock's loaded test.html (breakable at column 10)
        let mock = ts::firefox_with_debugger().await;
        let rdp = mock.rdp().unwrap();
        let handler = ts::handler(&mock);
        // When set_breakpoint gives no column
        let args = json!({"url": SCRIPT, "line_number": 6});
        let reply = set_breakpoint(ts::call("set_breakpoint", args), &handler)
            .await
            .unwrap();
        // Then the reply has the bound 0-based location, and that was sent
        assert_eq!(
            ts::json_of(&reply),
            json!({"breakpoint_id": "bp-1", "url": SCRIPT, "line_number": 6, "column_number": 10, "resolved": true})
        );
        let sent = rdp.wait_for_request("setBreakpoint", WAIT).await.unwrap();
        assert_eq!(sent.params["location"]["line"], 7);
        assert_eq!(sent.params["location"]["column"], 10);
        // And a script not loaded yet is set with its column, unresolved
        let later =
            json!({"url": "https://example.com/later.js", "line_number": 0, "column_number": 0});
        let reply = set_breakpoint(ts::call("set_breakpoint", later), &handler)
            .await
            .unwrap();
        assert_eq!(ts::json_of(&reply)["resolved"], false);
        ts::stop(&handler).await;
    }

    /// AUD-DB-03: a given column Firefox cannot break on moves to the next
    /// breakable one (Firefox ignored it silently, so the page never paused).
    #[tokio::test]
    async fn given_column_moves_to_a_breakable_one() {
        // Given the mock's loaded test.html, breakable at column 10 only
        let mock = ts::firefox_with_debugger().await;
        let rdp = mock.rdp().unwrap();
        let handler = ts::handler(&mock);
        // When set_breakpoint asks for column 4 of line 6
        let args = json!({"url": SCRIPT, "line_number": 6, "column_number": 4});
        let reply = set_breakpoint(ts::call("set_breakpoint", args), &handler)
            .await
            .unwrap();
        // Then column 10 was sent and reported
        assert_eq!(ts::json_of(&reply)["column_number"], 10);
        let sent = rdp.wait_for_request("setBreakpoint", WAIT).await.unwrap();
        assert_eq!(sent.params["location"]["column"], 10);
        // And a column past every breakable one is an error
        let args = json!({"url": SCRIPT, "line_number": 6, "column_number": 11});
        let error = set_breakpoint(ts::call("set_breakpoint", args), &handler)
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            format!(
                "line_number 6 of {SCRIPT} has no breakable position at or after column_number 11"
            )
        );
        ts::stop(&handler).await;
    }

    /// AUD-DB-04: Firefox keeps one breakpoint per location, so a second
    /// one there is refused naming the first (removing either used to
    /// remove both).
    #[tokio::test]
    async fn duplicate_breakpoint_is_refused() {
        let mock = ts::firefox_with_debugger().await;
        let rdp = mock.rdp().unwrap();
        let handler = ts::handler(&mock);
        let args = || ts::call("set_breakpoint", json!({"url": SCRIPT, "line_number": 6}));
        set_breakpoint(args(), &handler).await.unwrap();
        let error = set_breakpoint(args(), &handler)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.starts_with("Breakpoint bp-1 is already set there"),
            "{error}"
        );
        let sets = rdp
            .requests()
            .into_iter()
            .filter(|r| r.request_type == "setBreakpoint")
            .count();
        assert_eq!(sets, 1);
        ts::stop(&handler).await;
    }

    /// DBG-09: a column-less breakpoint on a line only a later inline
    /// script of the page can break on binds there.
    #[tokio::test]
    async fn breakpoint_binds_in_a_later_inline_script() {
        use bidi_browser_lite::test_support::fixtures::{
            RDP_BREAKPOINT_POSITIONS_INLINE_NONE, RDP_BREAKPOINT_POSITIONS_INLINE_SECOND,
        };
        // Given a page with two inline scripts, line 9 (1-based) breakable
        // at column 2 only in the second
        let mock = ts::firefox_with_debugger().await;
        let rdp = mock.rdp().unwrap();
        let page = "http://127.0.0.1:8/inline";
        for fixture in [
            RDP_BREAKPOINT_POSITIONS_INLINE_NONE,
            RDP_BREAKPOINT_POSITIONS_INLINE_SECOND,
        ] {
            let reply: Value = serde_json::from_str(fixture).unwrap();
            rdp.add_source(page, reply["positions"].clone());
        }
        let handler = ts::handler(&mock);
        // When set_breakpoint gives line_number 8 and no column
        let args = json!({"url": page, "line_number": 8});
        let reply = set_breakpoint(ts::call("set_breakpoint", args), &handler)
            .await
            .unwrap();
        // Then it bound to the second script's column
        let body = ts::json_of(&reply);
        assert_eq!(
            (body["column_number"].as_u64(), body["resolved"].as_bool()),
            (Some(2), Some(true))
        );
        ts::stop(&handler).await;
    }

    /// DBG-10: errors name the agent's 0-based line: no column for a script
    /// not loaded, or a line no loaded script can break on.
    #[tokio::test]
    async fn refused_breakpoints_are_argument_errors() {
        let mock = ts::firefox_with_debugger().await;
        let rdp = mock.rdp().unwrap();
        let page = "http://127.0.0.1:8/inline";
        rdp.add_source(page, json!({}));
        let handler = ts::handler(&mock);
        let later = json!({"url": "https://example.com/later.js", "line_number": 0});
        let error = set_breakpoint(ts::call("set_breakpoint", later), &handler)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("give column_number"), "{error}");
        let nowhere = json!({"url": page, "line_number": 4});
        let error = set_breakpoint(ts::call("set_breakpoint", nowhere), &handler)
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            format!("line_number 4 of {page} has no breakable position")
        );
        ts::stop(&handler).await;
    }

    /// RDP-06 / DBG-04: step_over reports the new location from the
    /// combined packet, 0-based, with the script URL.
    #[tokio::test]
    async fn step_over_reads_combined_packet() {
        // Given a debugger paused on line 7 (1-based, RDP)
        let mock = ts::firefox_with_debugger().await;
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
        assert_eq!(reply["location"]["line_number"], 7, "RDP line 8, 0-based");
        assert_eq!(reply["location"]["url"], SCRIPT);
        let step = rdp.wait_for_request("resume", WAIT).await.unwrap();
        assert_eq!(step.params["resumeLimit"]["type"], "next");
        ts::stop(&handler).await;
    }

    /// ISSUE-1: a call that runs page code fails as soon as the page pauses
    /// in the debugger (input.performActions used to time out after 10 s),
    /// naming the location; a call that answers keeps its result.
    #[tokio::test]
    async fn pause_interrupts_a_page_call() {
        // Given a tab with a breakpoint and a call the page will not answer
        let mock = ts::firefox_with_debugger().await;
        let rdp = mock.rdp().unwrap();
        let handler = ts::handler(&mock);
        set_breakpoint(breakpoint(0, None), &handler).await.unwrap();
        let session = handler.firefox(None).await.unwrap();
        let answers = async { Ok::<_, BackendError>(5) };
        let answered = unless_paused(&session, "tab-1", "click_element", answers).await;
        assert_eq!(answered.ok(), Some(5));
        let blocked = std::future::pending::<Result<(), BackendError>>();
        let call = unless_paused(&session, "tab-1", "click_element", blocked);
        // When the page pauses while it runs
        let pause = async {
            tokio::time::sleep(Duration::from_millis(100)).await;
            rdp.pause("breakpoint", None);
            std::future::pending::<()>().await
        };
        let outcome = tokio::select! {
            outcome = tokio::time::timeout(WAIT, call) => outcome,
            () = pause => unreachable!(),
        };
        // Then the call fails at once with where the page stopped
        let error = outcome
            .expect("the call waited past the pause")
            .unwrap_err();
        let error = error.to_string();
        assert!(
            error.contains(&format!("paused in the debugger at {SCRIPT} line"))
                && error.contains("while running click_element"),
            "{error}"
        );
        ts::stop(&handler).await;
    }

    /// RDP-09: search_scripts keeps Chrome's query language.
    #[tokio::test]
    async fn search_scripts_keeps_query_language() {
        // Given two sources with URLs plus an eval source without one
        let mock = ts::firefox_with_debugger().await;
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
            let entry = session.debuggers.lock().await["tab-1"].clone();
            let sources = entry.lock().await.debugger.sources();
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
        let mock = ts::firefox_with_debugger().await;
        let rdp = mock.rdp().unwrap();
        three_tabs(rdp);
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
        let mock = ts::firefox_with_debugger().await;
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
