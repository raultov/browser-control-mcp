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

    /// Whether no breakpoint is registered.
    pub(crate) fn is_empty(&self) -> bool {
        self.by_id.is_empty()
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
    /// Chrome's script id (what `set_breakpoint`'s `script_id` takes), when
    /// the browser gives one.
    pub(crate) script_id: Option<String>,
    /// 0-based line.
    pub(crate) line: u32,
    /// 0-based column.
    pub(crate) column: u32,
}

impl PauseLocation {
    /// `{url, script_id?, line_number, column_number}` for replies.
    pub(crate) fn to_json(&self) -> Value {
        let mut out =
            json!({"url": self.url, "line_number": self.line, "column_number": self.column});
        if let Some(id) = &self.script_id {
            out["script_id"] = json!(id);
        }
        out
    }

    /// ` at <url> line N, column M (0-based)` for messages (the script id
    /// when the URL is unknown).
    fn at(&self) -> String {
        let (line, column) = (self.line, self.column);
        match (&self.url, &self.script_id) {
            (Some(url), _) => format!(" at {url} line {line}, column {column} (0-based)"),
            (None, Some(id)) => format!(" at script {id} line {line}, column {column} (0-based)"),
            (None, None) => format!(" at line {line}, column {column} (0-based)"),
        }
    }
}

/// A tab whose thread is stopped in the debugger now, where (when known).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Paused(pub(crate) Option<PauseLocation>);

/// `{paused, location}` after a step or a resume.
pub(crate) fn pause_json(location: Option<&PauseLocation>) -> Value {
    let location = location.map(PauseLocation::to_json);
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

/// The error of a call the page interrupted by pausing in the debugger (its
/// own code hit a breakpoint): the page answers nothing more until it is
/// resumed, so the call fails at once saying where it stopped.
pub(crate) fn paused_during(location: Option<&PauseLocation>, call: &str) -> String {
    let at = location.map(PauseLocation::at).unwrap_or_default();
    format!(
        "The page paused in the debugger{at} while running {call}: the call did not finish, though its effect (a click, a navigation) may have happened. Inspect the paused frame with evaluate_on_call_frame, then continue with resume or step_over."
    )
}

/// The sentence a reply adds after an action whose own code paused the
/// page in the debugger (the action happened).
pub(crate) fn paused_note(location: Option<&PauseLocation>) -> String {
    let at = location.map(PauseLocation::at).unwrap_or_default();
    format!(
        "The page paused in the debugger{at}: inspect the paused frame with evaluate_on_call_frame, then continue with resume or step_over."
    )
}

/// Why input `tool` refused to run on a page paused in the debugger: the
/// page handles input events only once it resumes, so a click or a text
/// would be reported done while nothing happened.
pub(crate) fn paused_input(tool: &str, paused: &Paused) -> String {
    let at = paused.0.as_ref().map(PauseLocation::at).unwrap_or_default();
    format!(
        "The page is paused in the debugger{at}: it handles input events only once it resumes, so {tool} did nothing. Continue with resume or step_over first; evaluate_js and evaluate_on_call_frame still work while paused."
    )
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
            script_id: None,
            line: 7,
            column: 0,
        };
        assert_eq!(
            pause_json(Some(&at)),
            json!({"paused": true, "location": {"url": null, "line_number": 7, "column_number": 0}})
        );
        assert_eq!(pause_json(None), json!({"paused": false, "location": null}));
    }

    /// ISSUE-1: a call interrupted by a pause says where the page stopped
    /// (0-based, as every tool reports it) and how to go on.
    #[test]
    fn paused_call_names_the_location_and_next_steps() {
        // Given a pause with and without the script URL
        let at = |url: Option<&str>| PauseLocation {
            url: url.map(str::to_string),
            script_id: None,
            line: 3,
            column: 2,
        };
        // When the interrupted call's error is worded
        let named = paused_during(Some(&at(Some("http://a/cart.js"))), "click_element");
        let unnamed = paused_during(Some(&at(None)), "Input.dispatchMouseEvent");
        // Then it gives the location, the call and the tools to continue
        assert_eq!(
            named,
            "The page paused in the debugger at http://a/cart.js line 3, column 2 (0-based) while running click_element: the call did not finish, though its effect (a click, a navigation) may have happened. Inspect the paused frame with evaluate_on_call_frame, then continue with resume or step_over."
        );
        assert!(
            unnamed.starts_with("The page paused in the debugger at line 3, column 2 (0-based) while running Input.dispatchMouseEvent:"),
            "{unnamed}"
        );
        assert!(
            paused_during(None, "x")
                .starts_with("The page paused in the debugger while running x:")
        );
    }
}
