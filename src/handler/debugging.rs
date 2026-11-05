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

/// The first words of `resume`'s reply when nothing was paused (the same
/// on every browser: Chrome answers it sometimes, Firefox refuses it).
pub(crate) const NOTHING_TO_RESUME: &str = "The debugger was not paused: nothing to resume.";

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

    /// Mutable access to the protocol handle of `id`.
    pub(crate) fn get_mut(&mut self, id: &str) -> Option<&mut T> {
        self.by_id.get_mut(id)
    }

    /// Every breakpoint: its MCP id and protocol handle.
    pub(crate) fn iter_mut(&mut self) -> impl Iterator<Item = (&String, &mut T)> {
        self.by_id.iter_mut()
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
    /// When it pauses: only when this expression is truthy (ISSUE-87).
    pub(crate) condition: Option<String>,
    /// What it logs, as a logpoint that never pauses (ISSUE-87).
    pub(crate) log: Option<String>,
}

impl BreakpointSet {
    /// The reply object.
    pub(crate) fn to_json(&self) -> Value {
        let mut out = json!({
            "breakpoint_id": self.id,
            "url": self.url,
            "line_number": self.line,
            "column_number": self.column,
            "resolved": self.resolved,
            "condition": self.condition,
            "log": self.log,
        });
        // Only a breakpoint with a condition or a log lists them.
        if let Some(fields) = out.as_object_mut() {
            fields.retain(|key, value| {
                !(value.is_null() && ["condition", "log"].contains(&key.as_str()))
            });
        }
        out
    }

    /// The tool reply: a logpoint says where its output goes, and an
    /// unresolved breakpoint says it is not bound yet (ISSUE-87).
    pub(crate) fn reply(&self) -> CallToolResult {
        let first = match (self.log.is_some(), self.resolved) {
            (true, true) => {
                "Logpoint set: it logs to the console (get_console_logs) each time the line runs, and never pauses."
            }
            (false, true) => "Breakpoint set.",
            (true, false) => {
                "Logpoint set, but not bound yet: no loaded script has this URL, so it logs nothing until one loads (check the URL with search_scripts)."
            }
            (false, false) => {
                "Breakpoint set, but not bound yet: no loaded script has this URL, so it pauses nothing until one loads (check the URL with search_scripts)."
            }
        };
        text(first, &self.to_json())
    }
}

/// The condition a breakpoint is set with for `condition` (pause when it
/// is truthy) and `log` (a logpoint: log `label` and its value, and never
/// pause; only when `condition` holds, with one), as DevTools does on
/// Chrome. The expressions are on lines of their own, so a trailing `//`
/// comment cannot swallow the rest (ISSUE-87).
pub(crate) fn breakpoint_condition(
    condition: Option<&str>,
    log: Option<&str>,
    label: &str,
) -> Option<String> {
    let label = serde_json::to_string(&format!("logpoint {label}:")).unwrap_or_default();
    match (condition, log) {
        (None, None) => None,
        (Some(condition), None) => Some(format!("(\n{condition}\n)")),
        (None, Some(log)) => Some(format!("(console.log({label}, (\n{log}\n)), false)")),
        (Some(condition), Some(log)) => Some(format!(
            "((\n{condition}\n) && console.log({label}, (\n{log}\n)), false)"
        )),
    }
}

/// Whether `url` is absolute (it has a scheme: `http://`, `file://`,
/// `data:`...), as opposed to a path or a file name.
pub(crate) fn is_absolute_url(url: &str) -> bool {
    let scheme = url.split_once(':').map(|(scheme, _)| scheme);
    scheme.is_some_and(|s| {
        !s.is_empty()
            && s.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c))
    })
}

/// The script a breakpoint `url` names among the `loaded` script URLs: an
/// absolute URL is kept; a relative one (`app.js`, `js/app.js`) names the
/// loaded URL whose path ends with it, `None` when none does, and is
/// refused when several (distinct) do, naming them (ISSUE-87).
pub(crate) fn resolve_script_url<'a>(
    url: &str,
    loaded: impl Iterator<Item = &'a str>,
) -> Result<Option<String>, String> {
    if is_absolute_url(url) {
        return Ok(Some(url.to_string()));
    }
    let tail = url.trim_start_matches("./").trim_start_matches('/');
    let ends = |loaded: &str| {
        let path = loaded.split(['?', '#']).next().unwrap_or_default();
        path.ends_with(&format!("/{tail}"))
    };
    let mut found: Vec<String> = loaded.filter(|l| ends(l)).map(str::to_string).collect();
    found.sort();
    found.dedup();
    match found.len() {
        0 => Ok(None),
        1 => Ok(found.pop()),
        _ => Err(format!(
            "'{url}' is not an absolute URL, and several loaded scripts end with it: {}. Give one of them as 'url'.",
            found.join(", ")
        )),
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
    pub(crate) fn at(&self) -> String {
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

/// A step of a paused thread (`step_over`, `step_into`, `step_out`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Step {
    /// Runs the paused line, stepping over calls.
    Over,
    /// Enters the call on the paused line.
    Into,
    /// Runs to the end of the paused function, stopping in its caller.
    Out,
}

impl Step {
    /// Its Chrome command.
    pub(crate) fn cdp_method(self) -> &'static str {
        match self {
            Self::Over => "Debugger.stepOver",
            Self::Into => "Debugger.stepInto",
            Self::Out => "Debugger.stepOut",
        }
    }

    /// The first line of its reply.
    fn done(self) -> &'static str {
        match self {
            Self::Over => "Stepped over to the next expression in the debugger.",
            Self::Into => "Stepped into the call in the debugger.",
            Self::Out => "Stepped out to the caller in the debugger.",
        }
    }
}

/// The reply of `step` (`step_over`, `step_into`, `step_out`).
pub(crate) fn stepped(step: Step, location: Option<&PauseLocation>) -> CallToolResult {
    text(step.done(), &pause_json(location))
}

/// Most frames `call_stack` lists (the innermost), async callers included.
pub(crate) const MAX_STACK_FRAMES: usize = 50;

/// Most async callers (`setTimeout`, a promise...) `call_stack` lists.
pub(crate) const MAX_ASYNC_CALLERS: usize = 5;

/// One frame of a paused thread's stack.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct StackFrame {
    /// Empty for an anonymous function.
    pub(crate) function_name: String,
    pub(crate) location: PauseLocation,
}

impl StackFrame {
    fn to_json(&self) -> Value {
        let mut out = self.location.to_json();
        let name = match self.function_name.as_str() {
            "" => "(anonymous)",
            name => name,
        };
        out["function_name"] = json!(name);
        out
    }
}

/// An async caller of the stack: what scheduled it (`setTimeout`,
/// `Promise.then`...) and its frames.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AsyncSegment {
    pub(crate) description: String,
    pub(crate) frames: Vec<StackFrame>,
}

/// A paused thread's stack, innermost frame first (ISSUE-67).
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct CallStack {
    /// Why it paused (see [`pause_reason`]).
    pub(crate) reason: String,
    /// What was thrown, when it paused on an exception.
    pub(crate) exception: Option<String>,
    pub(crate) frames: Vec<StackFrame>,
    pub(crate) async_parents: Vec<AsyncSegment>,
    /// More frames than [`MAX_STACK_FRAMES`] were left out.
    pub(crate) truncated: bool,
}

impl CallStack {
    /// `{reason, exception?, frames: [{index, function_name, url,
    /// script_id?, line_number, column_number}], async_callers?, truncated?}`.
    pub(crate) fn to_json(&self) -> Value {
        let frames: Vec<Value> = self
            .frames
            .iter()
            .enumerate()
            .map(|(index, frame)| {
                let mut out = frame.to_json();
                out["index"] = json!(index);
                out
            })
            .collect();
        let mut out = json!({"reason": self.reason, "frames": frames});
        if let Some(thrown) = &self.exception {
            out["exception"] = json!(thrown);
        }
        if !self.async_parents.is_empty() {
            let parents: Vec<Value> = self
                .async_parents
                .iter()
                .map(|segment| {
                    let frames: Vec<Value> =
                        segment.frames.iter().map(StackFrame::to_json).collect();
                    json!({"description": segment.description, "frames": frames})
                })
                .collect();
            out["async_callers"] = json!(parents);
        }
        if self.truncated {
            out["truncated"] = json!(true);
        }
        out
    }

    /// `call_stack`'s reply.
    pub(crate) fn reply(&self) -> CallToolResult {
        let first = format!(
            "Paused ({}): {} frame(s), innermost first; evaluate_on_call_frame takes a frame_index.",
            self.reason,
            self.frames.len()
        );
        text(&first, &self.to_json())
    }
}

/// A pause's reason in the words both browsers share: `breakpoint`,
/// `step`, `debugger_statement`, `exception`, `promise_rejection`...
/// (Chrome's and Firefox's own names, in snake case).
pub(crate) fn pause_reason(native: &str) -> String {
    match native {
        "resumeLimit" => "step".to_string(),
        other => {
            let mut out = String::new();
            for c in other.chars() {
                if c.is_ascii_uppercase() {
                    out.push('_');
                }
                out.push(c.to_ascii_lowercase());
            }
            out
        }
    }
}

/// When the debugger pauses on exceptions (`pause_on_exceptions`).
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    ::serde::Deserialize,
    ::serde::Serialize,
    rust_mcp_sdk::macros::JsonSchema,
)]
pub enum PauseOnExceptions {
    // Each variant is renamed on its own: the schema derive ignores
    // `rename_all` and would list the Rust names (ISSUE-82).
    /// Never.
    #[default]
    #[serde(rename = "none")]
    None,
    /// On exceptions nothing catches (and unhandled promise rejections).
    #[serde(rename = "uncaught")]
    Uncaught,
    /// On every exception, caught or not.
    #[serde(rename = "all")]
    All,
}

impl PauseOnExceptions {
    /// Its name (Chrome's `Debugger.setPauseOnExceptions` state too).
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Uncaught => "uncaught",
            Self::All => "all",
        }
    }

    /// `pause_on_exceptions`' reply.
    pub(crate) fn reply(self) -> CallToolResult {
        let first = match self {
            Self::None => "The debugger does not pause on exceptions in this tab.",
            Self::Uncaught => "The debugger pauses on uncaught exceptions in this tab.",
            Self::All => "The debugger pauses on every exception, caught or not, in this tab.",
        };
        text(first, &json!({"pause_on_exceptions": self.as_str()}))
    }
}

/// `resume`'s reply (`paused` true when it stopped again right away).
pub(crate) fn resumed(location: Option<&PauseLocation>) -> CallToolResult {
    text("Debugger execution resumed.", &pause_json(location))
}

/// `resume`'s reply when the page was not paused.
pub(crate) fn nothing_to_resume() -> CallToolResult {
    text(NOTHING_TO_RESUME, &pause_json(None))
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

    /// ISSUE-87: a condition is sent as it is; a logpoint logs a label and
    /// the value and never pauses (only when the condition holds, with one).
    #[test]
    fn conditions_and_logpoints_are_breakpoint_conditions() {
        let label = "app.js:3";
        assert_eq!(breakpoint_condition(None, None, label), None);
        assert_eq!(
            breakpoint_condition(Some("n > 1"), None, label).as_deref(),
            Some("(\nn > 1\n)")
        );
        assert_eq!(
            breakpoint_condition(None, Some("sum"), label).as_deref(),
            Some("(console.log(\"logpoint app.js:3:\", (\nsum\n)), false)")
        );
        assert_eq!(
            breakpoint_condition(Some("n > 1"), Some("sum"), label).as_deref(),
            Some("((\nn > 1\n) && console.log(\"logpoint app.js:3:\", (\nsum\n)), false)")
        );
    }

    /// ISSUE-87: a relative URL names the loaded script whose URL ends with
    /// it (query and fragment aside); several are refused, naming them;
    /// none leaves it unresolved. Absolute URLs are kept.
    #[test]
    fn relative_urls_name_a_loaded_script() {
        let loaded = [
            "http://a/js/dbg67.js?v=2",
            "http://a/js/dbg67.js?v=2",
            "http://a/other.js",
            "http://a/lib/util.js",
            "http://b/util.js",
        ];
        let resolve = |url: &str| resolve_script_url(url, loaded.iter().copied());
        assert!(!is_absolute_url("dbg67.js") && is_absolute_url("http://a/x.js"));
        assert_eq!(
            resolve("dbg67.js"),
            Ok(Some("http://a/js/dbg67.js?v=2".to_string()))
        );
        assert_eq!(
            resolve("js/dbg67.js"),
            Ok(Some("http://a/js/dbg67.js?v=2".to_string()))
        );
        assert_eq!(
            resolve("./js/dbg67.js"),
            Ok(Some("http://a/js/dbg67.js?v=2".to_string()))
        );
        assert_eq!(resolve("ther.js"), Ok(None));
        assert_eq!(resolve("nope.js"), Ok(None));
        let several = resolve("util.js").unwrap_err();
        assert!(
            several.contains("http://a/lib/util.js") && several.contains("http://b/util.js"),
            "{several}"
        );
        assert_eq!(
            resolve("http://z/x.js"),
            Ok(Some("http://z/x.js".to_string()))
        );
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
            condition: None,
            log: None,
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

    /// ISSUE-67: the three steps word their reply alike on both browsers.
    #[test]
    fn steps_have_their_own_replies() {
        let first = |step: Step| {
            let reply = stepped(step, None);
            match &reply.content[0] {
                rust_mcp_sdk::schema::ContentBlock::TextContent(t) => {
                    t.text.lines().next().unwrap_or_default().to_string()
                }
                _ => String::new(),
            }
        };
        assert_eq!(
            first(Step::Over),
            "Stepped over to the next expression in the debugger."
        );
        assert_eq!(first(Step::Into), "Stepped into the call in the debugger.");
        assert_eq!(
            first(Step::Out),
            "Stepped out to the caller in the debugger."
        );
        assert_eq!(
            [Step::Over, Step::Into, Step::Out].map(Step::cdp_method),
            ["Debugger.stepOver", "Debugger.stepInto", "Debugger.stepOut"]
        );
    }

    /// ISSUE-67: the call stack, innermost first, with each frame's index
    /// (what `evaluate_on_call_frame`'s `frame_index` takes), name (or
    /// `(anonymous)`), 0-based place, the async callers, why it paused and
    /// the exception it paused on.
    #[test]
    fn call_stacks_list_their_frames() {
        let frame = |name: &str, line: u32| StackFrame {
            function_name: name.into(),
            location: PauseLocation {
                url: Some("http://a/cart.js".into()),
                script_id: None,
                line,
                column: 2,
            },
        };
        let stack = CallStack {
            reason: "exception".into(),
            exception: Some("TypeError: x is null".into()),
            frames: vec![frame("total", 3), frame("", 9)],
            async_parents: vec![AsyncSegment {
                description: "setTimeout".into(),
                frames: vec![frame("schedule", 20)],
            }],
            truncated: false,
        };
        assert_eq!(
            stack.to_json(),
            json!({"reason": "exception", "exception": "TypeError: x is null",
                "frames": [
                    {"index": 0, "function_name": "total", "url": "http://a/cart.js", "line_number": 3, "column_number": 2},
                    {"index": 1, "function_name": "(anonymous)", "url": "http://a/cart.js", "line_number": 9, "column_number": 2}],
                "async_callers": [{"description": "setTimeout", "frames": [
                    {"function_name": "schedule", "url": "http://a/cart.js", "line_number": 20, "column_number": 2}]}]})
        );
        let text = match &stack.reply().content[0] {
            rust_mcp_sdk::schema::ContentBlock::TextContent(t) => t.text.clone(),
            _ => String::new(),
        };
        assert!(text.starts_with("Paused (exception): 2 frame(s), innermost first; evaluate_on_call_frame takes a frame_index."), "{text}");
        assert_eq!(pause_reason("debuggerStatement"), "debugger_statement");
        assert_eq!(pause_reason("resumeLimit"), "step");
        assert_eq!(pause_reason("promiseRejection"), "promise_rejection");
    }

    /// ISSUE-67: the pause-on-exceptions modes and their reply.
    #[test]
    fn pause_on_exceptions_modes() {
        let mode: PauseOnExceptions = serde_json::from_value(json!("uncaught")).unwrap();
        assert_eq!(
            (mode.as_str(), PauseOnExceptions::default()),
            ("uncaught", PauseOnExceptions::None)
        );
        let text = match &PauseOnExceptions::All.reply().content[0] {
            rust_mcp_sdk::schema::ContentBlock::TextContent(t) => t.text.clone(),
            _ => String::new(),
        };
        assert!(
            text.starts_with("The debugger pauses on every exception, caught or not, in this tab."),
            "{text}"
        );
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
