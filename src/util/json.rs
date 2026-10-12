//! Reading fields of protocol messages without panicking.

use serde_json::Value;

/// A string field of a JSON object, if present.
pub fn str_field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

/// A string field, or `""` when absent (as the imported code did).
pub fn string_or_empty(value: &Value, key: &str) -> String {
    str_field(value, key).unwrap_or_default().to_string()
}

/// An owned optional string field.
pub fn opt_string(value: &Value, key: &str) -> Option<String> {
    str_field(value, key).map(str::to_string)
}
