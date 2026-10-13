//! The result of `evaluate_js` (and `evaluate_on_call_frame`), the same on
//! both browsers: `{value, type, subtype?, description?}`.
//!
//! `type` is JavaScript's `typeof`; `subtype` refines objects with Chrome's
//! names (`array`, `null`, `node`, `date`, `regexp`, `map`, `set`, `error`,
//! `promise`...). `value` is the JSON form of the result, `null` when it has
//! none (functions, DOM nodes, symbols, `NaN`, bigints, maps...), and then
//! `description` says what it was. A thrown exception is a tool error with
//! [`exception_message`]'s wording.

use rust_mcp_sdk::schema::CallToolResult;
use serde_json::{Value, json};

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
}

impl Evaluation {
    /// A value with a JSON form.
    pub(crate) fn json(kind: &str, subtype: Option<&str>, value: Value) -> Self {
        Self {
            value,
            kind: kind.to_string(),
            subtype: subtype.map(str::to_string),
            description: None,
        }
    }

    /// A value without a JSON form, shown by `description`.
    pub(crate) fn described(kind: &str, subtype: Option<&str>, description: &str) -> Self {
        Self {
            value: Value::Null,
            kind: kind.to_string(),
            subtype: subtype.map(str::to_string),
            description: Some(description.to_string()),
        }
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
        reply
    }

    /// The tool reply: the reply object as pretty JSON.
    pub(crate) fn reply(&self) -> CallToolResult {
        let body = serde_json::to_string_pretty(&self.to_json()).unwrap_or_default();
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

/// `true` for the `Runtime.evaluate` subtypes whose by-value form is the
/// object itself (other objects serialise to `{}` and are described).
pub(crate) fn has_json_form(subtype: Option<&str>) -> bool {
    matches!(subtype, None | Some("array"))
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
            Some(value) if has_json_form(subtype) => Evaluation::json(kind, subtype, value),
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
