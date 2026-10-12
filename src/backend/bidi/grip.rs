//! Firefox RDP value grips as plain JSON (evaluation in a paused frame).

use serde_json::{Map, Value};

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
