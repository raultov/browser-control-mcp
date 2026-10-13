use crate::handler::McpHandler;
use crate::handler::debugging::BreakpointSet;
use rust_mcp_sdk::{
    macros,
    schema::{CallToolError, CallToolRequestParams, CallToolResult},
};
use serde_json::{Value, json};

#[macros::mcp_tool(
    name = "set_breakpoint",
    description = "Sets a debugger breakpoint at a specific location, pausing execution when reached. Side effects: modifies debugger state (breakpoint added until removed). Prerequisites: requires an active Chrome tab; target script must be loaded. Returns: JSON {breakpoint_id ('bp-N', for remove_breakpoint), url, line_number, column_number (0-based, where the browser bound it), resolved (false until a script with that URL loads)}. Use this to debug specific code paths. Alternatives: 'pause_on_load' for early script execution, 'search_scripts' to find scripts by pattern."
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct SetBreakpointTool {
    /// Chrome instance id from open_instance/list_instances. Omit for the default instance.
    pub instance_id: Option<String>,
    /// The Tab ID of the target tab. Omit to use the active tab.
    pub tab_id: Option<String>,
    /// Script hash to identify the target script. Constraints: one of 'script_hash', 'script_id', or 'url' must be provided. Defaults to: None.
    pub script_hash: Option<String>,
    /// Script ID (from Debugger.scriptParsed event). Constraints: one of 'script_hash', 'script_id', or 'url' must be provided. Interactions: mutually exclusive with 'script_hash' and 'url' (first match wins). Defaults to: None.
    pub script_id: Option<String>,
    /// Script URL to match. Constraints: one of 'script_hash', 'script_id', or 'url' must be provided. Interactions: mutually exclusive with 'script_hash' and 'script_id'. Defaults to: None.
    pub url: Option<String>,
    /// Line number where breakpoint is set (0-indexed). Constraints: non-negative integer, must be within script bounds. Interactions: required parameter; combined with 'column_number' to pinpoint exact location.
    pub line_number: u32,
    /// Column number within the line. Constraints: non-negative integer. Interactions: optional; narrower precision if provided. Defaults to: start of line (0).
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
            .unwrap_or_default()
            .to_string();
        let state = session.debugger_state(args.tab_id.clone())?;
        let mut state = state.lock().await;
        let mut set = args.resolution(&reply, &state);
        set.id = state.breakpoints.add(native);
        Ok(set.reply())
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
}
