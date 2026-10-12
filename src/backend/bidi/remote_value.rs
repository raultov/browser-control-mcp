//! WebDriver BiDi `RemoteValue`s as plain JSON, so `evaluate_js` answers the
//! same shape on Firefox as on Chrome (`returnByValue`).

use serde_json::{Map, Value, json};

/// Converts a `RemoteValue` (spike H3) into plain JSON. Values without a JSON
/// form keep a small descriptive object (`{"type": "node", "sharedId": ..}`).
pub fn to_json(remote: &Value) -> Value {
    let value = remote.get("value");
    match remote.get("type").and_then(Value::as_str) {
        Some("undefined" | "null") => Value::Null,
        Some("string" | "boolean" | "bigint") => value.cloned().unwrap_or(Value::Null),
        Some("number") => value.cloned().unwrap_or(Value::Null),
        Some("array" | "set") => Value::Array(items(value)),
        Some("object") => Value::Object(entries(value)),
        Some("map") => Value::Object(entries(value)),
        Some("date" | "regexp") => value.cloned().unwrap_or(Value::Null),
        Some(other) => opaque(other, remote),
        None => Value::Null,
    }
}

fn items(value: Option<&Value>) -> Vec<Value> {
    value
        .and_then(Value::as_array)
        .map(|list| list.iter().map(to_json).collect())
        .unwrap_or_default()
}

/// `[[key, value], ...]` pairs; a key is a string or a `RemoteValue`.
fn entries(value: Option<&Value>) -> Map<String, Value> {
    let pairs = value.and_then(Value::as_array).cloned().unwrap_or_default();
    pairs
        .iter()
        .filter_map(|pair| {
            let key = pair.get(0)?;
            let key = match key.as_str() {
                Some(key) => key.to_string(),
                None => match to_json(key) {
                    Value::String(s) => s,
                    other => other.to_string(),
                },
            };
            Some((key, to_json(pair.get(1)?)))
        })
        .collect()
}

fn opaque(kind: &str, remote: &Value) -> Value {
    let mut out = json!({ "type": kind });
    if let (Some(map), Some(shared)) = (out.as_object_mut(), remote.get("sharedId")) {
        map.insert("sharedId".into(), shared.clone());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// BID-14: RemoteValues become plain JSON.
    #[test]
    fn remote_values_become_json() {
        let rows = [
            (json!({"type":"number","value":3}), json!(3)),
            (json!({"type":"string","value":"hi"}), json!("hi")),
            (json!({"type":"undefined"}), Value::Null),
            (json!({"type":"number","value":"NaN"}), json!("NaN")),
            (
                json!({"type":"array","value":[{"type":"boolean","value":true}]}),
                json!([true]),
            ),
            (
                json!({"type":"object","value":[["a",{"type":"null"}]]}),
                json!({"a": null}),
            ),
            (
                json!({"type":"map","value":[[{"type":"number","value":1},{"type":"string","value":"x"}]]}),
                json!({"1": "x"}),
            ),
            (
                json!({"type":"node","sharedId":"n1"}),
                json!({"type":"node","sharedId":"n1"}),
            ),
        ];
        for (remote, expected) in rows {
            assert_eq!(to_json(&remote), expected, "{remote}");
        }
    }

    #[test]
    fn real_object_fixture_becomes_json() {
        let reply: Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/bidi/script-evaluate-object.json"
        ))
        .unwrap();
        let value = to_json(&reply["result"]["result"]);
        assert_eq!(
            value,
            json!({"sum": 3, "list": [1, "a", null], "nested": {"b": true}})
        );
    }
}
