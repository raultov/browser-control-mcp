//! Firefox RDP value grips as plain JSON (evaluation in a paused frame).

use crate::handler::evaluation::Evaluation;
use serde_json::{Map, Value};

/// A grip as the shared evaluation result (`evaluate_on_call_frame`), with
/// Chrome's `typeof` / `subtype` names.
pub(crate) fn evaluation(grip: &Value) -> Evaluation {
    let Some(object) = grip.as_object() else {
        return match grip {
            Value::Null => Evaluation::json("object", Some("null"), Value::Null),
            Value::String(_) => Evaluation::json("string", None, grip.clone()),
            Value::Bool(_) => Evaluation::json("boolean", None, grip.clone()),
            _ => Evaluation::json("number", None, grip.clone()),
        };
    };
    let text = |key: &str| object.get(key).and_then(Value::as_str).unwrap_or_default();
    match text("type") {
        "undefined" => Evaluation::json("undefined", None, Value::Null),
        "null" => Evaluation::json("object", Some("null"), Value::Null),
        special @ ("NaN" | "Infinity" | "-Infinity" | "-0") => {
            Evaluation::described("number", None, special)
        }
        "longString" => Evaluation::json("string", None, to_json(grip)),
        "symbol" => Evaluation::described("symbol", None, &format!("Symbol({})", text("name"))),
        "BigInt" => Evaluation::described("bigint", None, &format!("{}n", text("text"))),
        _ => object_evaluation(object, grip),
    }
}

/// An object grip: plain objects and arrays by their preview, the rest
/// described by their class.
fn object_evaluation(object: &Map<String, Value>, grip: &Value) -> Evaluation {
    let class = object
        .get("class")
        .and_then(Value::as_str)
        .unwrap_or("Object");
    let preview = object.get("preview");
    let kind = preview
        .and_then(|p| p.get("kind"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    match (class, kind) {
        ("Function", _) => {
            let name = object
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            Evaluation::described("function", None, format!("function {name}").trim_end())
        }
        ("Array", _) => Evaluation::json("object", Some("array"), to_json(grip)),
        ("Object", _) => Evaluation::json("object", None, to_json(grip)),
        (_, "DOMNode") => Evaluation::described("object", Some("node"), &node_description(preview)),
        (_, "Error") => {
            let field = |key: &str| preview.and_then(|p| p.get(key)).and_then(Value::as_str);
            let description = match (field("name"), field("message")) {
                (Some(name), Some(message)) => format!("{name}: {message}"),
                _ => class.to_string(),
            };
            Evaluation::described("object", Some("error"), &description)
        }
        _ => {
            let subtype = match class {
                "Date" => Some("date"),
                "RegExp" => Some("regexp"),
                "Map" => Some("map"),
                "Set" => Some("set"),
                "Promise" => Some("promise"),
                _ => None,
            };
            Evaluation::described("object", subtype, class)
        }
    }
}

/// `div#id.class`, as Chrome describes an element.
fn node_description(preview: Option<&Value>) -> String {
    let attribute = |key: &str| {
        preview
            .and_then(|p| p.get("attributes"))
            .and_then(|a| a.get(key))
            .and_then(Value::as_str)
            .unwrap_or_default()
    };
    let name = preview
        .and_then(|p| p.get("nodeName"))
        .and_then(Value::as_str)
        .unwrap_or("node")
        .to_lowercase();
    let mut description = name;
    if !attribute("id").is_empty() {
        description = format!("{description}#{}", attribute("id"));
    }
    for class in attribute("class").split_whitespace() {
        description = format!("{description}.{class}");
    }
    description
}

/// What an `evaluationResult` packet says was thrown.
pub(crate) fn thrown(raw: &Value, exception: &Value) -> String {
    if let Some(message) = raw.get("exceptionMessage").and_then(Value::as_str) {
        return message
            .strip_prefix("uncaught exception: ")
            .unwrap_or(message)
            .to_string();
    }
    match evaluation(exception) {
        Evaluation {
            description: Some(description),
            ..
        } => description,
        evaluation => match evaluation.value {
            Value::String(s) => s,
            other => other.to_string(),
        },
    }
}

/// Converts a grip (spike H18) into plain JSON: primitives as they are,
/// `undefined`/`null` grips as `null`, objects from their preview, long
/// strings as their first characters with the total length.
pub fn to_json(grip: &Value) -> Value {
    let Some(object) = grip.as_object() else {
        return grip.clone();
    };
    match object.get("type").and_then(Value::as_str) {
        Some("undefined" | "null") => Value::Null,
        Some("NaN") => Value::String("NaN".into()),
        Some("Infinity") => Value::String("Infinity".into()),
        Some("-Infinity") => Value::String("-Infinity".into()),
        Some("longString") => {
            let initial = object
                .get("initial")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let length = object.get("length").and_then(Value::as_u64).unwrap_or(0);
            Value::String(format!("{initial}…({length} chars)"))
        }
        Some("object") => from_preview(object),
        _ => grip.clone(),
    }
}

fn from_preview(object: &Map<String, Value>) -> Value {
    let preview = object.get("preview");
    if let Some(items) = preview
        .and_then(|p| p.get("items"))
        .and_then(Value::as_array)
    {
        return Value::Array(items.iter().map(to_json).collect());
    }
    if let Some(props) = preview
        .and_then(|p| p.get("ownProperties"))
        .and_then(Value::as_object)
    {
        let map = props
            .iter()
            .map(|(k, v)| (k.clone(), to_json(v.get("value").unwrap_or(v))))
            .collect();
        return Value::Object(map);
    }
    let class = object
        .get("class")
        .and_then(Value::as_str)
        .unwrap_or("Object");
    Value::String(format!("[object {class}]"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// DBG-02 / EVL (Firefox frames): grips in the shared evaluation shape.
    #[test]
    fn grips_become_evaluations() {
        let rows = [
            (json!(11), json!({"value": 11, "type": "number"})),
            (
                json!({"type": "undefined"}),
                json!({"value": null, "type": "undefined"}),
            ),
            (
                json!({"type": "NaN"}),
                json!({"value": null, "type": "number", "description": "NaN"}),
            ),
            (
                json!({"type": "BigInt", "text": "10"}),
                json!({"value": null, "type": "bigint", "description": "10n"}),
            ),
            (
                json!({"type": "object", "class": "Array", "preview": {"items": [1, 2]}}),
                json!({"value": [1, 2], "type": "object", "subtype": "array"}),
            ),
            (
                json!({"type": "object", "class": "Function", "name": "f"}),
                json!({"value": null, "type": "function", "description": "function f"}),
            ),
            (
                json!({"type": "object", "class": "HTMLDivElement", "preview": {"kind": "DOMNode", "nodeName": "DIV", "attributes": {"id": "t", "class": "a b"}}}),
                json!({"value": null, "type": "object", "subtype": "node", "description": "div#t.a.b"}),
            ),
            (
                json!({"type": "object", "class": "ReferenceError", "preview": {"kind": "Error", "name": "ReferenceError", "message": "y is not defined"}}),
                json!({"value": null, "type": "object", "subtype": "error", "description": "ReferenceError: y is not defined"}),
            ),
        ];
        for (grip, expected) in rows {
            assert_eq!(evaluation(&grip).to_json(), expected, "{grip}");
        }
    }

    /// DBG-05 (Firefox half): what was thrown, without RDP's prefix.
    #[test]
    fn thrown_reads_the_exception_message() {
        let raw = json!({"exceptionMessage": "ReferenceError: y is not defined"});
        assert_eq!(
            thrown(&raw, &Value::Null),
            "ReferenceError: y is not defined"
        );
        let raw = json!({"exceptionMessage": "uncaught exception: boom"});
        assert_eq!(thrown(&raw, &json!("boom")), "boom");
        assert_eq!(thrown(&json!({}), &json!("boom")), "boom");
    }

    /// RDP-08: Grips become plain JSON.
    #[test]
    fn grips_become_json() {
        let rows = [
            (json!(10), json!(10)),
            (json!({"type":"undefined"}), Value::Null),
            (
                json!({"type":"longString","initial":"abc","length":5000}),
                json!("abc…(5000 chars)"),
            ),
            (
                json!({"type":"object","class":"Object","preview":{"ownProperties":{"a":{"value":1}}}}),
                json!({"a":1}),
            ),
            (
                json!({"type":"object","class":"Array","preview":{"items":[1,2]}}),
                json!([1, 2]),
            ),
        ];
        for (grip, expected) in rows {
            assert_eq!(to_json(&grip), expected, "{grip}");
        }
    }
}
