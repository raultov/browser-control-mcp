//! The result of `evaluate_js` (and `evaluate_on_call_frame`), the same on
//! both browsers: `{value, type, subtype?, description?}`.
//!
//! `type` is JavaScript's `typeof`; `subtype` refines objects with Chrome's
//! names (`array`, `null`, `node`, `date`, `regexp`, `map`, `set`, `error`,
//! `promise`...). `value` is the JSON form of the result, `null` when it has
//! none (functions, DOM nodes, symbols, `NaN`, bigints, maps...), and then
//! `description` says what it was. A thrown exception is a tool error with
//! [`exception_message`]'s wording. A result over [`EVAL_MAX_CHARS`] is cut
//! (strings) or described (objects) and marked `truncated: true`.

use rust_mcp_sdk::schema::CallToolResult;
use serde_json::{Value, json};

/// Most characters a result's `value` may take in a reply: larger results
/// flood the agent (and over 256 MB drop the browser connection).
pub(crate) const EVAL_MAX_CHARS: usize = 100_000;

/// How long Chrome lets an evaluation run before stopping it (ms), below
/// the 10 s command timeout: a script still running then (an endless loop)
/// kept the page's thread busy, and every later call on the tab failed.
pub(crate) const EVAL_TIMEOUT_MS: u64 = 8_000;

/// Why an evaluation Chrome stopped after [`EVAL_TIMEOUT_MS`] failed.
pub(crate) const STOPPED: &str =
    "the script ran for more than 8 s (an endless loop?) and was stopped; the page keeps working";

/// Why an evaluation got no result before the command timeout: no script
/// runs any more (Chrome stops those), so it waits for something.
pub(crate) const NO_RESULT: &str = "no result after 10 s: the expression waits for a promise that never settles; the page keeps working";

/// The marker [`MEASURE`] answers for an object over the limit.
const TOO_LARGE: &str = "__bcm_too_large__";

/// Called on an object result with [`EVAL_MAX_CHARS`] as argument: the
/// object itself, or `{TOO_LARGE: length}` when its JSON form is longer, so
/// a large object never crosses the connection (read it with
/// [`measured_length`]).
pub(crate) const MEASURE: &str = "function (limit) { let json; try { json = JSON.stringify(this); } catch (e) { return this; } \
     return json !== undefined && json.length > limit ? { __bcm_too_large__: json.length } : this; }";

/// The JSON length [`MEASURE`] reported, when the object was over the limit.
pub(crate) fn measured_length(by_value: &Value) -> Option<u64> {
    by_value.get(TOO_LARGE).and_then(Value::as_u64)
}

/// An evaluation result in the shared shape.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct Evaluation {
    /// The JSON form, or `null` when the value has none.
    pub(crate) value: Value,
    /// `typeof` the value.
    pub(crate) kind: String,
    /// What kind of object, for objects.
    pub(crate) subtype: Option<String>,
    /// What the value was, when `value` cannot show it.
    pub(crate) description: Option<String>,
    /// `value` was cut or dropped for size (see [`EVAL_MAX_CHARS`]).
    pub(crate) truncated: bool,
}

impl Evaluation {
    /// A value with a JSON form.
    pub(crate) fn json(kind: &str, subtype: Option<&str>, value: Value) -> Self {
        Self {
            value,
            kind: kind.to_string(),
            subtype: subtype.map(str::to_string),
            description: None,
            truncated: false,
        }
    }

    /// A value without a JSON form, shown by `description`.
    pub(crate) fn described(kind: &str, subtype: Option<&str>, description: &str) -> Self {
        Self {
            value: Value::Null,
            kind: kind.to_string(),
            subtype: subtype.map(str::to_string),
            description: Some(description.to_string()),
            truncated: false,
        }
    }

    /// An object whose JSON form (`chars` characters) is over the limit.
    pub(crate) fn too_large(kind: &str, subtype: Option<&str>, chars: u64) -> Self {
        Self {
            truncated: true,
            ..Self::described(
                kind,
                subtype,
                &format!(
                    "too large to return ({chars} characters of JSON); read parts of it with a narrower expression"
                ),
            )
        }
    }

    /// The result within [`EVAL_MAX_CHARS`]: a long string keeps its start,
    /// a larger JSON form is dropped for a description.
    pub(crate) fn bounded(&self) -> Self {
        if let Value::String(text) = &self.value {
            let length = text.chars().count();
            if length <= EVAL_MAX_CHARS {
                return self.clone();
            }
            return Self {
                value: Value::String(text.chars().take(EVAL_MAX_CHARS).collect()),
                description: Some(format!(
                    "string of {length} characters; value holds the first {EVAL_MAX_CHARS}"
                )),
                truncated: true,
                ..self.clone()
            };
        }
        let chars = self.value.to_string().chars().count();
        if chars <= EVAL_MAX_CHARS {
            return self.clone();
        }
        let chars = u64::try_from(chars).unwrap_or(u64::MAX);
        Self::too_large(&self.kind, self.subtype.as_deref(), chars)
    }

    /// The reply object.
    pub(crate) fn to_json(&self) -> Value {
        let mut reply = json!({"value": self.value, "type": self.kind});
        if let Some(subtype) = &self.subtype {
            reply["subtype"] = json!(subtype);
        }
        if let Some(description) = &self.description {
            reply["description"] = json!(description);
        }
        if self.truncated {
            reply["truncated"] = json!(true);
        }
        reply
    }

    /// The tool reply: the bounded reply object as pretty JSON.
    pub(crate) fn reply(&self) -> CallToolResult {
        let body = serde_json::to_string_pretty(&self.bounded().to_json()).unwrap_or_default();
        CallToolResult::text_content(vec![body.into()])
    }
}

/// The error text of a thrown exception: what was thrown (first line of an
/// error's description, or the thrown value) and its 1-based position.
pub(crate) fn exception_message(thrown: &str, line: i64, column: i64) -> String {
    let line = line.saturating_add(1);
    let column = column.saturating_add(1);
    format!("{thrown} (line {line}, column {column})")
}

/// `true` for the remote objects whose by-value form is the object itself:
/// plain objects and real arrays. Others serialise to `{}` (or, for array
/// likes such as `NodeList`, to `{"0": {}, ...}`) and are described.
pub(crate) fn has_json_form(remote: &Value) -> bool {
    match remote.get("subtype").and_then(Value::as_str) {
        None => true,
        Some("array") => remote.get("className").and_then(Value::as_str) == Some("Array"),
        Some(_) => false,
    }
}

/// A CDP `RemoteObject` (from `returnByValue: false`) in the shared shape;
/// `by_value` is its by-value form when it was fetched.
pub(crate) fn from_cdp(remote: &Value, by_value: Option<Value>) -> Evaluation {
    let text = |key: &str| remote.get(key).and_then(Value::as_str);
    let kind = text("type").unwrap_or("undefined");
    let subtype = text("subtype");
    let description = text("unserializableValue")
        .or(text("description"))
        .unwrap_or(kind);
    match (kind, subtype) {
        ("undefined", _) | ("object", Some("null")) => Evaluation::json(kind, subtype, Value::Null),
        ("object", _) => match by_value {
            Some(value) if has_json_form(remote) => Evaluation::json(kind, subtype, value),
            _ => Evaluation::described(kind, subtype, description),
        },
        _ => match remote.get("value") {
            Some(value) if text("unserializableValue").is_none() => {
                Evaluation::json(kind, subtype, value.clone())
            }
            _ => Evaluation::described(kind, subtype, description),
        },
    }
}

/// The error text of CDP `exceptionDetails` (`evaluate_js`).
pub(crate) fn cdp_exception(details: &Value) -> String {
    exception_message(
        &cdp_thrown(details),
        details["lineNumber"].as_i64().unwrap_or(0),
        details["columnNumber"].as_i64().unwrap_or(0),
    )
}

/// What CDP `exceptionDetails` says was thrown: the first line of an
/// error's description, else the thrown value, else the summary text.
pub(crate) fn cdp_thrown(details: &Value) -> String {
    let exception = &details["exception"];
    exception
        .get("description")
        .and_then(Value::as_str)
        .and_then(|d| d.lines().next())
        .map(str::to_string)
        .or_else(|| exception.get("value").map(thrown_value))
        .unwrap_or_else(|| details["text"].as_str().unwrap_or("exception").to_string())
}

/// A thrown non-error value as text (a string as is, anything else as JSON).
fn thrown_value(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AUD-EV-12: a reply never carries more than `EVAL_MAX_CHARS`: a long
    /// string is cut and marked, a large object is described.
    #[test]
    fn large_results_are_bounded() {
        assert!(MEASURE.contains(TOO_LARGE));
        assert_eq!(measured_length(&json!({TOO_LARGE: 7})), Some(7));
        assert_eq!(measured_length(&json!({"a": 7})), None);
        let long = Evaluation::json("string", None, json!("x".repeat(EVAL_MAX_CHARS + 5)));
        let body = long.bounded().to_json();
        assert_eq!(body["value"].as_str().unwrap().len(), EVAL_MAX_CHARS);
        assert_eq!(body["truncated"], true);
        assert_eq!(
            body["description"],
            format!(
                "string of {} characters; value holds the first {EVAL_MAX_CHARS}",
                EVAL_MAX_CHARS + 5
            )
        );
        let big: Vec<u32> = (0..60_000).collect();
        let array = Evaluation::json("object", Some("array"), json!(big));
        let body = array.bounded().to_json();
        assert_eq!(body["value"], Value::Null);
        assert_eq!(body["truncated"], true);
        assert!(
            body["description"]
                .as_str()
                .unwrap()
                .starts_with("too large to return ("),
            "{body}"
        );
        let small = Evaluation::json("number", None, json!(3));
        assert_eq!(small.bounded(), small);
    }

    fn chrome(key: &str) -> Value {
        let all: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/cdp/runtime-evaluate-remote-objects.json"
        ))
        .unwrap();
        all[key].clone()
    }

    /// EVL-01 / EVL-02 / EVL-03 (Chrome): values with a JSON form keep it;
    /// the others are described.
    #[test]
    fn cdp_remote_objects_become_evaluations() {
        let by_value = chrome("object_by_value")["value"].clone();
        let rows = [
            (
                from_cdp(&chrome("string"), None),
                json!({"value": "hi", "type": "string"}),
            ),
            (
                from_cdp(&chrome("object"), Some(by_value.clone())),
                json!({"value": by_value, "type": "object"}),
            ),
            (
                from_cdp(&chrome("node"), None),
                json!({"value": null, "type": "object", "subtype": "node", "description": "div#t.a.b"}),
            ),
            (
                from_cdp(&chrome("function"), None),
                json!({"value": null, "type": "function", "description": "function f(a){return a}"}),
            ),
            (
                from_cdp(&chrome("window"), None),
                json!({"value": null, "type": "object", "description": "Window"}),
            ),
            (
                from_cdp(&chrome("nan"), None),
                json!({"value": null, "type": "number", "description": "NaN"}),
            ),
            (
                from_cdp(&chrome("bigint"), None),
                json!({"value": null, "type": "bigint", "description": "10n"}),
            ),
            (
                from_cdp(&chrome("symbol"), None),
                json!({"value": null, "type": "symbol", "description": "Symbol(s)"}),
            ),
            (
                from_cdp(&chrome("undefined"), None),
                json!({"value": null, "type": "undefined"}),
            ),
            (
                from_cdp(&chrome("null"), None),
                json!({"value": null, "type": "object", "subtype": "null"}),
            ),
        ];
        for (evaluation, expected) in rows {
            assert_eq!(evaluation.to_json(), expected);
        }
    }

    /// EVL-03 (Chrome): array-likes are described, not read by value.
    #[test]
    fn cdp_array_likes_are_described() {
        let node_list = json!({"type": "object", "subtype": "array", "className": "NodeList", "description": "NodeList(6)", "objectId": "o"});
        assert!(!has_json_form(&node_list));
        assert_eq!(
            from_cdp(&node_list, Some(json!({"0": {}}))).to_json(),
            json!({"value": null, "type": "object", "subtype": "array", "description": "NodeList(6)"})
        );
        assert!(has_json_form(&chrome("array")));
    }

    /// EVL-04 (Chrome): a thrown error or value is reported with what was
    /// thrown and its 1-based position.
    #[test]
    fn cdp_exceptions_name_what_was_thrown() {
        let error: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/cdp/runtime-evaluate-exception.json"
        ))
        .unwrap();
        assert_eq!(
            cdp_exception(&error["result"]["exceptionDetails"]),
            "ReferenceError: x_not_defined is not defined (line 1, column 1)"
        );
        let string: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/cdp/runtime-evaluate-throw-string.json"
        ))
        .unwrap();
        assert_eq!(
            cdp_exception(&string["result"]["exceptionDetails"]),
            "boom (line 1, column 7)"
        );
    }
}
