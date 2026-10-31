use crate::handler::McpHandler;
use crate::handler::debugging::BreakpointSet;
use rust_mcp_sdk::{
    macros,
    schema::{CallToolError, CallToolRequestParams, CallToolResult},
};
use serde_json::{Value, json};

#[macros::mcp_tool(
    name = "set_breakpoint",
    description = "Sets a breakpoint at a 0-based line of a script (by url; on Chrome also by script id or hash from 'search_scripts'). Returns {breakpoint_id, url, line_number, column_number, resolved} where the browser bound it; resolved is false until a script with that URL loads. A line without code fails. The page's own debugger statements pause only while the tab has a breakpoint. Firefox: by url only."
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct SetBreakpointTool {
    /// Instance id; omit for the default instance.
    pub instance_id: Option<String>,
    /// Tab id; omit for the active tab.
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
}

impl SetBreakpointTool {
    pub async fn handle(
        params: CallToolRequestParams,
        handler: &McpHandler,
    ) -> Result<CallToolResult, CallToolError> {
        let args: SetBreakpointTool = crate::handler::parse_args(params)?;
        let session = handler.session(args.instance_id.clone()).await?;
        let (method, params) = args.command().ok_or_else(|| {
            CallToolError::from_message("Either script_id, url or script_hash must be provided")
        })?;
        let target = session.target(args.tab_id.clone()).await?;
        let failure = "Failed to set breakpoint";
        let response = crate::handler::send_or(&target, method, params, failure).await?;
        let reply = response.result.unwrap_or_default();
        let native = reply["breakpointId"]
            .as_str()
            .filter(|id| !id.is_empty())
            .ok_or_else(|| {
                CallToolError::from_message(
                    "Chrome set no breakpoint (its reply has no breakpointId)",
                )
            })?
            .to_string();
        let state = session.debugger_state(args.tab_id.clone())?;
        let past_end = args.past_loaded_end(&reply, &*state.lock().await);
        if let Some(last) = past_end {
            let remove = json!({"breakpointId": native});
            let _ = target
                .send_raw_command("Debugger.removeBreakpoint", remove)
                .await;
            return Err(CallToolError::from_message(format!(
                "There is no code at or after line {} in the loaded script with this URL (its last line is {last}): nothing was set. Pick a line with code ('search_scripts' finds it).",
                args.line_number
            )));
        }
        let set = {
            let mut state = state.lock().await;
            let mut set = args.resolution(&reply, &state);
            set.id = state.breakpoints.add(native);
            set
        };
        super::sync_pauses(&target, &state).await;
        Ok(set.reply())
    }

    /// The last line of the loaded scripts with the requested URL (or
    /// hash), when Chrome bound the breakpoint to none of them: it then
    /// never fires, unless another script with that URL loads later.
    fn past_loaded_end(&self, reply: &Value, state: &crate::handler::DebuggerState) -> Option<i32> {
        let bound = reply["locations"].as_array().is_some_and(|l| !l.is_empty());
        if bound || self.script_id.is_some() {
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
        }
    }

    fn command(&self) -> Option<(&'static str, serde_json::Value)> {
        let (method, mut location, wrap) = if let Some(id) = &self.script_id {
            ("Debugger.setBreakpoint", json!({"scriptId": id}), true)
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
        let params = if wrap {
            json!({ "location": location })
        } else {
            location
        };
        Some((method, params))
    }
}

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
}
