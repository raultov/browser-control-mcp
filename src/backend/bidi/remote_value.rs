//! WebDriver BiDi `RemoteValue`s as plain JSON, and as the shared
//! `evaluate_js` result (`handler::evaluation::Evaluation`), so
//! Firefox answers the same shape as Chrome.

use crate::handler::evaluation::Evaluation;
use serde_json::{Map, Value, json};

/// A `RemoteValue` as the shared evaluation result: Chrome's `typeof` /
/// `subtype` names, a JSON value when there is one, else a description.
pub(crate) fn evaluation(remote: &Value) -> Evaluation {
    let kind = remote
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("undefined");
    let value = remote.get("value");
    match kind {
        "undefined" => Evaluation::json(kind, None, Value::Null),
        "null" => Evaluation::json("object", Some("null"), Value::Null),
        "string" | "boolean" => Evaluation::json(kind, None, to_json(remote)),
        "number" => match value {
            Some(Value::String(special)) => Evaluation::described(kind, None, special),
            _ => Evaluation::json(kind, None, to_json(remote)),
        },
        "bigint" => {
            let digits = value.and_then(Value::as_str).unwrap_or_default();
            Evaluation::described(kind, None, &format!("{digits}n"))
        }
        "symbol" => Evaluation::described(kind, None, "Symbol()"),
        "function" => Evaluation::described(kind, None, "function"),
        "array" => match json_form(remote) {
            Some(json) => Evaluation::json("object", Some("array"), json),
            None => {
                let count = value.and_then(Value::as_array).map_or(0, Vec::len);
                Evaluation::described("object", Some("array"), &format!("Array({count})"))
            }
        },
        "object" => match json_form(remote) {
            Some(json) => Evaluation::json("object", None, json),
            None => Evaluation::described("object", None, "Object"),
        },
        "window" => Evaluation::described("object", None, "Window"),
        "node" => Evaluation::described("object", Some("node"), &node_description(value)),
        other => described_object(other, value),
    }
}

/// Objects without a JSON form, described the way Chrome describes them.
fn described_object(kind: &str, value: Option<&Value>) -> Evaluation {
    let count = || value.and_then(Value::as_array).map_or(0, Vec::len);
    let text = |key: &str| {
        value
            .and_then(|v| v.get(key))
            .and_then(Value::as_str)
            .unwrap_or_default()
    };
    let (subtype, description) = match kind {
        "date" => (
            "date",
            value.and_then(Value::as_str).unwrap_or("Date").to_string(),
        ),
        "regexp" => ("regexp", format!("/{}/{}", text("pattern"), text("flags"))),
        "map" => ("map", format!("Map({})", count())),
        "set" => ("set", format!("Set({})", count())),
        "nodelist" => ("array", format!("NodeList({})", count())),
        "htmlcollection" => ("array", format!("HTMLCollection({})", count())),
        "error" => ("error", "Error".to_string()),
        "weakmap" => ("weakmap", "WeakMap".to_string()),
        "weakset" => ("weakset", "WeakSet".to_string()),
        "arraybuffer" => ("arraybuffer", "ArrayBuffer".to_string()),
        "typedarray" => ("typedarray", "TypedArray".to_string()),
        other => (other, capitalised(other)),
    };
    Evaluation::described("object", Some(subtype), &description)
}

/// `div#id.class1.class2`, as Chrome describes an element.
fn node_description(value: Option<&Value>) -> String {
    let field = |key: &str| value.and_then(|v| v.get(key)).and_then(Value::as_str);
    let attribute = |key: &str| {
        value
            .and_then(|v| v.get("attributes"))
            .and_then(|a| a.get(key))
            .and_then(Value::as_str)
    };
    let mut description = field("localName").unwrap_or("node").to_string();
    if let Some(id) = attribute("id").filter(|id| !id.is_empty()) {
        description.push('#');
        description.push_str(id);
    }
    for class in attribute("class").unwrap_or_default().split_whitespace() {
        description.push('.');
        description.push_str(class);
    }
    description
}

fn capitalised(word: &str) -> String {
    let mut chars = word.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

/// Converts a `RemoteValue` (spike H3) into plain JSON (`null` when it has
/// no JSON form, see `json_form`).
pub fn to_json(remote: &Value) -> Value {
    json_form(remote).unwrap_or_default()
}

/// A `RemoteValue` as JSON, following `JSON.stringify` like Chrome's
/// by-value results: `NaN` and `±Infinity` are `null`, `-0` is `0`,
/// `undefined` is left out of objects (`null` in arrays). A BigInt or a
/// cycle anywhere inside means there is no JSON form (`None`), rather than
/// a number turned into a string or a cycle turned into `{}`. Values JSON
/// cannot hold keep a small descriptive object (`{"type": "node",
/// "sharedId": ..}`).
pub(crate) fn json_form(remote: &Value) -> Option<Value> {
    Converter::default().convert(remote)
}

/// Objects converted so far, by BiDi `internalId`: BiDi sends an object's
/// value only the first time it is reached, so a later reference is a copy
/// of it, or a cycle when it is reached from inside itself (not done yet).
#[derive(Default)]
struct Converter {
    done: std::collections::HashMap<String, Value>,
}

impl Converter {
    fn convert(&mut self, remote: &Value) -> Option<Value> {
        let value = remote.get("value");
        let internal = remote.get("internalId").and_then(Value::as_str);
        if let (Some(id), None) = (internal, value) {
            return self.done.get(id).cloned();
        }
        let json = match remote.get("type").and_then(Value::as_str) {
            Some("undefined" | "null") | None => Some(Value::Null),
            Some("string" | "boolean" | "date" | "regexp") => value.cloned().or(Some(Value::Null)),
            Some("number") => Some(number(value)),
            Some("bigint") => None,
            Some("array" | "set") => self.items(value).map(Value::Array),
            Some("object" | "map") => self.entries(value).map(Value::Object),
            Some(other) => Some(opaque(other, remote)),
        };
        if let (Some(id), Some(json)) = (internal, &json) {
            self.done.insert(id.to_string(), json.clone());
        }
        json
    }

    fn items(&mut self, value: Option<&Value>) -> Option<Vec<Value>> {
        let list = value.and_then(Value::as_array).cloned().unwrap_or_default();
        list.iter().map(|item| self.convert(item)).collect()
    }

    /// `[[key, value], ...]` pairs; a key is a string or a `RemoteValue`.
    fn entries(&mut self, value: Option<&Value>) -> Option<Map<String, Value>> {
        let pairs = value.and_then(Value::as_array).cloned().unwrap_or_default();
        let mut map = Map::new();
        for pair in &pairs {
            let (Some(key), Some(item)) = (pair.get(0), pair.get(1)) else {
                continue;
            };
            if item.get("type").and_then(Value::as_str) == Some("undefined") {
                continue;
            }
            let key = match key.as_str() {
                Some(key) => key.to_string(),
                None => match self.convert(key)? {
                    Value::String(s) => s,
                    other => other.to_string(),
                },
            };
            map.insert(key, self.convert(item)?);
        }
        Some(map)
    }
}

/// A BiDi number: special values travel as strings (`"NaN"`, `"-0"`,
/// `"Infinity"`, `"-Infinity"`); JSON has `0` for `-0` and nothing else.
fn number(value: Option<&Value>) -> Value {
    match value {
        Some(Value::String(special)) if special == "-0" => json!(0),
        Some(Value::Number(n)) => Value::Number(n.clone()),
        _ => Value::Null,
    }
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
            (json!({"type":"number","value":"NaN"}), Value::Null),
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

    /// EVL-01 / EVL-02 / EVL-03 (Firefox): the same shape as on Chrome.
    #[test]
    fn remote_values_become_evaluations() {
        let reply: Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/bidi/script-evaluate-values.json"
        ))
        .unwrap();
        let values = reply["result"]["result"]["value"].as_array().unwrap();
        let described: Vec<Value> = values.iter().map(|v| evaluation(v).to_json()).collect();
        let expected = [
            json!({"value": null, "type": "function", "description": "function"}),
            json!({"value": null, "type": "symbol", "description": "Symbol()"}),
            json!({"value": null, "type": "bigint", "description": "10n"}),
            json!({"value": null, "type": "number", "description": "NaN"}),
            json!({"value": null, "type": "number", "description": "-0"}),
            json!({"value": null, "type": "object", "subtype": "date", "description": "1970-01-01T00:00:00.000Z"}),
            json!({"value": null, "type": "object", "subtype": "regexp", "description": "/a/g"}),
            json!({"value": null, "type": "object", "subtype": "map", "description": "Map(1)"}),
            json!({"value": null, "type": "object", "subtype": "error", "description": "Error"}),
            json!({"value": null, "type": "object", "subtype": "promise", "description": "Promise"}),
            json!({"value": null, "type": "object", "description": "Window"}),
            json!({"value": null, "type": "object", "subtype": "set", "description": "Set(1)"}),
        ];
        assert_eq!(described, expected);
        // And values with a JSON form keep it
        let node: Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/bidi/script-evaluate-node.json"
        ))
        .unwrap();
        assert_eq!(
            evaluation(&node["result"]["result"]).to_json(),
            json!({"value": null, "type": "object", "subtype": "node", "description": "button#go"})
        );
        let rows = [
            (
                json!({"type": "number", "value": 3}),
                json!({"value": 3, "type": "number"}),
            ),
            (
                json!({"type": "undefined"}),
                json!({"value": null, "type": "undefined"}),
            ),
            (
                json!({"type": "null"}),
                json!({"value": null, "type": "object", "subtype": "null"}),
            ),
            (
                json!({"type": "array", "value": [{"type": "string", "value": "x"}]}),
                json!({"value": ["x"], "type": "object", "subtype": "array"}),
            ),
        ];
        for (remote, expected) in rows {
            assert_eq!(evaluation(&remote).to_json(), expected, "{remote}");
        }
    }

    /// AUD-EV-01: nested values follow JSON, as Chrome's by-value results
    /// do: numbers stay numbers (NaN and ±Infinity become null, -0 is 0),
    /// undefined is left out of objects and null in arrays.
    #[test]
    fn nested_special_values_follow_json() {
        let number = |v: Value| json!({"type": "number", "value": v});
        let remote = json!({"type": "object", "value": [
            ["a", number(json!("NaN"))],
            ["b", number(json!("-0"))],
            ["c", number(json!("Infinity"))],
            ["d", {"type": "undefined"}],
            ["e", {"type": "array", "value": [{"type": "undefined"}, number(json!(1))]}],
        ]});
        assert_eq!(
            evaluation(&remote).to_json(),
            json!({"value": {"a": null, "b": 0, "c": null, "e": [null, 1]}, "type": "object"})
        );
    }

    /// AUD-EV-01: a nested BigInt or a cycle leaves the object without a
    /// JSON form, described as on Chrome, instead of a string or `{}`.
    #[test]
    fn objects_without_json_form_are_described() {
        let bigint = json!({"type": "object", "value": [["b", {"type": "bigint", "value": "10"}]]});
        let cycle = json!({"type": "object", "internalId": "1", "value": [
            ["a", {"type": "number", "value": 1}],
            ["self", {"type": "object", "internalId": "1"}],
        ]});
        let list = json!({"type": "array", "value": [{"type": "bigint", "value": "1"}]});
        let described = json!({"value": null, "type": "object", "description": "Object"});
        assert_eq!(evaluation(&bigint).to_json(), described);
        assert_eq!(evaluation(&cycle).to_json(), described);
        assert_eq!(
            evaluation(&list).to_json(),
            json!({"value": null, "type": "object", "subtype": "array", "description": "Array(1)"})
        );
    }

    /// AUD-EV-01: an object reached twice (not a cycle) is serialised both
    /// times; BiDi sends its value only the first time.
    #[test]
    fn shared_references_are_resolved() {
        let shared = json!({"type": "object", "value": [
            ["a", {"type": "object", "internalId": "7", "value": [["x", {"type": "number", "value": 1}]]}],
            ["b", {"type": "object", "internalId": "7"}],
        ]});
        assert_eq!(to_json(&shared), json!({"a": {"x": 1}, "b": {"x": 1}}));
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
