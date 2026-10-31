pub mod evaluate_on_call_frame;
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

/// Processes a single CDP debugger event, updating the shared state accordingly.
/// This function is extracted from the listener loop to enable isolated unit testing.
/// Returns `true` for a pause the tab must not keep (`DebuggerState::
/// skips_pauses`): it is not recorded, and the listener resumes it.
pub(crate) async fn process_debugger_event(
    event: &WsResponse,
    state: &Arc<Mutex<DebuggerState>>,
) -> bool {
    if event.method.as_deref() == Some("Debugger.paused") && state.lock().await.skips_pauses() {
        return true;
    }
    if let Some(method) = event.method.as_deref() {
        if method == "Debugger.scriptParsed" {
            if let (Some(script_id), Some(hash)) = (
                extract_from_value(&event.params, "scriptId"),
                extract_from_value(&event.params, "hash"),
            ) {
                let start_line = position(&event.params, "startLine");
                let start_column = position(&event.params, "startColumn");
                let end_line = position(&event.params, "endLine");
                let url = extract_from_value(&event.params, "url").unwrap_or_default();

                let mut st = state.lock().await;
                st.scripts.insert(
                    script_id.to_string(),
                    ScriptInfo {
                        hash: hash.to_string(),
                        start_line,
                        start_column,
                        end_line,
                        url: url.to_string(),
                    },
                );
            }
        } else if method == "Debugger.paused"
            && let Some(call_first_frame_id) = event
                .params
                .as_ref()
                .and_then(|p: &serde_json::Value| p.get("callFrames"))
                .and_then(|frames: &serde_json::Value| frames.as_array())
                .and_then(|frames: &Vec<serde_json::Value>| frames.first())
                .and_then(|first_frame: &serde_json::Value| first_frame.get("callFrameId"))
                .and_then(|id: &serde_json::Value| id.as_str())
        {
            state.lock().await.paused_call_frame_id = Some(call_first_frame_id.to_string());
        } else if method == "Debugger.resumed" {
            state.lock().await.paused_call_frame_id = None;
        }
    }
    false
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

/// Waits up to `limit` for the next `Debugger.paused` on `events`; its
/// script is named from `state`'s parsed scripts.
pub(crate) async fn wait_for_pause(
    events: &mut cdp_browser_lite::EventFilter,
    limit: std::time::Duration,
    state: &Arc<Mutex<DebuggerState>>,
) -> Option<PauseLocation> {
    let location = next_pause(events, limit).await?;
    Some(with_script_url(location, &*state.lock().await))
}

/// The next `Debugger.paused` on `events` within `limit`.
async fn next_pause(
    events: &mut cdp_browser_lite::EventFilter,
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
/// from the tab's state (see `DebuggerState::skips_pauses`). Best effort: an
/// older Chrome without the command keeps pausing.
pub(crate) async fn sync_pauses(target: &CdpTarget, state: &Arc<Mutex<DebuggerState>>) {
    let skip = state.lock().await.skips_pauses();
    let _ = target
        .send_raw_command(
            "Debugger.setSkipAllPauses",
            serde_json::json!({ "skip": skip }),
        )
        .await;
}

/// Keeps the tab's debugger state from its `Debugger` events. A pause with
/// no breakpoint and no `pause_on_load` (the page's own `debugger;`) is
/// resumed at once: `Debugger.setSkipAllPauses` does not survive a
/// cross-process navigation, so it is sent again too.
pub(crate) fn start_debugger_listener(
    target: &crate::handler::cdp_domains::cdp_target::CdpTarget,
    state: Arc<Mutex<DebuggerState>>,
) -> tokio::task::JoinHandle<()> {
    let page = target.clone();
    let process = move |event, state| {
        let page = page.clone();
        async move {
            if process_debugger_event(&event, &state).await {
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
        state.breakpoints.add("1:3:0:http://a/app.js".into());
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
