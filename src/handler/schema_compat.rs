/// Schema compatibility layer for Gemini/strict tool-calling engines.
use rust_mcp_sdk::schema::Tool;
use serde_json::{Map, Value};

/// Returns `tool` with every property schema normalized and cookie import properties filtered if disabled.
pub(crate) fn normalize_tool_with_options(mut tool: Tool, allow_cookie_import: bool) -> Tool {
    if !allow_cookie_import {
        filter_cookie_import(&mut tool);
    }
    if let Some(properties) = tool.input_schema.properties.as_mut() {
        for property in properties.values_mut() {
            normalize_object(property);
        }
    }
    drop_shared_descriptions(&mut tool);
    tool
}

/// Arguments that mean the same on every tool, which the server's
/// instructions say once. A tool's description of one that only repeats
/// that meaning is left out of tools/list, which is in the model's context
/// on every turn (CTX-01); one that says more (switch_tab's tab_id) is kept.
const SHARED_ARGUMENTS: [(&str, &str); 2] = [
    ("instance_id", "Default: 'default'."),
    ("tab_id", "Default: the active tab."),
];

fn drop_shared_descriptions(tool: &mut Tool) {
    let Some(properties) = tool.input_schema.properties.as_mut() else {
        return;
    };
    for (name, shared) in SHARED_ARGUMENTS {
        if let Some(property) = properties.get_mut(name)
            && property.get("description").and_then(Value::as_str) == Some(shared)
        {
            property.remove("description");
        }
    }
}

/// The arguments tools/list hides without `--allow-cookie-import`.
pub(crate) const COOKIE_IMPORT_ARGS: [&str; 3] =
    ["copy_cookies", "source_profile", "confirm_restart"];

fn filter_cookie_import(tool: &mut Tool) {
    if let Some(properties) = tool.input_schema.properties.as_mut() {
        for name in COOKIE_IMPORT_ARGS {
            properties.remove(name);
        }
    }
    tool.input_schema
        .required
        .retain(|r| !COOKIE_IMPORT_ARGS.contains(&r.as_str()));
    if let Some(desc) = tool.description.as_mut()
        && let Some(idx) = desc.find(" Note: copy_cookies")
    {
        desc.truncate(idx);
    }
}

fn normalize_value(value: &mut Value) {
    match value {
        Value::Object(map) => normalize_object(map),
        Value::Array(entries) => entries.iter_mut().for_each(normalize_value),
        _ => {}
    }
}

/// Normalizes a schema object bottom-up.
fn normalize_object(map: &mut Map<String, Value>) {
    for child in map.values_mut() {
        normalize_value(child);
    }
    merge_variant_enums(map);
    collapse_nullable_any_of(map);
    collapse_nullable_type(map);
    fix_array_items_type(map);
}

/// `anyOf: [<schema>, {"type": "null"}]` (an optional value) becomes
/// `<schema>`; optionality is already expressed by `required`.
fn collapse_nullable_any_of(map: &mut Map<String, Value>) {
    let Some(Value::Array(variants)) = map.get("anyOf") else {
        return;
    };
    let is_null = |v: &Value| v.get("type") == Some(&Value::String("null".into()));
    let mut non_null = variants.iter().filter(|v| !is_null(v));
    let (Some(Value::Object(only)), None) = (non_null.next(), non_null.next()) else {
        return;
    };
    if variants.len() != 2 {
        return;
    }
    let only = only.clone();
    map.remove("anyOf");
    for (key, value) in only {
        map.entry(key).or_insert(value);
    }
}

/// Collapses a `oneOf`/`anyOf` list of single-value `enum` objects into one
/// string `enum`.
fn merge_variant_enums(map: &mut Map<String, Value>) {
    for keyword in ["oneOf", "anyOf"] {
        let Some(variants) = map.get(keyword).and_then(Value::as_array) else {
            continue;
        };
        if variants.is_empty() {
            continue;
        }

        let mut values = Vec::new();
        let all_plain_enums = variants.iter().all(|variant| {
            let Some(variant) = variant.as_object() else {
                return false;
            };
            let Some(entries) = variant.get("enum").and_then(Value::as_array) else {
                return false;
            };
            if variant.len() != 1 || !entries.iter().all(Value::is_string) {
                return false;
            }
            values.extend(entries.iter().cloned());
            true
        });
        if !all_plain_enums {
            continue;
        }

        map.remove(keyword);
        map.insert("type".to_string(), Value::String("string".to_string()));
        map.insert("enum".to_string(), Value::Array(values));
    }
}

/// Rewrites `"type": ["x", "null"]` as `"type": "x"`.
fn collapse_nullable_type(map: &mut Map<String, Value>) {
    let Some(types) = map.get("type").and_then(Value::as_array) else {
        return;
    };
    let Some(concrete) = types
        .iter()
        .filter_map(Value::as_str)
        .find(|name| *name != "null")
        .map(str::to_string)
    else {
        return;
    };
    map.insert("type".to_string(), Value::String(concrete));
}

/// If a map has an `"items"` field, ensures it has `"type": "array"`.
/// Also, if `items` is an object, ensures it has a `"type"` property (e.g. `"type": "string"` for enums).
fn fix_array_items_type(map: &mut Map<String, Value>) {
    if map.contains_key("items") {
        if let Some(Value::String(t)) = map.get("type") {
            if t != "array" {
                map.insert("type".to_string(), Value::String("array".to_string()));
            }
        } else if !map.contains_key("type") {
            map.insert("type".to_string(), Value::String("array".to_string()));
        }

        // Check if the items object itself is an enum without a type, and set it to string
        if let Some(Value::Object(items_map)) = map.get_mut("items")
            && items_map.contains_key("enum")
            && !items_map.contains_key("type")
        {
            items_map.insert("type".to_string(), Value::String("string".to_string()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn normalize(schema: Value) -> Value {
        let mut map = schema
            .as_object()
            .expect("schema must be an object")
            .clone();
        normalize_object(&mut map);
        Value::Object(map)
    }

    /// The description `tools/list` gives `argument` of `tool`.
    fn listed_description(tool: Tool, argument: &str) -> Option<String> {
        let tool = normalize_tool_with_options(tool, false);
        let properties = tool.input_schema.properties.unwrap_or_default();
        properties
            .get(argument)
            .and_then(|p| p.get("description"))
            .and_then(Value::as_str)
            .map(str::to_string)
    }

    /// CTX-03: an argument the instructions explain once (instance_id,
    /// tab_id) is not described again on every tool, unless its
    /// description says more than the shared meaning.
    #[test]
    fn shared_arguments_are_described_once() {
        // Given navigate, whose instance_id and tab_id mean what they mean everywhere
        let navigate = crate::handler::cdp_domains::page::navigate::NavigateTool::tool;
        // Then tools/list gives them no description of their own
        assert_eq!(listed_description(navigate(), "instance_id"), None);
        assert_eq!(listed_description(navigate(), "tab_id"), None);
        // And an argument of navigate that is not shared keeps its own
        assert!(listed_description(navigate(), "url").is_some());
        // Given switch_tab, whose tab_id is the tab to switch to
        let switch = crate::handler::chrome_instance::switch_tab::SwitchTabTool::tool();
        // Then that description is kept
        let described = listed_description(switch, "tab_id").unwrap_or_default();
        assert!(described.contains("tab-1"), "{described}");
    }

    #[test]
    fn test_normalize_tool_wrapper() {
        let tool = crate::handler::cdp_domains::page::navigate::NavigateTool::tool();
        let _ = normalize_tool_with_options(tool, false);
    }

    #[test]
    fn given_nullable_enum_array_when_normalizing_then_it_becomes_a_plain_typed_array() {
        let normalized = normalize(json!({
            "description": "presets",
            "items": {
                "oneOf": [{ "enum": ["WEBGL_SOFTWARE"] }, { "enum": ["OTHER_PRESET"] }]
            },
            "type": ["array", "null"]
        }));

        assert_eq!(
            normalized,
            json!({
                "description": "presets",
                "items": { "type": "string", "enum": ["WEBGL_SOFTWARE", "OTHER_PRESET"] },
                "type": "array"
            })
        );
    }

    #[test]
    fn given_nullable_scalar_when_normalizing_then_the_null_member_is_dropped() {
        let normalized = normalize(json!({ "type": ["string", "null"] }));
        assert_eq!(normalized, json!({ "type": "string" }));
    }

    #[test]
    fn given_a_scalar_type_when_normalizing_then_it_is_left_untouched() {
        let normalized = normalize(json!({ "type": "boolean" }));
        assert_eq!(normalized, json!({ "type": "boolean" }));
    }

    #[test]
    fn given_a_null_only_type_when_normalizing_then_it_is_left_untouched() {
        let normalized = normalize(json!({ "type": ["null"] }));
        assert_eq!(normalized, json!({ "type": ["null"] }));
    }

    #[test]
    fn given_a_one_of_with_richer_variants_when_normalizing_then_it_is_left_untouched() {
        let schema = json!({
            "oneOf": [
                { "type": "string" },
                { "enum": ["a"], "description": "not a bare variant" }
            ]
        });
        assert_eq!(normalize(schema.clone()), schema);
    }

    #[test]
    fn given_nested_object_properties_when_normalizing_then_they_are_rewritten_too() {
        let normalized = normalize(json!({
            "type": ["object", "null"],
            "properties": { "inner": { "type": ["integer", "null"] } }
        }));

        assert_eq!(
            normalized,
            json!({
                "type": "object",
                "properties": { "inner": { "type": "integer" } }
            })
        );
    }
}
