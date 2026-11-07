pub mod call_stack;
pub mod evaluate_on_call_frame;
pub(crate) mod frames;
pub mod pause_on_load;
pub mod remove_breakpoint;
pub mod resume;
pub mod search_scripts;
pub mod set_breakpoint;
pub mod step_over;

use crate::handler::cdp_domains::cdp_target::CdpTarget;
use crate::handler::debugging::{self, PauseLocation};
use crate::handler::{DebuggerState, ScriptInfo, extract_from_value};
use cdp_browser_lite::WsResponse;
use std::sync::Arc;
use tokio::sync::Mutex;

/// A script position: Chrome sends integers; strings are accepted too.
fn position(params: &Option<serde_json::Value>, key: &str) -> i32 {
    let value = params.as_ref().and_then(|p| p.get(key));
    let number = value
        .and_then(|v| v.as_i64())
        .and_then(|n| i32::try_from(n).ok());
    let text = value.and_then(|v| v.as_str()).and_then(|s| s.parse().ok());
    number.or(text).unwrap_or(0)
}

/// How Chrome refuses a resume or a step when nothing is paused.
pub(crate) const NOT_PAUSED_REFUSAL: &str = "Can only perform operation while paused";

/// Forgets the tab's kept pause when Chrome refused `error` (a resume's or
/// a step's) as not paused: the pause went without a `Debugger.resumed`,
/// and keeping it would block input tools (ISSUE-83). Returns whether it
/// was that refusal.
pub(crate) async fn forget_refused_pause(error: &str, state: &Arc<Mutex<DebuggerState>>) -> bool {
    let refused = error.contains(NOT_PAUSED_REFUSAL);
    if refused {
        let mut state = state.lock().await;
        state.clear_pause();
        state.paused_frame = None;
    }
    refused
}

/// [`process_debugger_event_in`] for an event of the tab.
#[cfg(test)]
pub(crate) async fn process_debugger_event(
    event: &WsResponse,
    state: &Arc<Mutex<DebuggerState>>,
) -> bool {
    process_debugger_event_in(event, state, None).await
}

/// Processes a single CDP debugger event of the tab (`frame` `None`) or of
/// its frame of another origin attached as session `frame`, updating the
/// shared state accordingly (extracted from the listener loop to enable
/// isolated unit testing). A frame's scripts and pause are recorded under
/// their tab's script ids (`frames::frame_script_id`), and its pauses and
/// resumes announced (`frames::FrameEvents`); events of a frame no longer
/// debugged are ignored. Returns `true` for a pause the tab must not keep
/// (`DebuggerState::skips_pauses`): it is not recorded, and the listener
/// resumes it.
pub(crate) async fn process_debugger_event_in(
    event: &WsResponse,
    state: &Arc<Mutex<DebuggerState>>,
    frame: Option<&str>,
) -> bool {
    let method = event.method.as_deref();
    let mut st = state.lock().await;
    if method == Some("Debugger.paused") && st.skips_pauses() {
        return true;
    }
    let number = match frame.map(|session| st.frame_number(session)) {
        Some(None) => return false,
        Some(number) => number,
        None => None,
    };
    let event = match number {
        Some(number) => &frames::in_frame(event.clone(), number),
        None => event,
    };
    match method {
        Some("Debugger.scriptParsed") => on_script_parsed(&event.params, &mut st, number),
        Some("Runtime.executionContextsCleared") => on_contexts_cleared(&mut st, number, frame),
        Some("Debugger.paused") => on_paused(&event.params, &mut st, frame),
        Some("Debugger.resumed") if st.paused_frame.as_deref() == frame => {
            st.clear_pause();
            st.paused_frame = None;
        }
        _ => {}
    }
    let paused_or_resumed = matches!(method, Some("Debugger.paused" | "Debugger.resumed"));
    if number.is_some() && paused_or_resumed {
        st.frame_events.announce(event.clone());
    }
    false
}

/// The document of the tab (`frame` `None`) or of its frame `number`
/// (session `frame`) was replaced: its scripts are gone, and Chrome may
/// reuse their ids (ISSUE-66). A pause in it (or, for the tab, in any of
/// its frames) went with it: Chrome sends no `Debugger.resumed` then, and
/// a kept pause would block input tools (ISSUE-83).
fn on_contexts_cleared(st: &mut DebuggerState, number: Option<u32>, frame: Option<&str>) {
    st.scripts
        .retain(|id, _| frames::split_script_id(id).0 != number);
    st.pause_on_load_sources
        .retain(|id| frames::split_script_id(id).0 != number);
    if frame.is_none() || st.paused_frame.as_deref() == frame {
        st.clear_pause();
        st.paused_frame = None;
    }
}

/// Records the script a `Debugger.scriptParsed` announces, of frame
/// `number` (`None`: the tab).
fn on_script_parsed(
    params: &Option<serde_json::Value>,
    st: &mut DebuggerState,
    number: Option<u32>,
) {
    let (Some(script_id), Some(hash)) = (
        extract_from_value(params, "scriptId"),
        extract_from_value(params, "hash"),
    ) else {
        return;
    };
    // A script a protocol client created (the server's snippets, the
    // agent's evaluate_js) has no URL and no stack: not the page's, so not
    // kept (ISSUE-66). A page's `eval` has the stack that ran it.
    let url = extract_from_value(params, "url").unwrap_or_default();
    let stack = params.as_ref().and_then(|p| p.get("stackTrace"));
    if url.is_empty() && stack.is_none_or(serde_json::Value::is_null) {
        return;
    }
    let id = number.map_or_else(
        || script_id.to_string(),
        |number| frames::frame_script_id(number, script_id),
    );
    // pause_on_load's own `debugger;`: not the page's (ISSUE-84).
    if url == pause_on_load::PAUSE_ON_LOAD_URL {
        if st.pause_on_load_sources.len() < crate::handler::MAX_PAUSE_ON_LOAD_SOURCES {
            st.pause_on_load_sources.insert(id);
        }
        return;
    }
    let info = ScriptInfo {
        hash: hash.to_string(),
        start_line: position(params, "startLine"),
        start_column: position(params, "startColumn"),
        end_line: position(params, "endLine"),
        url: url.to_string(),
    };
    st.scripts.insert(id, info);
}

/// Records the pause a `Debugger.paused` announces, in `frame` (`None`:
/// the tab).
fn on_paused(params: &Option<serde_json::Value>, st: &mut DebuggerState, frame: Option<&str>) {
    let first = params
        .as_ref()
        .and_then(|p| p.get("callFrames"))
        .and_then(|frames| frames.as_array())
        .and_then(|frames| frames.first())
        .and_then(|first| first.get("callFrameId"))
        .and_then(|id| id.as_str());
    if let Some(call_frame_id) = first {
        st.pauses_recorded += 1;
        st.paused_call_frame_id = Some(call_frame_id.to_string());
        st.paused_at = params.as_ref().and_then(pause_location);
        st.paused_frame = frame.map(str::to_string);
        if let Some(params) = params {
            let (ids, mut stack) = call_stack(params);
            // Stopped at pause_on_load's own `debugger;` (ISSUE-84).
            let injected = st.paused_at.as_ref().and_then(|at| at.script_id.as_ref());
            if injected.is_some_and(|id| st.pause_on_load_sources.contains(id)) {
                stack.reason = "pause_on_load".to_string();
            }
            st.paused_frame_ids = ids;
            st.paused_stack = Some(stack);
        }
    }
}

/// The ids and the stack of a `Debugger.paused` (ISSUE-67), at most
/// `debugging::MAX_STACK_FRAMES` frames, async callers included.
fn call_stack(params: &serde_json::Value) -> (Vec<String>, debugging::CallStack) {
    let frames = params["callFrames"].as_array().cloned().unwrap_or_default();
    let ids = frames
        .iter()
        .take(debugging::MAX_STACK_FRAMES)
        .filter_map(|f| f["callFrameId"].as_str().map(str::to_string))
        .collect();
    let mut room = debugging::MAX_STACK_FRAMES;
    let own = stack_frames(&frames, &mut room, true);
    let mut async_parents = Vec::new();
    let mut parent = params.get("asyncStackTrace");
    while let Some(segment) =
        parent.filter(|_| async_parents.len() < debugging::MAX_ASYNC_CALLERS && room > 0)
    {
        let frames = segment["callFrames"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        async_parents.push(debugging::AsyncSegment {
            description: segment["description"]
                .as_str()
                .unwrap_or("async")
                .to_string(),
            frames: stack_frames(&frames, &mut room, false),
        });
        parent = segment.get("parent");
    }
    let hit = params["hitBreakpoints"]
        .as_array()
        .is_some_and(|hit| !hit.is_empty());
    let native = params["reason"].as_str().unwrap_or("other");
    let stack = debugging::CallStack {
        reason: debugging::pause_reason(if hit { "breakpoint" } else { native }),
        exception: thrown_text(native, params.get("data")),
        truncated: frames.len() > own.len(),
        frames: own,
        async_parents,
    };
    (ids, stack)
}

/// What a pause on an exception or a rejection says was thrown: its
/// description's first line (the rest is its stack), else its value.
fn thrown_text(reason: &str, data: Option<&serde_json::Value>) -> Option<String> {
    if !matches!(reason, "exception" | "promiseRejection") {
        return None;
    }
    let data = data?;
    match data["description"].as_str() {
        Some(description) => description.lines().next().map(str::to_string),
        None => data
            .get("value")
            .map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_string)),
    }
}

/// Up to `room` frames of a CDP stack, taking them from it:
/// `Debugger.CallFrame`s (`located`: their place is in `location`) or
/// `Runtime.CallFrame`s (the place is the frame's own fields).
fn stack_frames(
    frames: &[serde_json::Value],
    room: &mut usize,
    located: bool,
) -> Vec<debugging::StackFrame> {
    let taken: Vec<debugging::StackFrame> = frames
        .iter()
        .take(*room)
        .filter_map(|frame| {
            let at = if located {
                frame.get("location")?
            } else {
                frame
            };
            let number = |key: &str| at[key].as_u64().and_then(|n| u32::try_from(n).ok());
            let url = frame["url"]
                .as_str()
                .filter(|u| !u.is_empty())
                .map(str::to_string);
            Some(debugging::StackFrame {
                function_name: frame["functionName"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
                location: PauseLocation {
                    url,
                    script_id: at["scriptId"].as_str().map(str::to_string),
                    line: number("lineNumber")?,
                    column: number("columnNumber").unwrap_or(0),
                },
                scopes: scope_chain(frame),
            })
        })
        .collect();
    *room = room.saturating_sub(taken.len());
    taken
}

/// The scope chain of a `Debugger.CallFrame` (at most
/// [`debugging::MAX_SCOPES`]); none for a `Runtime.CallFrame`.
fn scope_chain(frame: &serde_json::Value) -> Vec<debugging::Scope> {
    let chain = frame["scopeChain"].as_array().into_iter().flatten();
    chain
        .take(debugging::MAX_SCOPES)
        .map(|scope| debugging::Scope {
            kind: scope["type"].as_str().unwrap_or("unknown").to_string(),
            name: scope["name"]
                .as_str()
                .filter(|n| !n.is_empty())
                .map(str::to_string),
        })
        .collect()
}

/// Where a `Debugger.paused` event stopped (top frame), 0-based as CDP.
pub(crate) fn pause_location(params: &serde_json::Value) -> Option<PauseLocation> {
    let frame = params.get("callFrames")?.as_array()?.first()?;
    let location = frame.get("location")?;
    let number = |key: &str| {
        location
            .get(key)
            .and_then(serde_json::Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
    };
    let url = frame
        .get("url")
        .and_then(serde_json::Value::as_str)
        .filter(|u| !u.is_empty())
        .map(str::to_string);
    let script_id = location
        .get("scriptId")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    Some(PauseLocation {
        url,
        script_id,
        line: number("lineNumber")?,
        column: number("columnNumber").unwrap_or(0),
    })
}

/// `location` with its script's URL from the tab's parsed scripts when the
/// pause did not carry one: Chrome leaves `callFrames[].url` empty (it is
/// deprecated in favour of `location.scriptId`).
pub(crate) fn with_script_url(mut location: PauseLocation, state: &DebuggerState) -> PauseLocation {
    let injected = location.script_id.as_ref();
    if location.url.is_none() && injected.is_some_and(|id| state.pause_on_load_sources.contains(id))
    {
        location.url = Some(pause_on_load::PAUSE_ON_LOAD_URL.to_string());
    }
    if location.url.is_none()
        && let Some(script) = location
            .script_id
            .as_ref()
            .and_then(|id| state.scripts.get(id))
        && !script.url.is_empty()
    {
        location.url = Some(script.url.clone());
    }
    location
}

/// Waits up to `limit` for the next `Debugger.paused` on `events` (see
/// `frames::debugger_events`), then for `state` to record it (past the
/// `recorded` pauses it had when `events` subscribed, see
/// [`pause_recorded`]); its script is named from `state`'s parsed scripts.
pub(crate) async fn wait_for_pause(
    events: &mut (impl tokio_stream::Stream<Item = cdp_browser_lite::CdpResult<WsResponse>> + Unpin),
    (limit, recorded): (std::time::Duration, u64),
    state: &Arc<Mutex<DebuggerState>>,
) -> Option<PauseLocation> {
    let location = next_pause(events, limit).await?;
    recorded_or_warn(state, recorded).await;
    Some(with_script_url(location, &*state.lock().await))
}

/// [`pause_recorded`], logging when the wait hit its limit (the tool
/// answers anyway: the pause it saw is real).
async fn recorded_or_warn(state: &Arc<Mutex<DebuggerState>>, recorded: u64) {
    if !pause_recorded(state, recorded).await {
        tracing::warn!("the debugger listener did not record a pause within {PAUSE_RECORD_WAIT:?}");
    }
}

/// The most a tool waits for the tab's state to record a pause it saw.
const PAUSE_RECORD_WAIT: std::time::Duration = std::time::Duration::from_millis(500);

/// Waits (at most [`PAUSE_RECORD_WAIT`]) for the tab's `state` to record
/// a pause after the `recorded` it had when the tool subscribed: the tool
/// saw the pause on its own subscription, the debugger listener runs
/// apart, and a call right after the tool's answer (call_stack,
/// evaluate_on_call_frame) read the state before the listener recorded
/// the pause, or the previous one (ISSUE-94). A tab that skips its pauses
/// records none: no wait. `false` when the wait hit its limit.
async fn pause_recorded(state: &Arc<Mutex<DebuggerState>>, recorded: u64) -> bool {
    let deadline = tokio::time::Instant::now() + PAUSE_RECORD_WAIT;
    loop {
        {
            let st = state.lock().await;
            if st.pauses_recorded > recorded || st.skips_pauses() {
                return true;
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

/// The next `Debugger.paused` on `events` within `limit`.
async fn next_pause(
    events: &mut (impl tokio_stream::Stream<Item = cdp_browser_lite::CdpResult<WsResponse>> + Unpin),
    limit: std::time::Duration,
) -> Option<PauseLocation> {
    use tokio_stream::StreamExt;
    let pause = async {
        while let Some(event) = events.next().await {
            let Ok(event) = event else { continue };
            if event.method.as_deref() == Some("Debugger.paused") {
                return event.params.as_ref().and_then(pause_location);
            }
        }
        None
    };
    tokio::time::timeout(limit, pause).await.ok().flatten()
}

/// Tells Chrome whether to skip every pause (`Debugger.setSkipAllPauses`)
/// from the tab's state (see `DebuggerState::skips_pauses`), in the tab and
/// its debugged frames. Best effort: an older Chrome without the command
/// keeps pausing.
pub(crate) async fn sync_pauses(target: &CdpTarget, state: &Arc<Mutex<DebuggerState>>) {
    let (settings, frames) = {
        let state = state.lock().await;
        (pause_settings(&state), state.debugged_frames())
    };
    let sessions = std::iter::once(target.clone())
        .chain(frames.into_iter().map(|frame| CdpTarget::tab(frame.tab)));
    for session in sessions {
        for (method, params) in &settings {
            let _ = session.send_raw_command(method, params.clone()).await;
        }
    }
}

/// How deep Chrome records async stacks while the tab is debugged (the
/// async callers `call_stack` lists): 0 otherwise, as they cost the page.
const ASYNC_STACK_DEPTH: u32 = 16;

/// The commands telling a debugger session how the tab pauses: whether it
/// skips every pause, on which exceptions it pauses, and how deep async
/// stacks go (ISSUE-67).
pub(crate) fn pause_settings(state: &DebuggerState) -> [(&'static str, serde_json::Value); 3] {
    let skip = state.skips_pauses();
    let depth = if skip { 0 } else { ASYNC_STACK_DEPTH };
    let exceptions = state.pause_on_exceptions.as_str();
    [
        (
            "Debugger.setSkipAllPauses",
            serde_json::json!({ "skip": skip }),
        ),
        (
            "Debugger.setPauseOnExceptions",
            serde_json::json!({ "state": exceptions }),
        ),
        (
            "Debugger.setAsyncCallStackDepth",
            serde_json::json!({ "maxDepth": depth }),
        ),
    ]
}

/// Keeps the tab's debugger state from its `Debugger` events. A pause with
/// no breakpoint and no `pause_on_load` (the page's own `debugger;`) is
/// resumed at once: `Debugger.setSkipAllPauses` does not survive a
/// cross-process navigation, so it is sent again too.
pub(crate) fn start_debugger_listener(
    target: &crate::handler::cdp_domains::cdp_target::CdpTarget,
    state: Arc<Mutex<DebuggerState>>,
) -> tokio::task::JoinHandle<()> {
    start_debugger_listener_in(target, state, None)
}

/// [`start_debugger_listener`] for the tab (`frame` `None`) or its frame of
/// another origin attached as session `frame` (`target` is that session).
pub(crate) fn start_debugger_listener_in(
    target: &crate::handler::cdp_domains::cdp_target::CdpTarget,
    state: Arc<Mutex<DebuggerState>>,
    frame: Option<String>,
) -> tokio::task::JoinHandle<()> {
    let page = target.clone();
    let process = move |event, state| {
        let page = page.clone();
        let frame = frame.clone();
        async move {
            if process_debugger_event_in(&event, &state, frame.as_deref()).await {
                let skip = serde_json::json!({ "skip": true });
                let _ = page
                    .send_raw_command("Debugger.setSkipAllPauses", skip)
                    .await;
                let _ = page
                    .send_raw_command("Debugger.resume", serde_json::json!({}))
                    .await;
            }
        }
    };
    // Every event of the target, in order, so a navigation's
    // `Runtime.executionContextsCleared` comes before its new scripts.
    let relevant = move |event: WsResponse, state| {
        let wanted = event
            .method
            .as_deref()
            .is_some_and(|m| m.starts_with("Debugger.") || m == "Runtime.executionContextsCleared");
        let processing = wanted.then(|| process(event, state));
        async move {
            if let Some(processing) = processing {
                processing.await;
            }
        }
    };
    crate::handler::cdp_domains::event_pump::spawn_state_listener(target, "", state, relevant)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::handler::cdp_domains::tests::make_event;
    use serde_json::json;

    /// ISSUE-67 (Chrome): a pause keeps its whole stack (every frame's id,
    /// for `frame_index`), why it paused (a breakpoint hit, an exception
    /// with what was thrown) and its async callers; a resume forgets it.
    #[tokio::test]
    async fn pauses_keep_their_stack() {
        let all = crate::handler::debugging::PauseOnExceptions::All;
        let state = Arc::new(Mutex::new(DebuggerState {
            pause_on_exceptions: all,
            ..Default::default()
        }));
        let frame = |id: &str, name: &str, line: u64| json!({"callFrameId": id, "functionName": name, "url": "", "location": {"scriptId": "3", "lineNumber": line, "columnNumber": 2}});
        let paused = json!({"reason": "exception", "data": {"description": "TypeError: x\n    at boom (http://a/:1:1)"},
            "callFrames": [frame("f0", "price", 1), frame("f1", "", 5)],
            "asyncStackTrace": {"description": "setTimeout", "callFrames": [{"functionName": "onclick", "scriptId": "9", "url": "http://a/", "lineNumber": 0, "columnNumber": 0}]}});
        process_debugger_event(&make_event("Debugger.paused", paused), &state).await;
        let st = state.lock().await;
        assert_eq!(st.paused_frame_ids, ["f0", "f1"]);
        let stack = st.paused_stack.clone().unwrap();
        assert_eq!(
            (stack.reason.as_str(), stack.exception.as_deref()),
            ("exception", Some("TypeError: x"))
        );
        assert_eq!(stack.frames.len(), 2);
        assert_eq!(
            (
                stack.frames[1].function_name.as_str(),
                stack.frames[1].location.line
            ),
            ("", 5)
        );
        assert_eq!(stack.async_parents[0].description, "setTimeout");
        drop(st);
        let hit = json!({"reason": "other", "hitBreakpoints": ["1:1:0:http://a/app.js"], "callFrames": [frame("f0", "price", 1)]});
        process_debugger_event(&make_event("Debugger.paused", hit), &state).await;
        assert_eq!(
            state.lock().await.paused_stack.as_ref().unwrap().reason,
            "breakpoint"
        );
        process_debugger_event(&make_event("Debugger.resumed", json!({})), &state).await;
        let st = state.lock().await;
        assert!(st.paused_stack.is_none() && st.paused_frame_ids.is_empty());
    }

    /// ISSUE-67: pausing on exceptions keeps the tab's pauses (they were
    /// skipped without a breakpoint), and each session is told the mode and
    /// records async stacks only while the tab is debugged.
    #[test]
    fn pausing_on_exceptions_keeps_pauses() {
        let mut state = DebuggerState::default();
        assert!(state.skips_pauses());
        let idle = pause_settings(&state);
        assert_eq!(idle[1].1, json!({"state": "none"}));
        assert_eq!(idle[2].1, json!({"maxDepth": 0}));
        state.pause_on_exceptions = crate::handler::debugging::PauseOnExceptions::Uncaught;
        assert!(!state.skips_pauses());
        let settings = pause_settings(&state);
        assert_eq!(
            settings.clone().map(|(m, _)| m),
            [
                "Debugger.setSkipAllPauses",
                "Debugger.setPauseOnExceptions",
                "Debugger.setAsyncCallStackDepth"
            ]
        );
        assert_eq!(
            (
                settings[0].1.clone(),
                settings[1].1.clone(),
                settings[2].1.clone()
            ),
            (
                json!({"skip": false}),
                json!({"state": "uncaught"}),
                json!({"maxDepth": 16})
            )
        );
    }

    #[tokio::test]
    async fn test_script_parsed_inserts_script_info() {
        let state = Arc::new(Mutex::new(DebuggerState::default()));
        let event = make_event(
            "Debugger.scriptParsed",
            json!({
                "scriptId": "42",
                "hash": "abc123",
                "url": "http://a/app.js",
                "startLine": "10",
                "startColumn": "5"
            }),
        );

        process_debugger_event(&event, &state).await;

        let st = state.lock().await;
        assert_eq!(st.scripts.len(), 1);
        let script = st.scripts.get("42").unwrap();
        assert_eq!(script.hash, "abc123");
        assert_eq!(script.start_line, 10);
        assert_eq!(script.start_column, 5);
    }

    #[tokio::test]
    async fn test_script_parsed_defaults_line_column() {
        let state = Arc::new(Mutex::new(DebuggerState::default()));
        let event = make_event(
            "Debugger.scriptParsed",
            json!({
                "scriptId": "1",
                "hash": "def456",
                "url": "http://a/app.js"
            }),
        );

        process_debugger_event(&event, &state).await;

        let st = state.lock().await;
        let script = st.scripts.get("1").unwrap();
        assert_eq!(script.start_line, 0);
        assert_eq!(script.start_column, 0);
    }

    #[tokio::test]
    async fn test_script_parsed_missing_hash_ignored() {
        let state = Arc::new(Mutex::new(DebuggerState::default()));
        let event = make_event(
            "Debugger.scriptParsed",
            json!({
                "scriptId": "99"
            }),
        );

        process_debugger_event(&event, &state).await;

        let st = state.lock().await;
        assert!(st.scripts.is_empty());
    }

    #[tokio::test]
    async fn test_script_parsed_missing_script_id_ignored() {
        let state = Arc::new(Mutex::new(DebuggerState::default()));
        let event = make_event(
            "Debugger.scriptParsed",
            json!({
                "hash": "abc"
            }),
        );

        process_debugger_event(&event, &state).await;

        let st = state.lock().await;
        assert!(st.scripts.is_empty());
    }

    #[tokio::test]
    async fn test_paused_stores_call_frame_id() {
        let state = with_breakpoint();
        let event = make_event(
            "Debugger.paused",
            json!({
                "callFrames": [
                    { "callFrameId": "frame-0", "functionName": "main" },
                    { "callFrameId": "frame-1", "functionName": "helper" }
                ]
            }),
        );

        process_debugger_event(&event, &state).await;

        let st = state.lock().await;
        assert_eq!(st.paused_call_frame_id.as_deref(), Some("frame-0"));
    }

    #[tokio::test]
    async fn test_paused_empty_call_frames_ignored() {
        let state = Arc::new(Mutex::new(DebuggerState::default()));
        let event = make_event(
            "Debugger.paused",
            json!({
                "callFrames": []
            }),
        );

        process_debugger_event(&event, &state).await;

        let st = state.lock().await;
        assert!(st.paused_call_frame_id.is_none());
    }

    #[tokio::test]
    async fn test_paused_missing_call_frames_ignored() {
        let state = Arc::new(Mutex::new(DebuggerState::default()));
        let event = make_event("Debugger.paused", json!({}));

        process_debugger_event(&event, &state).await;

        let st = state.lock().await;
        assert!(st.paused_call_frame_id.is_none());
    }

    #[tokio::test]
    async fn test_unknown_method_ignored() {
        let state = Arc::new(Mutex::new(DebuggerState::default()));
        let event = make_event("Debugger.resumed", json!({}));

        process_debugger_event(&event, &state).await;

        let st = state.lock().await;
        assert!(st.scripts.is_empty());
        assert!(st.paused_call_frame_id.is_none());
    }

    #[tokio::test]
    async fn test_event_without_method_ignored() {
        let state = Arc::new(Mutex::new(DebuggerState::default()));
        let event = WsResponse {
            method: None,
            params: Some(json!({"scriptId": "1", "hash": "abc"})),
            ..Default::default()
        };

        process_debugger_event(&event, &state).await;

        let st = state.lock().await;
        assert!(st.scripts.is_empty());
    }

    #[tokio::test]
    async fn test_multiple_script_parsed_events() {
        let state = Arc::new(Mutex::new(DebuggerState::default()));

        for i in 0..5 {
            let event = make_event(
                "Debugger.scriptParsed",
                json!({
                    "scriptId": format!("script-{}", i),
                    "hash": format!("hash-{}", i),
                    "url": "http://a/app.js",
                    "startLine": i.to_string(),
                    "startColumn": "0"
                }),
            );
            process_debugger_event(&event, &state).await;
        }

        let st = state.lock().await;
        assert_eq!(st.scripts.len(), 5);
        for i in 0..5 {
            let key = format!("script-{}", i);
            let script = st.scripts.get(&key).unwrap();
            assert_eq!(script.hash, format!("hash-{}", i));
            assert_eq!(script.start_line, i);
        }
    }

    #[tokio::test]
    async fn test_paused_overwrites_previous_frame_id() {
        let state = with_breakpoint();

        let event1 = make_event(
            "Debugger.paused",
            json!({
                "callFrames": [{ "callFrameId": "old-frame" }]
            }),
        );
        process_debugger_event(&event1, &state).await;
        assert_eq!(
            state.lock().await.paused_call_frame_id.as_deref(),
            Some("old-frame")
        );

        let event2 = make_event(
            "Debugger.paused",
            json!({
                "callFrames": [{ "callFrameId": "new-frame" }]
            }),
        );
        process_debugger_event(&event2, &state).await;
        assert_eq!(
            state.lock().await.paused_call_frame_id.as_deref(),
            Some("new-frame")
        );
    }

    /// ISSUE-66 (Chrome): the scripts the server (or the agent's
    /// evaluate_js) creates over CDP, with no URL and no stack, are not
    /// kept: search_scripts neither counts nor searches them. A page's own
    /// `eval` (no URL, but a stack) and a script with a URL are kept.
    #[tokio::test]
    async fn the_servers_own_scripts_are_not_kept() {
        // Given a snippet, a page eval and a script file
        let state = Arc::new(Mutex::new(DebuggerState::default()));
        let stack = json!({"callFrames": [{"functionName": "onclick", "scriptId": "3", "url": "http://a/", "lineNumber": 0, "columnNumber": 0}]});
        for parsed in [
            json!({"scriptId": "1", "hash": "h1", "url": ""}),
            json!({"scriptId": "2", "hash": "h2", "url": "", "stackTrace": stack}),
            json!({"scriptId": "3", "hash": "h3", "url": "http://a/app.js"}),
        ] {
            // When Chrome announces them
            process_debugger_event(&make_event("Debugger.scriptParsed", parsed), &state).await;
        }
        // Then only the page's scripts are kept
        let st = state.lock().await;
        let mut kept: Vec<&String> = st.scripts.keys().collect();
        kept.sort();
        assert_eq!(kept, ["2", "3"]);
    }

    /// ISSUE-66 (Chrome): a navigation clears the scripts of the document
    /// it replaced: after a change of process Chrome reuses their ids, so a
    /// kept one named another script (the server's snippet, found by
    /// search_scripts), and the count held the first page's scripts. The
    /// scripts of a frame of another origin stay (its session clears them).
    #[tokio::test]
    async fn a_navigation_clears_the_documents_scripts() {
        // Given a script of the page and one of frame 2
        let state = Arc::new(Mutex::new(DebuggerState::default()));
        let parsed = json!({"scriptId": "6", "hash": "h", "url": "chrome://new-tab-page/app.js"});
        process_debugger_event(&make_event("Debugger.scriptParsed", parsed), &state).await;
        let framed = ScriptInfo {
            hash: "h".into(),
            start_line: 0,
            start_column: 0,
            end_line: 0,
            url: "http://b/inner.js".into(),
        };
        state.lock().await.scripts.insert("2.7".into(), framed);
        // When the page's contexts are cleared (it navigated)
        let cleared = make_event("Runtime.executionContextsCleared", json!({}));
        process_debugger_event(&cleared, &state).await;
        // Then only the frame's script is left
        let st = state.lock().await;
        let kept: Vec<&String> = st.scripts.keys().collect();
        assert_eq!(kept, ["2.7"]);
    }

    /// ISSUE-83 (Chrome): a navigation while paused forgets the pause.
    /// Chrome tears the paused document down without `Debugger.resumed`, so
    /// the kept pause made input tools refuse to run and call_stack show
    /// the old stack for good.
    #[tokio::test]
    async fn a_navigation_while_paused_forgets_the_pause() {
        let frame = json!({"callFrameId": "f0", "functionName": "price", "url": "", "location": {"scriptId": "3", "lineNumber": 1, "columnNumber": 2}});
        let paused = json!({"reason": "other", "callFrames": [frame]});
        // Given a pause of the tab (with a breakpoint, so pauses are kept)
        let state = Arc::new(Mutex::new(DebuggerState::default()));
        let breakpoint = frames::ChromeBreakpoint::on_tab("b");
        state.lock().await.breakpoints.add(breakpoint);
        process_debugger_event(&make_event("Debugger.paused", paused), &state).await;
        assert!(state.lock().await.paused_stack.is_some());
        // When its contexts are cleared (it navigated), with no resume
        let cleared = make_event("Runtime.executionContextsCleared", json!({}));
        process_debugger_event(&cleared, &state).await;
        // Then the pause is forgotten
        let st = state.lock().await;
        assert!(st.paused_call_frame_id.is_none() && st.paused_stack.is_none());
        assert!(st.paused_at.is_none() && st.paused_frame_ids.is_empty());
    }

    /// ISSUE-84 (Chrome): the `debugger;` script pause_on_load injects is
    /// not one of the page's scripts (search_scripts leaves it out), and a
    /// pause in it says so: its URL is pause_on_load's and its reason
    /// `pause_on_load` (it was an `(anonymous)` frame without a URL).
    #[tokio::test]
    async fn the_pause_on_load_script_is_named() {
        use super::pause_on_load::PAUSE_ON_LOAD_URL;
        // Given a pending pause_on_load whose script was parsed as script 4
        let state = Arc::new(Mutex::new(DebuggerState {
            pause_on_load_script: Some("1".into()),
            ..Default::default()
        }));
        let parsed = json!({"scriptId": "4", "hash": "h", "url": PAUSE_ON_LOAD_URL});
        process_debugger_event(&make_event("Debugger.scriptParsed", parsed), &state).await;
        // When the page pauses in it
        let frame = json!({"callFrameId": "f0", "functionName": "", "url": "", "location": {"scriptId": "4", "lineNumber": 0, "columnNumber": 0}});
        let paused = json!({"reason": "other", "callFrames": [frame]});
        process_debugger_event(&make_event("Debugger.paused", paused), &state).await;
        // Then it is not a page script, and the pause is named as its own
        let st = state.lock().await;
        assert!(st.scripts.get("4").is_none());
        assert_eq!(st.paused_stack.as_ref().unwrap().reason, "pause_on_load");
        let at = with_script_url(st.paused_at.clone().unwrap(), &st);
        assert_eq!(at.url.as_deref(), Some(PAUSE_ON_LOAD_URL));
        drop(st);
        // And a new document forgets its id (Chrome reuses ids)
        let cleared = make_event("Runtime.executionContextsCleared", json!({}));
        process_debugger_event(&cleared, &state).await;
        assert!(state.lock().await.pause_on_load_sources.is_empty());
    }

    /// ISSUE-94: a tool that saw the next pause on its own subscription
    /// answers once the tab's state recorded a new pause (the listener runs
    /// apart: a call_stack right after a step said "not paused"), not on
    /// the previous pause still held at the same place.
    #[tokio::test]
    async fn the_next_pause_is_recorded_before_answering() {
        let state = Arc::new(Mutex::new(DebuggerState::default()));
        state
            .lock()
            .await
            .breakpoints
            .add(frames::ChromeBreakpoint::on_tab("b"));
        let frame = json!({"callFrameId": "f9", "url": "", "location": {"scriptId": "3", "lineNumber": 5, "columnNumber": 2}});
        let paused = make_event("Debugger.paused", json!({"callFrames": [frame]}));
        // Given a pause the state already holds (Chrome repeats call frame
        // ids, and a step can stop where it was): the tool subscribed then
        process_debugger_event(&paused, &state).await;
        let recorded = state.lock().await.pauses_recorded;
        // And the listener records the next pause a little later
        let later = state.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            process_debugger_event(&paused, &later).await;
        });
        // When the tool waits for it
        let started = std::time::Instant::now();
        let done = pause_recorded(&state, recorded).await;
        // Then it waited for the new one, not the one held, well before the limit
        assert!(done);
        assert!(started.elapsed() >= std::time::Duration::from_millis(50));
        assert!(started.elapsed() < PAUSE_RECORD_WAIT);
        assert_eq!(state.lock().await.pauses_recorded, recorded + 1);
    }

    /// A pause the state never records ends the wait at its limit, saying
    /// so; a tab that skips its pauses records none, so it is not waited for.
    #[tokio::test]
    async fn a_pause_never_recorded_is_waited_for_only_up_to_the_limit() {
        let state = Arc::new(Mutex::new(DebuggerState::default()));
        state
            .lock()
            .await
            .breakpoints
            .add(frames::ChromeBreakpoint::on_tab("b"));
        let started = std::time::Instant::now();
        assert!(!pause_recorded(&state, 0).await);
        assert!(started.elapsed() >= PAUSE_RECORD_WAIT);
        let skipping = Arc::new(Mutex::new(DebuggerState::default()));
        assert!(skipping.lock().await.skips_pauses());
        let started = std::time::Instant::now();
        assert!(pause_recorded(&skipping, 0).await);
        assert!(started.elapsed() < std::time::Duration::from_millis(50));
    }

    /// ISSUE-100: a paused frame lists its scope chain (type and name), as
    /// evaluate_on_call_frame's `scope` numbers it; async callers have none.
    #[test]
    fn paused_frames_list_their_scopes() {
        let chain = json!([{"type": "local", "object": {}}, {"type": "closure", "name": "f", "object": {}}, {"type": "global", "object": {}}]);
        let params = json!({"reason": "other", "callFrames": [{"callFrameId": "c0", "functionName": "g", "url": "",
            "location": {"scriptId": "3", "lineNumber": 1, "columnNumber": 0}, "scopeChain": chain}]});
        let (_, stack) = call_stack(&params);
        let scopes = &stack.frames[0].scopes;
        let kinds: Vec<&str> = scopes.iter().map(|s| s.kind.as_str()).collect();
        assert_eq!(kinds, ["local", "closure", "global"]);
        assert_eq!(scopes[1].name.as_deref(), Some("f"));
        let listed = stack.to_json();
        assert_eq!(
            listed["frames"][0]["scopes"][1],
            json!({"index": 1, "type": "closure", "name": "f"})
        );
    }

    /// FIX-01: Script start positions are read from integers.
    #[tokio::test]
    async fn script_positions_read_from_integers() {
        // Given Debugger.scriptParsed with numeric positions, as Chrome sends them
        let state = Arc::new(Mutex::new(DebuggerState::default()));
        let event = make_event(
            "Debugger.scriptParsed",
            serde_json::json!({"scriptId": "7", "hash": "h", "url": "http://a/app.js", "startLine": 12, "startColumn": 4}),
        );
        // When it is processed
        process_debugger_event(&event, &state).await;
        // Then the cached positions are the numbers
        let st = state.lock().await;
        let script = st.scripts.get("7").unwrap();
        assert_eq!((script.start_line, script.start_column), (12, 4));
    }

    /// A tab state with one breakpoint, so its pauses are kept.
    pub(crate) fn with_breakpoint() -> Arc<Mutex<DebuggerState>> {
        let mut state = DebuggerState::default();
        state.breakpoints.add(
            crate::handler::cdp_domains::debugger::frames::ChromeBreakpoint::on_tab(
                "1:3:0:http://a/app.js",
            ),
        );
        Arc::new(Mutex::new(state))
    }

    /// AUD-EV-07: without a breakpoint or `pause_on_load`, a pause (the
    /// page's own `debugger;`) is not kept: the listener resumes it.
    #[tokio::test]
    async fn pauses_without_breakpoints_are_resumed() {
        let paused = || {
            make_event(
                "Debugger.paused",
                json!({"callFrames": [{"callFrameId": "F1"}]}),
            )
        };
        let idle = Arc::new(Mutex::new(DebuggerState::default()));
        assert!(process_debugger_event(&paused(), &idle).await);
        assert!(idle.lock().await.paused_call_frame_id.is_none());
        let debugging = with_breakpoint();
        assert!(!process_debugger_event(&paused(), &debugging).await);
        assert!(debugging.lock().await.paused_call_frame_id.is_some());
    }

    /// ISSUE-10: Chrome leaves a pause's `callFrames[].url` empty (it is
    /// deprecated): the location keeps the script id and names the script's
    /// URL from the tab's parsed scripts.
    #[tokio::test]
    async fn pause_names_its_script_from_parsed_scripts() {
        // Given script 35 parsed from http://a/cart.js, and a pause in it without a URL
        let state = with_breakpoint();
        let parsed = json!({"scriptId": "35", "hash": "h", "url": "http://a/cart.js"});
        process_debugger_event(&make_event("Debugger.scriptParsed", parsed), &state).await;
        let frame = json!({"callFrameId": "f1", "url": "", "location": {"scriptId": "35", "lineNumber": 3, "columnNumber": 2}});
        let location = pause_location(&json!({"callFrames": [frame]})).unwrap();
        // When it is named from the tab's scripts
        let named = with_script_url(location, &*state.lock().await);
        // Then it has the URL and the script id
        assert_eq!(
            named.to_json(),
            json!({"url": "http://a/cart.js", "script_id": "35", "line_number": 3, "column_number": 2})
        );
    }

    /// ISSUE-17: the kept pause records where it stopped; resuming clears it.
    #[tokio::test]
    async fn pause_records_its_location() {
        // Given a tab with a breakpoint
        let state = with_breakpoint();
        // When it pauses
        let frame = json!({"callFrameId": "f1", "url": "", "location": {"scriptId": "35", "lineNumber": 3, "columnNumber": 2}});
        let paused = json!({"reason": "other", "callFrames": [frame]});
        process_debugger_event(&make_event("Debugger.paused", paused), &state).await;
        // Then the location is recorded
        let at = state.lock().await.paused_at.clone().unwrap();
        assert_eq!(
            (at.script_id.as_deref(), at.line, at.column),
            (Some("35"), 3, 2)
        );
        // And resuming clears it
        process_debugger_event(&make_event("Debugger.resumed", json!({})), &state).await;
        assert!(state.lock().await.paused_at.is_none());
    }

    /// FIX-02: Resuming clears the paused call frame.
    #[tokio::test]
    async fn resume_clears_paused_frame() {
        // Given a pause followed by Debugger.resumed
        let state = with_breakpoint();
        let paused = serde_json::json!({"callFrames": [{"callFrameId": "F1"}]});
        process_debugger_event(&make_event("Debugger.paused", paused), &state).await;
        assert!(state.lock().await.paused_call_frame_id.is_some());
        process_debugger_event(
            &make_event("Debugger.resumed", serde_json::json!({})),
            &state,
        )
        .await;
        // Then no paused frame remains
        assert!(state.lock().await.paused_call_frame_id.is_none());
    }
}
