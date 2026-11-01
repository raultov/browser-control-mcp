pub mod evaluate_on_call_frame;
pub(crate) mod frames;
pub mod pause_on_load;
pub mod remove_breakpoint;
pub mod resume;
pub mod search_scripts;
pub mod set_breakpoint;
pub mod step_over;

use crate::handler::cdp_domains::cdp_target::CdpTarget;
use crate::handler::debugging::PauseLocation;
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
        Some("Debugger.paused") => on_paused(&event.params, &mut st, frame),
        Some("Debugger.resumed") if st.paused_frame.as_deref() == frame => {
            st.paused_call_frame_id = None;
            st.paused_at = None;
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
    let id = number.map_or_else(
        || script_id.to_string(),
        |number| frames::frame_script_id(number, script_id),
    );
    let info = ScriptInfo {
        hash: hash.to_string(),
        start_line: position(params, "startLine"),
        start_column: position(params, "startColumn"),
        end_line: position(params, "endLine"),
        url: extract_from_value(params, "url")
            .unwrap_or_default()
            .to_string(),
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
        st.paused_call_frame_id = Some(call_frame_id.to_string());
        st.paused_at = params.as_ref().and_then(pause_location);
        st.paused_frame = frame.map(str::to_string);
    }
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
/// `frames::debugger_events`); its script is named from `state`'s parsed
/// scripts.
pub(crate) async fn wait_for_pause(
    events: &mut (impl tokio_stream::Stream<Item = cdp_browser_lite::CdpResult<WsResponse>> + Unpin),
    limit: std::time::Duration,
    state: &Arc<Mutex<DebuggerState>>,
) -> Option<PauseLocation> {
    let location = next_pause(events, limit).await?;
    Some(with_script_url(location, &*state.lock().await))
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
    let (skip, frames) = {
        let state = state.lock().await;
        (state.skips_pauses(), state.debugged_frames())
    };
    let skip = serde_json::json!({ "skip": skip });
    let _ = target
        .send_raw_command("Debugger.setSkipAllPauses", skip.clone())
        .await;
    for frame in frames {
        let _ = frame
            .tab
            .send_raw_command("Debugger.setSkipAllPauses", skip.clone())
            .await;
    }
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
    crate::handler::cdp_domains::event_pump::spawn_state_listener(
        target, "Debugger", state, process,
    )
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::handler::cdp_domains::tests::make_event;
    use serde_json::json;

    #[tokio::test]
    async fn test_script_parsed_inserts_script_info() {
        let state = Arc::new(Mutex::new(DebuggerState::default()));
        let event = make_event(
            "Debugger.scriptParsed",
            json!({
                "scriptId": "42",
                "hash": "abc123",
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
                "hash": "def456"
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

    /// FIX-01: Script start positions are read from integers.
    #[tokio::test]
    async fn script_positions_read_from_integers() {
        // Given Debugger.scriptParsed with numeric positions, as Chrome sends them
        let state = Arc::new(Mutex::new(DebuggerState::default()));
        let event = make_event(
            "Debugger.scriptParsed",
            serde_json::json!({"scriptId": "7", "hash": "h", "startLine": 12, "startColumn": 4}),
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
