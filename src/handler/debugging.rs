//! The replies of the debugger tools, the same on both browsers.
//!
//! Breakpoints get MCP-level ids (`bp-N`, per tab) whatever the protocol's
//! own (CDP `1:3:0:http://…`, a Firefox location), like tab ids. Every line
//! number an agent reads or writes is 0-based (Firefox's RDP counts from 1;
//! the backends convert).

use rust_mcp_sdk::schema::CallToolResult;
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// The error of a tool that needs a paused debugger.
pub(crate) const NOT_PAUSED: &str = "the debugger is not paused";

/// How long `step_over` waits for the thread to stop again.
pub(crate) const STEP_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

/// How long `resume` watches for an immediate new pause (another
/// breakpoint hit right away), so the agent does not have to poll.
pub(crate) const RESUME_WAIT: std::time::Duration = std::time::Duration::from_millis(250);

/// Breakpoints of one tab by MCP id (`bp-1`, `bp-2`...), mapped to what the
/// protocol needs to remove them. Ids are not reused.
#[derive(Debug)]
pub(crate) struct BreakpointIds<T> {
    by_id: BTreeMap<String, T>,
    counter: usize,
}

impl<T> Default for BreakpointIds<T> {
    fn default() -> Self {
        Self {
            by_id: BTreeMap::new(),
            counter: 0,
        }
    }
}

impl<T> BreakpointIds<T> {
    /// Registers a breakpoint; returns its MCP id.
    pub(crate) fn add(&mut self, native: T) -> String {
        self.counter += 1;
        let id = format!("bp-{}", self.counter);
        self.by_id.insert(id.clone(), native);
        id
    }

    /// The protocol handle of `id`, or the error listing the known ids.
    pub(crate) fn get(&self, id: &str) -> Result<&T, String> {
        self.by_id.get(id).ok_or_else(|| {
            let ids: Vec<&str> = self.by_id.keys().map(String::as_str).collect();
            let known = if ids.is_empty() {
                "none".to_string()
            } else {
                ids.join(", ")
            };
            format!("No breakpoint '{id}'. Breakpoints: {known}")
        })
    }

    /// Forgets `id` (once the browser removed it).
    pub(crate) fn remove(&mut self, id: &str) -> Option<T> {
        self.by_id.remove(id)
    }

    /// The id already registered for `native`, if any.
    pub(crate) fn id_of(&self, native: &T) -> Option<&str>
    where
        T: PartialEq,
    {
        self.by_id
            .iter()
            .find(|(_, known)| *known == native)
            .map(|(id, _)| id.as_str())
    }
}

/// Where a breakpoint was set.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct BreakpointSet {
    /// MCP id.
    pub(crate) id: String,
    /// Script URL, when known.
    pub(crate) url: Option<String>,
    /// 0-based line.
    pub(crate) line: u32,
    /// 0-based column, when known.
    pub(crate) column: Option<u32>,
    /// Whether a loaded script has this location (else it binds when one loads).
    pub(crate) resolved: bool,
}

impl BreakpointSet {
    /// The reply object.
    pub(crate) fn to_json(&self) -> Value {
        json!({
            "breakpoint_id": self.id,
            "url": self.url,
            "line_number": self.line,
            "column_number": self.column,
            "resolved": self.resolved,
        })
    }

    /// The tool reply.
    pub(crate) fn reply(&self) -> CallToolResult {
        text("Breakpoint set.", &self.to_json())
    }
}

/// `remove_breakpoint`'s reply.
pub(crate) fn removed(id: &str) -> CallToolResult {
    text("Breakpoint removed.", &json!({ "removed": id }))
}

/// Where the thread stopped.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PauseLocation {
    /// Script URL, when known.
    pub(crate) url: Option<String>,
    /// 0-based line.
    pub(crate) line: u32,
    /// 0-based column.
    pub(crate) column: u32,
}

/// `{paused, location}` after a step or a resume.
pub(crate) fn pause_json(location: Option<&PauseLocation>) -> Value {
    let location =
        location.map(|l| json!({"url": l.url, "line_number": l.line, "column_number": l.column}));
    json!({"paused": location.is_some(), "location": location})
}

/// `step_over`'s reply.
pub(crate) fn stepped(location: Option<&PauseLocation>) -> CallToolResult {
    text(
        "Stepped over to the next expression in the debugger.",
        &pause_json(location),
    )
}

/// `resume`'s reply (`paused` true when it stopped again right away).
pub(crate) fn resumed(location: Option<&PauseLocation>) -> CallToolResult {
    text("Debugger execution resumed.", &pause_json(location))
}

/// The error of an expression that threw in a paused frame.
pub(crate) fn frame_exception(thrown: &str) -> String {
    format!("Evaluation threw: {thrown}")
}

fn text(first_line: &str, body: &Value) -> CallToolResult {
    let body = serde_json::to_string_pretty(body).unwrap_or_default();
    CallToolResult::text_content(vec![format!("{first_line}\n{body}").into()])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// DBG-01: breakpoint ids are MCP-level, never reused, and an unknown id
    /// lists the known ones.
    #[test]
    fn breakpoint_ids_are_mcp_level() {
        // Given two breakpoints, the first removed
        let mut ids = BreakpointIds::default();
        assert_eq!(ids.add("1:3:0:http://a/app.js"), "bp-1");
        assert_eq!(ids.add("1:4:0:http://a/app.js"), "bp-2");
        assert_eq!(ids.remove("bp-1"), Some("1:3:0:http://a/app.js"));
        // When another is added and an unknown one is asked for
        let third = ids.add("x");
        let error = ids.get("bp-1").unwrap_err();
        // Then the id is new and the error lists the live ids
        assert_eq!(third, "bp-3");
        assert_eq!(error, "No breakpoint 'bp-1'. Breakpoints: bp-2, bp-3");
    }

    /// DBG-01 / DBG-03: the reply shapes.
    #[test]
    fn debugger_reply_shapes() {
        let set = BreakpointSet {
            id: "bp-1".into(),
            url: Some("http://a/app.js".into()),
            line: 3,
            column: Some(2),
            resolved: true,
        };
        assert_eq!(
            set.to_json(),
            json!({"breakpoint_id": "bp-1", "url": "http://a/app.js", "line_number": 3, "column_number": 2, "resolved": true})
        );
        let at = PauseLocation {
            url: None,
            line: 7,
            column: 0,
        };
        assert_eq!(
            pause_json(Some(&at)),
            json!({"paused": true, "location": {"url": null, "line_number": 7, "column_number": 0}})
        );
        assert_eq!(pause_json(None), json!({"paused": false, "location": null}));
    }
}
