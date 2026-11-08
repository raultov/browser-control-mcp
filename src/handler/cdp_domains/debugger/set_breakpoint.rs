use super::frames::{ChromeBreakpoint, frame_script_id, split_script_id};
use crate::handler::cdp_domains::cdp_target::CdpTarget;
use crate::handler::debugging::BreakpointSet;
use crate::handler::{DebuggerState, McpHandler};
use rust_mcp_sdk::{
    macros,
    schema::{CallToolError, CallToolRequestParams, CallToolResult},
};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::sync::Mutex;

#[macros::mcp_tool(
    name = "set_breakpoint",
    description = "Sets a breakpoint at a 0-based line of a script, by url (on Chrome also by script id or hash from search_scripts). A url without a scheme ('app.js') names the loaded script ending with it (several: fails naming them; none: Chrome binds to the first such script that loads, Firefox fails). 'condition' pauses only when it is truthy; 'log' makes a logpoint that writes to the console (get_console_logs, 'logpoint <url>:<line>:') instead of pausing. Returns {breakpoint_id, url, line_number, column_number, resolved}; resolved is false until a matching script loads. A line without code fails. The page's own debugger statements pause only while the tab has a breakpoint. Firefox: by url only.",
    destructive_hint = false
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct SetBreakpointTool {
    /// Default: 'default'.
    pub instance_id: Option<String>,
    /// Default: the active tab.
    pub tab_id: Option<String>,
    /// Script hash from search_scripts (Chrome).
    pub script_hash: Option<String>,
    /// Script id from search_scripts (Chrome).
    pub script_id: Option<String>,
    /// Script URL.
    pub url: Option<String>,
    /// 0-based line.
    pub line_number: u32,
    /// 0-based column. Default: the first breakable one of the line.
    pub column_number: Option<u32>,
    /// Pause only when this JavaScript expression is truthy (evaluated in the paused frame).
    pub condition: Option<String>,
    /// Log this expression's value to the console each time the line runs, without pausing (a logpoint; with condition, only when it holds).
    pub log: Option<String>,
}

impl SetBreakpointTool {
    pub async fn handle(
        params: CallToolRequestParams,
        handler: &McpHandler,
    ) -> Result<CallToolResult, CallToolError> {
        let mut args: SetBreakpointTool = crate::handler::parse_args(params)?;
        let session = handler.session(args.instance_id.clone()).await?;
        if args.script_id.is_none() && args.url.is_none() && args.script_hash.is_none() {
            return Err(CallToolError::from_message(
                "Either script_id, url or script_hash must be provided",
            ));
        }
        let target = session.target(args.tab_id.clone()).await?;
        let state = session.debugger_state(args.tab_id.clone())?;
        let url_regex = args.resolve_url(&*state.lock().await)?;
        let (method, params) = args.command(url_regex.as_deref()).ok_or_else(|| {
            CallToolError::from_message("Either script_id, url or script_hash must be provided")
        })?;
        let placed = match &args.script_id {
            Some(id) => place_in_script(&target, &state, id, params).await?,
            None => place_everywhere(&target, &state, method, params).await?,
        };
        let reply = placed.reply;
        let past_end = args.past_loaded_end(&reply, &*state.lock().await);
        if let Some(last) = past_end {
            remove_everywhere(&target, &state, &placed.breakpoint).await;
            return Err(CallToolError::from_message(format!(
                "There is no code at or after line {} in the loaded script with this URL (its last line is {last}): nothing was set. Pick a line with code ('search_scripts' finds it).",
                args.line_number
            )));
        }
        let set = {
            let mut state = state.lock().await;
            let mut set = args.resolution(&reply, &state);
            set.id = state.breakpoints.add(placed.breakpoint);
            set
        };
        super::sync_pauses(&target, &state).await;
        Ok(set.reply())
    }

    /// The last line of the loaded scripts with the requested URL (or
    /// hash), when Chrome bound the breakpoint to none of them: it then
    /// never fires, unless another script with that URL loads later.
    fn past_loaded_end(&self, reply: &Value, state: &crate::handler::DebuggerState) -> Option<i32> {
        if bound(reply) || self.script_id.is_some() {
            return None;
        }
        state
            .scripts
            .values()
            .filter(|s| match (&self.url, &self.script_hash) {
                (Some(url), _) => &s.url == url,
                (None, Some(hash)) => &s.hash == hash,
                (None, None) => false,
            })
            .map(|s| s.end_line)
            .max()
    }

    /// Where Chrome bound the breakpoint: the first resolved location
    /// (`locations` for by-URL breakpoints, `actualLocation` by script id),
    /// else the requested one, unresolved until a matching script loads.
    fn resolution(&self, reply: &Value, state: &crate::handler::DebuggerState) -> BreakpointSet {
        let resolved = reply["locations"]
            .as_array()
            .and_then(|l| l.first())
            .or_else(|| reply.get("actualLocation"));
        let number = |key: &str| {
            resolved
                .and_then(|l| l.get(key))
                .and_then(Value::as_u64)
                .and_then(|n| u32::try_from(n).ok())
        };
        let script_url = resolved
            .and_then(|l| l["scriptId"].as_str())
            .or(self.script_id.as_deref())
            .and_then(|id| state.scripts.get(id))
            .map(|script| script.url.clone())
            .filter(|url| !url.is_empty());
        BreakpointSet {
            id: String::new(),
            url: self.url.clone().or(script_url),
            line: number("lineNumber").unwrap_or(self.line_number),
            column: number("columnNumber").or(self.column_number),
            resolved: resolved.is_some(),
            condition: self.condition.clone(),
            log: self.log.clone(),
        }
    }

    /// A relative `url` made the loaded script's that ends with it; with
    /// none loaded, the `urlRegex` that matches the first one to load
    /// (ISSUE-87). Several are refused.
    fn resolve_url(&mut self, state: &DebuggerState) -> Result<Option<String>, CallToolError> {
        let Some(url) = self.url.clone().filter(|_| self.script_id.is_none()) else {
            return Ok(None);
        };
        let loaded = state.scripts.values().map(|script| script.url.as_str());
        let resolved = crate::handler::debugging::resolve_script_url(&url, loaded)
            .map_err(CallToolError::from_message)?;
        match resolved {
            Some(found) => {
                self.url = Some(found);
                Ok(None)
            }
            None => Ok(Some(suffix_regex(&url))),
        }
    }

    /// The condition Chrome is given for `condition` and `log`.
    fn condition(&self) -> Option<String> {
        let script = self.url.as_deref().or(self.script_id.as_deref());
        self.condition_at(script.or(self.script_hash.as_deref()).unwrap_or("?"))
    }

    /// The breakpoint condition for `condition` and `log`, a logpoint
    /// labelled `<script>:<line>` (see `debugging::breakpoint_condition`).
    pub(crate) fn condition_at(&self, script: &str) -> Option<String> {
        let label = format!("{script}:{}", self.line_number);
        crate::handler::debugging::breakpoint_condition(
            self.condition.as_deref(),
            self.log.as_deref(),
            &label,
        )
    }

    fn command(&self, url_regex: Option<&str>) -> Option<(&'static str, serde_json::Value)> {
        let (method, mut location, wrap) = if let Some(id) = &self.script_id {
            ("Debugger.setBreakpoint", json!({"scriptId": id}), true)
        } else if let Some(regex) = url_regex {
            (
                "Debugger.setBreakpointByUrl",
                json!({"urlRegex": regex}),
                false,
            )
        } else if let Some(url) = &self.url {
            ("Debugger.setBreakpointByUrl", json!({"url": url}), false)
        } else {
            let hash = self.script_hash.as_ref()?;
            (
                "Debugger.setBreakpointByUrl",
                json!({"scriptHash": hash}),
                false,
            )
        };
        if let Some(map) = location.as_object_mut() {
            map.insert("lineNumber".into(), json!(self.line_number));
            if let Some(col) = self.column_number {
                map.insert("columnNumber".into(), json!(col));
            }
        }
        let mut params = if wrap {
            json!({ "location": location })
        } else {
            location
        };
        if let Some(condition) = self.condition() {
            params["condition"] = json!(condition);
        }
        Some((method, params))
    }
}

/// The `urlRegex` of the scripts whose URL path ends with `tail` (a file
/// name or a relative path), query and fragment aside.
fn suffix_regex(tail: &str) -> String {
    let tail = tail.trim_start_matches("./").trim_start_matches('/');
    let mut escaped = String::new();
    for c in tail.chars() {
        if "\\.^$|?*+()[]{}".contains(c) {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    format!("(^|/){escaped}([?#].*)?$")
}

/// A breakpoint Chrome set, and the reply that says where it bound (its
/// script ids as the tab names them).
struct Placed {
    breakpoint: ChromeBreakpoint,
    reply: Value,
}

/// The id of the breakpoint a reply of Chrome's sets, or the error saying
/// it set none.
fn native_of(reply: &Value) -> Result<String, CallToolError> {
    super::frames::breakpoint_id(reply).ok_or_else(|| {
        CallToolError::from_message("Chrome set no breakpoint (its reply has no breakpointId)")
    })
}

/// Whether a reply of `Debugger.setBreakpointByUrl` bound the breakpoint.
fn bound(reply: &Value) -> bool {
    reply["locations"].as_array().is_some_and(|l| !l.is_empty())
}

/// Sets the breakpoint `params` (`Debugger.setBreakpoint`, by script id)
/// in the session of script `id`: the tab's, or its frame's (with Chrome's
/// id there).
async fn place_in_script(
    target: &CdpTarget,
    state: &Arc<Mutex<DebuggerState>>,
    id: &str,
    mut params: Value,
) -> Result<Placed, CallToolError> {
    let routed = state.lock().await.script_target(id, target);
    let (on, raw) = routed.ok_or_else(|| {
        CallToolError::from_message(format!(
            "The frame of script '{id}' is gone: run 'search_scripts' again."
        ))
    })?;
    params["location"]["scriptId"] = json!(raw);
    let response = crate::handler::send_or(&on, "Debugger.setBreakpoint", params, FAILURE).await?;
    let mut reply = response.result.unwrap_or_default();
    let native = native_of(&reply)?;
    if let Some(script) = reply.pointer_mut("/actualLocation/scriptId") {
        *script = json!(id);
    }
    let session = split_script_id(id)
        .0
        .and(on.page_tab())
        .map(|frame| frame.session_id().to_string());
    let breakpoint = ChromeBreakpoint {
        natives: vec![(session, native)],
        by_url: None,
    };
    Ok(Placed { breakpoint, reply })
}

/// Sets the breakpoint `params` (by URL or hash) in the tab and in each of
/// its debugged frames; the reply is the tab's, or the first frame's that
/// bound it when the tab's did not.
async fn place_everywhere(
    target: &CdpTarget,
    state: &Arc<Mutex<DebuggerState>>,
    method: &str,
    params: Value,
) -> Result<Placed, CallToolError> {
    let response = crate::handler::send_or(target, method, params.clone(), FAILURE).await?;
    let mut reply = response.result.unwrap_or_default();
    let mut breakpoint = ChromeBreakpoint {
        natives: vec![(None, native_of(&reply)?)],
        by_url: Some(params.clone()),
    };
    let frames = state.lock().await.debugged_frames();
    for frame in frames {
        let sent = frame.tab.send_raw_command(method, params.clone()).await;
        let Some(mut in_frame) = sent.ok().and_then(|response| response.result) else {
            continue;
        };
        let Some(native) = super::frames::breakpoint_id(&in_frame) else {
            continue;
        };
        let session = frame.tab.session_id().to_string();
        breakpoint.natives.push((Some(session), native));
        if !bound(&reply) && bound(&in_frame) {
            let locations = in_frame["locations"].as_array_mut().into_iter().flatten();
            for location in locations {
                let raw = location["scriptId"].as_str().unwrap_or_default();
                location["scriptId"] = json!(frame_script_id(frame.number, raw));
            }
            reply = in_frame;
        }
    }
    Ok(Placed { breakpoint, reply })
}

/// Removes `breakpoint` wherever it was set (best effort).
async fn remove_everywhere(
    target: &CdpTarget,
    state: &Arc<Mutex<DebuggerState>>,
    breakpoint: &ChromeBreakpoint,
) {
    for (_, native) in breakpoint
        .natives
        .iter()
        .filter(|(frame, _)| frame.is_none())
    {
        let remove = json!({"breakpointId": native});
        let _ = target
            .send_raw_command("Debugger.removeBreakpoint", remove)
            .await;
    }
    let frames = state.lock().await.debugged_frames();
    super::frames::remove_from_frames(breakpoint, &frames).await;
}

/// The error prefix of a breakpoint Chrome refused.
const FAILURE: &str = "Failed to set breakpoint";

#[cfg(test)]
mod tests {
    use super::*;
    use rust_mcp_sdk::schema::CallToolRequestParams;

    #[tokio::test]
    async fn test_set_breakpoint_validation() {
        let handler = McpHandler::new_test();

        // Missing all identifiers
        let params: CallToolRequestParams = serde_json::from_value(json!({
            "name": "set_breakpoint",
            "arguments": {
                "line_number": 10
            }
        }))
        .unwrap();
        let result = SetBreakpointTool::handle(params, &handler).await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Either script_id, url or script_hash must be provided")
        );
    }

    /// DBG-01 / DBG-02 (Chrome): set_breakpoint answers an MCP id with the
    /// resolved 0-based location; remove_breakpoint takes that id.
    #[tokio::test]
    async fn breakpoints_get_mcp_ids_and_resolved_locations() {
        use crate::handler::cdp_domains::debugger::remove_breakpoint::RemoveBreakpointTool;
        use crate::handler::cdp_domains::tests::{
            call, mock_handler, reply_text, spawn_recording_mock,
        };
        // Given a Chrome that binds app.js:3 to column 2
        let mock = spawn_recording_mock().await;
        mock.respond_with("Debugger.setBreakpointByUrl", |_| {
            json!({"breakpointId": "1:3:0:http://a/app.js", "locations": [{"scriptId": "7", "lineNumber": 3, "columnNumber": 2}]})
        });
        let handler = mock_handler(mock.http_port);
        // When set_breakpoint runs
        let args = json!({"url": "http://a/app.js", "line_number": 3});
        let reply = SetBreakpointTool::handle(call("set_breakpoint", args), &handler)
            .await
            .unwrap();
        // Then the reply has the MCP id and the bound location
        let text = reply_text(&reply);
        let body: Value = serde_json::from_str(&text[text.find('{').unwrap()..]).unwrap();
        assert_eq!(
            body,
            json!({"breakpoint_id": "bp-1", "url": "http://a/app.js", "line_number": 3, "column_number": 2, "resolved": true})
        );
        // And remove_breakpoint bp-1 removes the CDP breakpoint, once
        let remove = || call("remove_breakpoint", json!({"breakpoint_id": "bp-1"}));
        let removed = RemoveBreakpointTool::handle(remove(), &handler)
            .await
            .unwrap();
        assert!(reply_text(&removed).contains("\"removed\": \"bp-1\""));
        let sent = &mock.commands_named("Debugger.removeBreakpoint")[0].params;
        assert_eq!(sent["breakpointId"], "1:3:0:http://a/app.js");
        let again = RemoveBreakpointTool::handle(remove(), &handler)
            .await
            .unwrap_err();
        assert_eq!(again.to_string(), "No breakpoint 'bp-1'. Breakpoints: none");
    }

    /// AUD-EV-07: without breakpoints the page's `debugger;` statements
    /// are skipped (anti-debugging pages froze every tool); the first
    /// breakpoint turns pauses on and removing the last turns them off.
    #[tokio::test]
    async fn pauses_are_skipped_without_breakpoints() {
        use crate::handler::cdp_domains::debugger::remove_breakpoint::RemoveBreakpointTool;
        use crate::handler::cdp_domains::tests::{call, mock_handler, spawn_recording_mock};
        let mock = spawn_recording_mock().await;
        mock.respond_with(
            "Debugger.setBreakpointByUrl",
            |_| json!({"breakpointId": "1:3:0:http://a/app.js", "locations": []}),
        );
        let handler = mock_handler(mock.http_port);
        let skips = || -> Vec<Value> {
            mock.commands_named("Debugger.setSkipAllPauses")
                .into_iter()
                .map(|c| c.params["skip"].clone())
                .collect()
        };
        // When the first breakpoint is set (connecting enables the debugger)
        let args = json!({"url": "http://a/app.js", "line_number": 3});
        SetBreakpointTool::handle(call("set_breakpoint", args), &handler)
            .await
            .unwrap();
        // Then pauses were skipped after Debugger.enable, then turned on
        assert_eq!(skips(), [json!(true), json!(false)]);
        // And removing it skips them again
        let remove = call("remove_breakpoint", json!({"breakpoint_id": "bp-1"}));
        RemoveBreakpointTool::handle(remove, &handler)
            .await
            .unwrap();
        assert_eq!(skips(), [json!(true), json!(false), json!(true)]);
    }

    /// AUD-DB-07: a line after the end of the loaded script with that URL
    /// fails, saying where it ends, and the breakpoint is removed (it said
    /// `resolved: false`, "until a script loads", for a script loaded
    /// already). With no such script loaded the breakpoint waits for one.
    #[tokio::test]
    async fn line_past_a_loaded_script_fails() {
        use crate::handler::cdp_domains::tests::{call, mock_handler, spawn_recording_mock};
        let mock = spawn_recording_mock().await;
        mock.respond_with(
            "Debugger.setBreakpointByUrl",
            |_| json!({"result": {"breakpointId": "1:9999:0:http://a/app.js", "locations": []}}),
        );
        let handler = mock_handler(mock.http_port);
        let session = handler.session(None).await.unwrap();
        session.target(None).await.unwrap();
        let script = crate::handler::ScriptInfo {
            hash: "h".into(),
            start_line: 0,
            start_column: 0,
            end_line: 20,
            url: "http://a/app.js".into(),
        };
        let state = session.debugger_state(None).unwrap();
        state.lock().await.scripts.insert("7".into(), script);
        let args = json!({"url": "http://a/app.js", "line_number": 9999});
        let error = SetBreakpointTool::handle(call("set_breakpoint", args), &handler)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("no code at or after line 9999"), "{error}");
        assert!(error.contains("its last line is 20"), "{error}");
        let removed = &mock.commands_named("Debugger.removeBreakpoint")[0].params;
        assert_eq!(removed["breakpointId"], "1:9999:0:http://a/app.js");
        assert!(state.lock().await.breakpoints.is_empty());
        // A URL no loaded script has still waits
        let args = json!({"url": "http://a/later.js", "line_number": 3});
        let reply = SetBreakpointTool::handle(call("set_breakpoint", args), &handler).await;
        assert!(reply.is_ok(), "{:?}", reply.err());
    }

    /// ISSUE-87 (Chrome): a condition and a logpoint are sent as the
    /// breakpoint's condition (the logpoint logs and never pauses), and the
    /// reply says what was set.
    #[tokio::test]
    async fn conditions_and_logpoints_are_sent() {
        use crate::handler::cdp_domains::tests::{
            call, mock_handler, reply_text, spawn_recording_mock,
        };
        let mock = spawn_recording_mock().await;
        mock.respond_with("Debugger.setBreakpointByUrl", |_| {
            json!({"breakpointId": "1:3:0:http://a/app.js", "locations": [{"scriptId": "7", "lineNumber": 3, "columnNumber": 2}]})
        });
        let handler = mock_handler(mock.http_port);
        // When a logpoint with a condition is set
        let args =
            json!({"url": "http://a/app.js", "line_number": 3, "condition": "n > 1", "log": "sum"});
        let reply = SetBreakpointTool::handle(call("set_breakpoint", args), &handler)
            .await
            .unwrap();
        // Then Chrome gets the condition that logs and never pauses
        let sent = &mock.commands_named("Debugger.setBreakpointByUrl")[0].params;
        let expected = crate::handler::debugging::breakpoint_condition(
            Some("n > 1"),
            Some("sum"),
            "http://a/app.js:3",
        );
        assert_eq!(sent["condition"], json!(expected.unwrap()));
        // And the reply says it is a logpoint, with both
        let text = reply_text(&reply);
        assert!(text.starts_with("Logpoint set"), "{text}");
        assert!(
            text.contains("\"condition\": \"n > 1\"") && text.contains("\"log\": \"sum\""),
            "{text}"
        );
    }

    /// ISSUE-87 (Chrome): a relative URL names the loaded script that ends
    /// with it; with none loaded it waits for one by its end (`urlRegex`),
    /// and the reply says it is not bound yet (it was silently never bound).
    #[tokio::test]
    async fn relative_urls_are_resolved() {
        use crate::handler::cdp_domains::tests::{
            call, mock_handler, reply_text, spawn_recording_mock,
        };
        let mock = spawn_recording_mock().await;
        // (a loaded script binds it; a URL regex no loaded script matches does not)
        mock.respond_with("Debugger.setBreakpointByUrl", |params| {
            let locations = match params.get("url") {
                Some(_) => json!([{"scriptId": "7", "lineNumber": 1, "columnNumber": 0}]),
                None => json!([]),
            };
            json!({"breakpointId": "1:1:0:x", "locations": locations})
        });
        let handler = mock_handler(mock.http_port);
        let session = handler.session(None).await.unwrap();
        session.target(None).await.unwrap();
        let script = crate::handler::ScriptInfo {
            hash: "h".into(),
            start_line: 0,
            start_column: 0,
            end_line: 20,
            url: "http://a/js/dbg67.js".into(),
        };
        let state = session.debugger_state(None).unwrap();
        state.lock().await.scripts.insert("7".into(), script);
        // When a loaded script's file name is given
        let args = json!({"url": "dbg67.js", "line_number": 1});
        SetBreakpointTool::handle(call("set_breakpoint", args), &handler)
            .await
            .unwrap();
        // Then its URL is sent
        let sent = mock.commands_named("Debugger.setBreakpointByUrl");
        assert_eq!(sent[0].params["url"], "http://a/js/dbg67.js");
        // When a file name no loaded script ends with is given
        let args = json!({"url": "later.js", "line_number": 1});
        let reply = SetBreakpointTool::handle(call("set_breakpoint", args), &handler)
            .await
            .unwrap();
        // Then it waits for one ending with it, and says so
        let sent = &mock.commands_named("Debugger.setBreakpointByUrl")[1].params;
        assert_eq!(sent["urlRegex"], r"(^|/)later\.js([?#].*)?$");
        assert!(sent.get("url").is_none());
        assert!(reply_text(&reply).starts_with("Breakpoint set, but not bound yet"));
    }
}
