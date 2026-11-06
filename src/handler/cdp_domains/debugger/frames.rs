//! The debugger of a tab's frames of another origin on Chrome (#36). Such
//! a frame runs in another process, with a target and a session of its own
//! (`frame_watch` attaches it), so its scripts, breakpoints and pauses are
//! only known there. Its scripts join the tab's under ids of the form
//! `2.17` (frame 2, Chrome's script 17): each process numbers its scripts
//! from 1, so plain ids would collide with the page's. Breakpoints by URL
//! are set in every frame, also in frames that attach later; a pause
//! notes its frame, where `resume`, `step_over` and
//! `evaluate_on_call_frame` then go, and is announced to whoever waits on
//! the tab ([`FrameEvents`]), also for frames that attached meanwhile.

use crate::handler::DebuggerState;
use crate::handler::cdp_domains::cdp_target::CdpTarget;
use cdp_browser_lite::{CdpResult, Tab, WsResponse};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::{Stream, StreamExt};

/// How many frame pauses and resumes wait for a slow reader.
const FRAME_EVENTS: usize = 16;

/// The pauses and resumes of a tab's debugged frames, with their tab's
/// script ids ([`in_frame`]), for the calls waiting on the tab (see
/// [`debugger_events`]).
pub(crate) struct FrameEvents(broadcast::Sender<WsResponse>);

impl Default for FrameEvents {
    fn default() -> Self {
        Self(broadcast::channel(FRAME_EVENTS).0)
    }
}

impl FrameEvents {
    /// Announces `event` (nobody waiting is fine).
    pub(crate) fn announce(&self, event: WsResponse) {
        let _ = self.0.send(event);
    }

    /// The frame events from now on.
    pub(crate) fn subscribe(&self) -> broadcast::Receiver<WsResponse> {
        self.0.subscribe()
    }
}

/// A frame of another origin whose debugger feeds its tab's state.
#[derive(Clone)]
pub(crate) struct DebuggedFrame {
    /// The number that prefixes its script ids.
    pub(crate) number: u32,
    /// Its target id: the entry goes when that target detaches.
    pub(crate) target_id: String,
    /// The session the server attached to it.
    pub(crate) tab: Tab,
}

/// A breakpoint on Chrome: its id in each session it was set in, and for
/// one set by URL or hash the command that sets it, sent again to each
/// frame that attaches later.
#[derive(Clone, Debug, Default)]
pub(crate) struct ChromeBreakpoint {
    /// `(frame session, breakpoint id)`; `None` for the tab's session.
    pub(crate) natives: Vec<(Option<String>, String)>,
    /// The `Debugger.setBreakpointByUrl` params, when set by URL or hash.
    pub(crate) by_url: Option<Value>,
}

impl ChromeBreakpoint {
    /// A breakpoint set in the tab's session only, as `native`.
    #[cfg(test)]
    pub(crate) fn on_tab(native: impl Into<String>) -> Self {
        Self {
            natives: vec![(None, native.into())],
            by_url: None,
        }
    }
}

/// The id script `raw` of frame `number` has among its tab's scripts.
pub(crate) fn frame_script_id(number: u32, raw: &str) -> String {
    format!("{number}.{raw}")
}

/// The frame number (`None`: the tab's script) and Chrome's id of `id`.
pub(crate) fn split_script_id(id: &str) -> (Option<u32>, &str) {
    match id.split_once('.') {
        Some((number, raw)) => match number.parse() {
            Ok(number) => (Some(number), raw),
            Err(_) => (None, id),
        },
        None => (None, id),
    }
}

/// `event` of frame `number` with its call frames' script ids as its tab
/// names them ([`frame_script_id`]).
pub(crate) fn in_frame(mut event: WsResponse, number: u32) -> WsResponse {
    let frames = event
        .params
        .as_mut()
        .and_then(|p| p.get_mut("callFrames"))
        .and_then(Value::as_array_mut);
    for frame in frames.into_iter().flatten() {
        let script = frame.pointer_mut("/location/scriptId");
        if let Some(id) = script.as_ref().and_then(|id| id.as_str()) {
            let named = frame_script_id(number, id);
            if let Some(script) = script {
                *script = json!(named);
            }
        }
    }
    event
}

/// The `Debugger` events of the tab of `target`, and the pauses and
/// resumes of its debugged frames (`frames`, from [`FrameEvents`]).
pub(crate) fn debugger_events(
    target: &CdpTarget,
    frames: broadcast::Receiver<WsResponse>,
) -> impl Stream<Item = CdpResult<WsResponse>> + Unpin + Send + use<> {
    let frames = BroadcastStream::new(frames).filter_map(|event| event.ok().map(Ok));
    target.on_domain("Debugger").merge(frames)
}

/// Where the pause of `target`'s tab is (the tab, or a frame), how many
/// pauses its state had recorded (`DebuggerState::pauses_recorded`, for
/// `wait_for_pause`), and its `Debugger` events from now on (see
/// [`debugger_events`]), subscribed before a command that ends the pause:
/// the next one can come before the reply.
pub(crate) async fn paused_with_events(
    target: &CdpTarget,
    state: &Arc<Mutex<DebuggerState>>,
) -> (
    CdpTarget,
    u64,
    impl Stream<Item = CdpResult<WsResponse>> + Unpin + Send + use<>,
) {
    let state = state.lock().await;
    let events = debugger_events(target, state.frame_events.subscribe());
    (state.paused_target(target), state.pauses_recorded, events)
}

impl DebuggerState {
    /// Registers the frame of `target_id`, attached as `tab`; its number.
    pub(crate) fn register_frame(&mut self, target_id: &str, tab: Tab) -> u32 {
        self.frame_count += 1;
        let frame = DebuggedFrame {
            number: self.frame_count,
            target_id: target_id.to_string(),
            tab,
        };
        let session = frame.tab.session_id().to_string();
        self.frames.insert(session, frame);
        self.frame_count
    }

    /// Forgets the frame of `target_id` (detached): its scripts, its
    /// breakpoint ids, and a pause in it.
    pub(crate) fn forget_frame_target(&mut self, target_id: &str) {
        let gone: Vec<(String, u32)> = self
            .frames
            .iter()
            .filter(|(_, frame)| frame.target_id == target_id)
            .map(|(session, frame)| (session.clone(), frame.number))
            .collect();
        for (session, number) in gone {
            self.frames.remove(&session);
            self.scripts
                .retain(|id, _| split_script_id(id).0 != Some(number));
            for (_, breakpoint) in self.breakpoints.iter_mut() {
                breakpoint
                    .natives
                    .retain(|(in_session, _)| in_session.as_deref() != Some(&session));
            }
            if self.paused_frame.as_deref() == Some(&session) {
                self.paused_frame = None;
                self.clear_pause();
            }
        }
    }

    /// The number of the frame attached as `session`, if it is debugged.
    pub(crate) fn frame_number(&self, session: &str) -> Option<u32> {
        self.frames.get(session).map(|frame| frame.number)
    }

    /// The debugged frames.
    pub(crate) fn debugged_frames(&self) -> Vec<DebuggedFrame> {
        self.frames.values().cloned().collect()
    }

    /// Where commands about script `id` go, with Chrome's id there: the
    /// tab's `target`, or its frame; `None` when that frame is gone.
    pub(crate) fn script_target(
        &self,
        id: &str,
        target: &CdpTarget,
    ) -> Option<(CdpTarget, String)> {
        match split_script_id(id) {
            (None, raw) => Some((target.clone(), raw.to_string())),
            (Some(number), raw) => self
                .frames
                .values()
                .find(|frame| frame.number == number)
                .map(|frame| (CdpTarget::tab(frame.tab.clone()), raw.to_string())),
        }
    }

    /// Where the kept pause is: the tab's `target`, or the frame paused.
    pub(crate) fn paused_target(&self, target: &CdpTarget) -> CdpTarget {
        self.paused_frame
            .as_ref()
            .and_then(|session| self.frames.get(session))
            .map_or_else(|| target.clone(), |frame| CdpTarget::tab(frame.tab.clone()))
    }
}

/// Readies the debugger of frame `frame` (already registered in `state`)
/// before it runs: it pauses as its tab does (see `pause_settings`), and
/// the breakpoints by URL are set there too.
pub(crate) async fn prepare_frame(frame: &Tab, state: &Arc<Mutex<DebuggerState>>) {
    let (settings, by_url) = {
        let mut state = state.lock().await;
        let settings = super::pause_settings(&state);
        let by_url: Vec<(String, Value)> = state
            .breakpoints
            .iter_mut()
            .filter_map(|(id, breakpoint)| Some((id.clone(), breakpoint.by_url.clone()?)))
            .collect();
        (settings, by_url)
    };
    for (method, params) in settings {
        let _ = frame.send_raw_command(method, params).await;
    }
    for (id, params) in by_url {
        let Some(native) = set_by_url(frame, params).await else {
            continue;
        };
        let session = frame.session_id().to_string();
        if let Some(breakpoint) = state.lock().await.breakpoints.get_mut(&id) {
            breakpoint.natives.push((Some(session), native));
        }
    }
}

/// `Debugger.setBreakpointByUrl` with `params` in `session`: the
/// breakpoint id, if it set one.
async fn set_by_url(session: &Tab, params: Value) -> Option<String> {
    let reply = session
        .send_raw_command("Debugger.setBreakpointByUrl", params)
        .await
        .ok()?;
    breakpoint_id(&reply.result.unwrap_or_default())
}

/// The `breakpointId` of a breakpoint command's reply.
pub(crate) fn breakpoint_id(reply: &Value) -> Option<String> {
    reply["breakpointId"]
        .as_str()
        .filter(|id| !id.is_empty())
        .map(str::to_string)
}

/// Removes `breakpoint` from the frames it was set in (best effort: a
/// frame may be gone); the tab's id, if it has one, is left to the caller.
pub(crate) async fn remove_from_frames(breakpoint: &ChromeBreakpoint, frames: &[DebuggedFrame]) {
    for (session, native) in &breakpoint.natives {
        let frame = session
            .as_ref()
            .and_then(|session| frames.iter().find(|f| f.tab.session_id() == session));
        if let Some(frame) = frame {
            let params = json!({ "breakpointId": native });
            let _ = frame
                .tab
                .send_raw_command("Debugger.removeBreakpoint", params)
                .await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ISSUE-36: frame script ids carry the frame's number; the page's
    /// stay as Chrome sends them.
    #[test]
    fn frame_script_ids_round_trip() {
        assert_eq!(frame_script_id(2, "17"), "2.17");
        assert_eq!(split_script_id("2.17"), (Some(2), "17"));
        assert_eq!(split_script_id("17"), (None, "17"));
        assert_eq!(split_script_id("x.17"), (None, "x.17"));
    }

    /// ISSUE-36: a frame's pause names its scripts as the tab does.
    #[test]
    fn frame_events_use_the_tab_script_ids() {
        let frame = json!({"callFrameId": "c", "location": {"scriptId": "4", "lineNumber": 1}});
        let event = WsResponse {
            method: Some("Debugger.paused".into()),
            params: Some(json!({"callFrames": [frame.clone(), frame]})),
            ..Default::default()
        };
        let named = in_frame(event, 3);
        let params = named.params.unwrap();
        assert_eq!(params["callFrames"][0]["location"]["scriptId"], "3.4");
        assert_eq!(params["callFrames"][1]["location"]["scriptId"], "3.4");
    }

    use crate::handler::cdp_domains::debugger::process_debugger_event_in;
    use crate::handler::cdp_domains::tests::{
        call, make_event, mock_handler, reply_text, spawn_recording_mock,
    };
    use cdp_browser_lite::test_support::mock_devtools::MockDevTools;

    /// A handler on the mock whose tab debugs one frame of another origin
    /// (target `T-frame`, number 1); the frame's session id.
    async fn handler_with_frame() -> (&'static MockDevTools, crate::handler::McpHandler, String) {
        let mock = spawn_recording_mock().await;
        let handler = mock_handler(mock.http_port);
        let address = format!("127.0.0.1:{}", mock.http_port);
        let browser =
            cdp_browser_lite::BrowserClient::connect(&address, std::time::Duration::from_secs(2))
                .await
                .unwrap();
        let frame = browser.attach("T-frame").await.unwrap();
        let session = frame.session_id().to_string();
        let state = handler
            .session(None)
            .await
            .unwrap()
            .debugger_state(None)
            .unwrap();
        assert_eq!(state.lock().await.register_frame("T-frame", frame), 1);
        (mock, handler, session)
    }

    /// The session ids the mock received `method` on.
    fn sessions_of(mock: &MockDevTools, method: &str) -> Vec<Option<String>> {
        mock.commands_named(method)
            .into_iter()
            .map(|c| c.session_id)
            .collect()
    }

    /// ISSUE-36: a frame's scripts and pause are recorded under its
    /// number, the pause is announced to waiting calls, and only the
    /// frame's resume ends it; a frame no longer debugged is ignored.
    #[tokio::test]
    async fn frame_events_feed_the_tab_state() {
        // Given a tab debugging frame 1, with a breakpoint (pauses kept)
        let (_mock, handler, frame) = handler_with_frame().await;
        let state = handler
            .session(None)
            .await
            .unwrap()
            .debugger_state(None)
            .unwrap();
        state
            .lock()
            .await
            .breakpoints
            .add(ChromeBreakpoint::on_tab("b"));
        let mut announced = state.lock().await.frame_events.subscribe();
        // When the frame parses script 7 and pauses in it
        let parsed = json!({"scriptId": "7", "hash": "h", "url": "http://b/inner.js"});
        process_debugger_event_in(
            &make_event("Debugger.scriptParsed", parsed),
            &state,
            Some(&frame),
        )
        .await;
        let call_frame =
            json!({"callFrameId": "c1", "url": "", "location": {"scriptId": "7", "lineNumber": 1}});
        let paused = make_event("Debugger.paused", json!({"callFrames": [call_frame]}));
        process_debugger_event_in(&paused, &state, Some(&frame)).await;
        // Then the script and the pause carry the frame's number
        {
            let state = state.lock().await;
            assert_eq!(state.scripts["1.7"].url, "http://b/inner.js");
            assert_eq!(state.paused_frame.as_deref(), Some(frame.as_str()));
            assert_eq!(
                state.paused_at.as_ref().unwrap().script_id.as_deref(),
                Some("1.7")
            );
        }
        let event = announced.try_recv().unwrap();
        assert_eq!(
            event.params.unwrap()["callFrames"][0]["location"]["scriptId"],
            "1.7"
        );
        // And the tab resuming does not end it; the frame resuming does
        let resumed = make_event("Debugger.resumed", json!({}));
        process_debugger_event_in(&resumed, &state, None).await;
        assert!(state.lock().await.paused_call_frame_id.is_some());
        process_debugger_event_in(&resumed, &state, Some(&frame)).await;
        assert!(state.lock().await.paused_call_frame_id.is_none());
    }

    /// ISSUE-83: a frame's document going (its contexts cleared) forgets a
    /// pause of that frame only; the tab's going forgets any pause.
    #[tokio::test]
    async fn cleared_contexts_forget_their_own_pause() {
        let (_mock, handler, frame) = handler_with_frame().await;
        let state = handler
            .session(None)
            .await
            .unwrap()
            .debugger_state(None)
            .unwrap();
        // (with a breakpoint, so pauses are kept)
        let breakpoint = ChromeBreakpoint::on_tab("b");
        state.lock().await.breakpoints.add(breakpoint);
        let call_frame =
            json!({"callFrameId": "c1", "url": "", "location": {"scriptId": "7", "lineNumber": 1}});
        let paused = make_event("Debugger.paused", json!({"callFrames": [call_frame]}));
        let cleared = make_event("Runtime.executionContextsCleared", json!({}));
        let is_paused = async || state.lock().await.paused_call_frame_id.is_some();
        // Given the tab paused, when the frame's contexts are cleared
        process_debugger_event_in(&paused, &state, None).await;
        process_debugger_event_in(&cleared, &state, Some(&frame)).await;
        // Then the tab's pause is kept
        assert!(is_paused().await);
        // Given the frame paused, when its contexts are cleared
        process_debugger_event_in(&paused, &state, Some(&frame)).await;
        process_debugger_event_in(&cleared, &state, Some(&frame)).await;
        // Then its pause is forgotten
        assert!(!is_paused().await);
        // Given the frame paused, when the tab's contexts are cleared
        process_debugger_event_in(&paused, &state, Some(&frame)).await;
        process_debugger_event_in(&cleared, &state, None).await;
        // Then its pause is forgotten too (the frame went with the page)
        assert!(!is_paused().await);
        assert!(state.lock().await.paused_frame.is_none());
    }

    /// ISSUE-36: a detached frame's scripts go, and its late events are
    /// ignored.
    #[tokio::test]
    async fn detached_frames_are_forgotten() {
        let (_mock, handler, frame) = handler_with_frame().await;
        let state = handler
            .session(None)
            .await
            .unwrap()
            .debugger_state(None)
            .unwrap();
        let parsed = json!({"scriptId": "7", "hash": "h", "url": "http://b/inner.js"});
        process_debugger_event_in(
            &make_event("Debugger.scriptParsed", parsed),
            &state,
            Some(&frame),
        )
        .await;
        assert_eq!(state.lock().await.scripts.len(), 1);
        state.lock().await.forget_frame_target("T-frame");
        assert!(state.lock().await.scripts.is_empty());
        let parsed = json!({"scriptId": "8", "hash": "h", "url": "http://b/late.js"});
        process_debugger_event_in(
            &make_event("Debugger.scriptParsed", parsed),
            &state,
            Some(&frame),
        )
        .await;
        assert!(state.lock().await.scripts.is_empty());
    }

    /// ISSUE-36: a breakpoint by URL is set in the tab and the frame, says
    /// where the frame bound it when the tab did not, and is removed from
    /// both; one by a frame script's id goes to that frame with Chrome's id.
    #[tokio::test]
    async fn breakpoints_reach_the_frames() {
        use crate::handler::cdp_domains::debugger::remove_breakpoint::RemoveBreakpointTool;
        use crate::handler::cdp_domains::debugger::set_breakpoint::SetBreakpointTool;
        // Given a tab with no such script and a frame that binds it in script 7
        let (mock, handler, frame) = handler_with_frame().await;
        // (the tab is asked first, then the frame)
        let calls = std::sync::atomic::AtomicUsize::new(0);
        mock.respond_with("Debugger.setBreakpointByUrl", move |_| {
            let locations = match calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) {
                0 => json!([]),
                _ => json!([{"scriptId": "7", "lineNumber": 1, "columnNumber": 2}]),
            };
            json!({"breakpointId": "B", "locations": locations})
        });
        mock.respond_with(
            "Debugger.setBreakpoint",
            |_| json!({"breakpointId": "S", "actualLocation": {"scriptId": "7", "lineNumber": 3}}),
        );
        // When a breakpoint is set by URL
        let args = json!({"url": "http://b/inner.js", "line_number": 1});
        let reply = SetBreakpointTool::handle(call("set_breakpoint", args), &handler)
            .await
            .unwrap();
        // Then both sessions got it, and the reply is where the frame bound it
        let sent = sessions_of(mock, "Debugger.setBreakpointByUrl");
        assert_eq!(sent.len(), 2, "{sent:?}");
        assert_eq!(sent[1].as_deref(), Some(frame.as_str()));
        let text = reply_text(&reply);
        assert!(
            text.contains("\"resolved\": true") && text.contains("\"column_number\": 2"),
            "{text}"
        );
        // And removing it removes it from both
        let removed = call("remove_breakpoint", json!({"breakpoint_id": "bp-1"}));
        RemoveBreakpointTool::handle(removed, &handler)
            .await
            .unwrap();
        assert_eq!(sessions_of(mock, "Debugger.removeBreakpoint").len(), 2);
        // When a breakpoint is set by the frame script's id
        let args = json!({"script_id": "1.7", "line_number": 3});
        SetBreakpointTool::handle(call("set_breakpoint", args), &handler)
            .await
            .unwrap();
        // Then it went to the frame, with Chrome's id
        let by_id = mock.commands_named("Debugger.setBreakpoint");
        assert_eq!(by_id[0].session_id.as_deref(), Some(frame.as_str()));
        assert_eq!(by_id[0].params["location"]["scriptId"], "7");
    }

    /// ISSUE-36: a frame's script is read in the frame; a pause in the
    /// frame is resumed there.
    #[tokio::test]
    async fn frame_scripts_and_pauses_are_handled_in_the_frame() {
        use crate::handler::cdp_domains::debugger::resume::ResumeTool;
        use crate::handler::cdp_domains::debugger::search_scripts::SearchScriptsTool;
        // Given frame 1's script 7, and a pause in that frame
        let (mock, handler, frame) = handler_with_frame().await;
        mock.respond_with(
            "Debugger.getScriptSource",
            |_| json!({"scriptSource": "function innerHandler() {}"}),
        );
        let state = handler
            .session(None)
            .await
            .unwrap()
            .debugger_state(None)
            .unwrap();
        let parsed = json!({"scriptId": "7", "hash": "h", "url": "http://b/inner.js"});
        process_debugger_event_in(
            &make_event("Debugger.scriptParsed", parsed),
            &state,
            Some(&frame),
        )
        .await;
        {
            let mut state = state.lock().await;
            state.paused_call_frame_id = Some("c1".into());
            state.paused_frame = Some(frame.clone());
        }
        // When the scripts are searched, Then frame 1's is read in the frame
        let found = SearchScriptsTool::handle(
            call("search_scripts", json!({"query": "innerHandler"})),
            &handler,
        )
        .await
        .unwrap();
        assert!(
            reply_text(&found).contains("\"scriptId\": \"1.7\""),
            "{}",
            reply_text(&found)
        );
        let read = mock.commands_named("Debugger.getScriptSource");
        assert_eq!(read[0].session_id.as_deref(), Some(frame.as_str()));
        assert_eq!(read[0].params["scriptId"], "7");
        // When resume runs, Then the frame resumes
        ResumeTool::handle(call("resume", json!({})), &handler)
            .await
            .unwrap();
        assert_eq!(sessions_of(mock, "Debugger.resume"), [Some(frame)]);
    }
}
