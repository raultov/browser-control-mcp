//! The JavaScript debugger of Firefox tabs, over RDP (`bidi_lite::rdp`).
//!
//! One `Debugger` per tab. It is attached when the tab is registered (so
//! its WebSocket frames and page errors are watched from the start, see
//! `watch.rs`) by matching the tab's position in BiDi's and RDP's tab lists,
//! without activating it; when that match is not possible, the first
//! debugger tool attaches it by activating the tab (spike H15). A background
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
use crate::handler::{McpHandler, TargetArgs, parse_args};
use bidi_browser_lite::bidi_lite::rdp::client::{RdpClient, TabDescriptor};
use bidi_browser_lite::bidi_lite::rdp::debugger::{
    BreakpointLocation, ThreadState, parse_thread_states,
};
use bidi_browser_lite::{Browser, Debugger};
use rust_mcp_sdk::schema::{CallToolError, CallToolRequestParams, CallToolResult};
use serde_json::{Value, json};
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

/// Whether attaching may bring the tab to the front, when its RDP
/// descriptor cannot be told apart from the others.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Activation {
    /// A debugger tool asked: attach whatever it takes.
    Allowed,
    /// Background attach (frames, page errors): never steal the focus.
    Avoided,
}

/// The RDP descriptor of the tab showing `context`. BiDi and RDP name tabs
/// differently (a UUID, an actor) but list them in the same tab order, so
/// the position matches them. `None` (no guess) when the lists differ in
/// length (a tab opening or closing meanwhile) or any pair of web pages at
/// the same position shows different URLs (another order, another window).
async fn descriptor_of(browser: &Browser, rdp: &RdpClient, context: &str) -> Option<TabDescriptor> {
    let contexts = browser.list_tabs().await.ok()?;
    let tabs = rdp.list_tabs().await.ok()?;
    let urls: Vec<(&str, &str)> = contexts
        .iter()
        .zip(&tabs)
        .map(|(c, t)| (c.url.as_str(), t.url.as_str()))
        .collect();
    if contexts.len() != tabs.len() || !same_pages(&urls) {
        return None;
    }
    let index = contexts.iter().position(|c| c.context == context)?;
    tabs.into_iter().nth(index)
}

/// Whether BiDi's and RDP's URLs, pair by pair, can be the same tabs: web
/// pages must match (fragment aside); other pages (`about:`, error pages,
/// which RDP shows as `about:neterror?...` and BiDi as the requested URL)
/// prove nothing either way.
pub(crate) fn same_pages(pairs: &[(&str, &str)]) -> bool {
    let web = |url: &str| -> Option<String> {
        let parsed = url::Url::parse(url).ok()?;
        matches!(parsed.scheme(), "http" | "https").then(|| {
            let mut page = parsed;
            page.set_fragment(None);
            page.to_string()
        })
    };
    pairs.iter().all(|(bidi, rdp)| match (web(bidi), web(rdp)) {
        (Some(a), Some(b)) => a == b,
        _ => true,
    })
}

/// Attaches a debugger to `tab` and starts its pause tracking and its
/// resource watch (WebSocket frames, page errors); `None` when the tab could
/// only be found by activating it and `activation` forbids it.
async fn attach(
    browser: &Browser,
    tab: &FirefoxTab,
    activation: Activation,
) -> Result<Option<TabDebugger>, BackendError> {
    let rdp = browser.rdp_client().await.map_err(super::lifecycle_error)?;
    let debugger = match descriptor_of(browser, &rdp, tab.tab.context_id()).await {
        Some(descriptor) => Debugger::attach(&rdp, &descriptor).await,
        None if activation == Activation::Allowed => {
            tab.tab
                .activate()
                .await
                .map_err(|e| super::protocol_error("browsingContext.activate", &e))?;
            Debugger::attach_selected(&rdp).await
        }
        None => return Ok(None),
    }
    .map_err(|e| rdp_error("attach", &e))?;
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
    Ok(Some(TabDebugger {
        debugger,
        breakpoints: BreakpointIds::default(),
        pause,
        _task: task,
        _watch: watch,
    }))
}

/// Attaches the debugger of tab `id` in the background (never activating
/// it), so its WebSocket frames and page errors are captured from now on.
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
    let Ok(Some(entry)) = attach(browser, tab, Activation::Avoided).await else {
        return false;
    };
    let mut debuggers = session.debuggers.lock().await;
    if debuggers.contains_key(id) {
        drop(debuggers);
        let _ = entry.debugger.detach().await;
        return true;
    }
    debuggers.insert(id.to_string(), entry);
    true
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
    let detached: Vec<TabDebugger> = gone.iter().filter_map(|g| debuggers.remove(g)).collect();
    let watched = debuggers.contains_key(id);
    drop(debuggers);
    for entry in detached {
        let _ = entry.debugger.detach().await;
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
/// order: the browser (session slot) before the debuggers.
async fn with_debugger<T>(
    session: &FirefoxSession,
    tab_id: Option<&str>,
    f: impl AsyncFnOnce(&mut TabDebugger) -> Result<T, BackendError>,
) -> Result<T, BackendError> {
    let (id, tab) = session.tab(tab_id).await?;
    let browser = session.browser().await?;
    prune_and_check(session, &id).await;
    let mut debuggers = session.debuggers.lock().await;
    if !debuggers.contains_key(&id)
        && let Some(entry) = attach(&browser, &tab, Activation::Allowed).await?
    {
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
    BackendError::InvalidArgument(debugging::NOT_PAUSED.into())
}

/// Where a pause stopped, 0-based as every tool reports it, with the URL of
/// its source.
fn pause_location(entry: &TabDebugger, pause: &PauseInfo) -> Option<PauseLocation> {
    let (actor, line, column) = pause.location.clone()?;
    let url = entry
        .debugger
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
        tokio::time::sleep(Duration::from_millis(25)).await;
        if let Some(now) = current_pause(entry).filter(|p| Some(p) != before) {
            return pause_location(entry, &now);
        }
    }
    None
}

/// Where 0-based `line` of the scripts loaded from a URL can break.
#[derive(Debug, PartialEq)]
enum Breakable {
    /// No loaded script has the URL (the breakpoint binds when one loads).
    NotLoaded,
    /// The first breakable column of the line.
    At(u32),
    /// Loaded, but no script with the URL can break on that line.
    Nowhere,
}

/// The first breakable column of 0-based `line` among the loaded scripts
/// with `url` (a page with several inline scripts has one source per
/// `<script>`, all with the page's URL): what Firefox binds a column-less
/// breakpoint to.
async fn breakable_column(
    debugger: &Debugger,
    url: &str,
    line: u32,
) -> Result<Breakable, BackendError> {
    let rdp_line = line.saturating_add(1);
    let query = json!({"query": {
        "start": {"line": rdp_line, "column": 0},
        "end": {"line": rdp_line.saturating_add(1), "column": 0},
    }});
    let mut found = Breakable::NotLoaded;
    for source in debugger
        .sources()
        .into_iter()
        .filter(|s| s.url.as_deref() == Some(url))
    {
        found = Breakable::Nowhere;
        let reply = debugger
            .client()
            .request(
                &source.actor,
                "getBreakpointPositionsCompressed",
                query.clone(),
            )
            .await
            .map_err(|e| rdp_error("getBreakpointPositionsCompressed", &e))?;
        let column = reply.body["positions"][rdp_line.to_string()]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_u64)
            .min()
            .and_then(|c| u32::try_from(c).ok());
        if let Some(column) = column {
            return Ok(Breakable::At(column));
        }
    }
    Ok(found)
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
    let session = handler.firefox(args.instance_id.as_deref()).await?;
    let set = with_debugger(&session, args.tab_id.as_deref(), async |entry| {
        // Resolved here (bidi-lite resolves too, but does not say where), so
        // the reply has the column Firefox binds to.
        let breakable = breakable_column(&entry.debugger, &url, args.line_number).await?;
        let column = match (&breakable, args.column_number) {
            (_, Some(column)) => Some(column),
            (Breakable::At(column), None) => Some(*column),
            (Breakable::NotLoaded, None) => None,
            (Breakable::Nowhere, None) => {
                return Err(BackendError::InvalidArgument(format!(
                    "line_number {} of {url} has no breakable position",
                    args.line_number
                )));
            }
        };
        let location = BreakpointLocation {
            source_url: url.clone(),
            line,
            column,
        };
        entry
            .debugger
            .set_breakpoint(&location, None)
            .await
            .map_err(|e| rdp_error("setBreakpoint", &e))?;
        Ok(BreakpointSet {
            id: entry.breakpoints.add(location.clone()),
            url: Some(url.clone()),
            line: args.line_number,
            column: location.column,
            resolved: breakable != Breakable::NotLoaded,
        })
    })
    .await?;
    Ok(set.reply())
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

    fn watchers(rdp: &bidi_browser_lite::test_support::MockRdp) -> usize {
        rdp.requests()
            .iter()
            .filter(|r| r.request_type == "getWatcher")
            .count()
    }

    /// RDP-02 / FWS-06: tabs BiDi and RDP list alike are attached when the
    /// session starts, without activation; a tab they cannot match is
    /// attached by the first debugger tool, by activating it, once.
    #[tokio::test]
    async fn debugger_attaches_lazily_to_requested_tab() {
        // Given a Firefox instance over the mocks (one tab in each list)
        let mock = MockFirefox::start(true).await.unwrap();
        let rdp = mock.rdp().unwrap();
        let handler = ts::handler(&mock);
        // When the session starts and a second tab opens
        crate::backend::bidi::tools::open_tab(ts::call("open_tab", json!({})), &handler)
            .await
            .unwrap();
        // Then tab-1 was attached and watched, nothing was activated
        assert_eq!(watchers(rdp), 1);
        assert!(rdp.watched_resources().contains(&"websocket".to_string()));
        assert_eq!(mock.bidi().count("browsingContext.activate"), 0);
        // When set_breakpoint addresses tab-2 (RDP lists one tab, BiDi two)
        set_breakpoint(breakpoint(1, Some("tab-2")), &handler)
            .await
            .unwrap();
        // Then tab-2 was activated through BiDi and attached
        let activated = mock.bidi().commands_named("browsingContext.activate");
        assert_eq!(activated.len(), 1);
        assert_eq!(activated[0].params["context"], "ctx-1");
        assert_eq!(watchers(rdp), 2);
        // And a second debugger tool on it does not attach again
        set_breakpoint(breakpoint(2, Some("tab-2")), &handler)
            .await
            .unwrap();
        assert_eq!(watchers(rdp), 2);
        assert_eq!(mock.bidi().count("browsingContext.activate"), 1);
        ts::stop(&handler).await;
    }

    /// REV-F11-2: tabs are matched by position only when every web page at
    /// the same position shows the same URL.
    #[test]
    fn positions_match_only_with_the_same_pages() {
        assert!(same_pages(&[
            ("https://a.test/x#top", "https://a.test/x"),
            ("http://gone.invalid/", "about:neterror?e=dnsNotFound"),
            ("about:blank", "about:blank"),
        ]));
        assert!(!same_pages(&[
            ("https://a.test/", "https://b.test/"),
            ("https://b.test/", "https://a.test/"),
        ]));
    }

    /// REV-F11-7: the debugger of a tab the page closed is detached at the
    /// next attach check, even if no debugger tool runs.
    #[tokio::test]
    async fn closed_tabs_lose_their_debugger() {
        // Given tab-1 watched since the session started
        let mock = MockFirefox::start(true).await.unwrap();
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

    /// DBG-01 (Firefox): the reply has the column Firefox binds to and
    /// whether a loaded script has that URL.
    #[tokio::test]
    async fn breakpoint_reply_has_resolved_column() {
        // Given the mock's loaded test.html (breakable at column 10)
        let mock = MockFirefox::start(true).await.unwrap();
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

    /// RDP-06 / DBG-04: step_over reports the new location from the
    /// combined packet, 0-based, with the script URL.
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
        assert_eq!(reply["location"]["line_number"], 7, "RDP line 8, 0-based");
        assert_eq!(reply["location"]["url"], SCRIPT);
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
