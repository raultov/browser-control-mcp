//! Page, runtime and input operations on one Firefox tab.

use super::protocol_error;
use super::remote_value;
use crate::backend::error::BackendError;
use bidi_browser_lite::{ReadinessState, Tab};
use serde_json::{Value, json};

/// Sends a command on the tab's session, mapping errors.
pub(crate) async fn command(tab: &Tab, method: &str, params: Value) -> Result<Value, BackendError> {
    tab.send_raw_command(method, params)
        .await
        .map(|reply| reply.result.unwrap_or(Value::Null))
        .map_err(|e| protocol_error(method, &e))
}

/// `browsingContext.navigate` waiting for `load` (spike H2): `{navigation, url}`.
pub(crate) async fn navigate(tab: &Tab, url: &str) -> Result<Value, BackendError> {
    tab.navigate(url, ReadinessState::Complete)
        .await
        .map(|reply| reply.result.unwrap_or(Value::Null))
        .map_err(|e| protocol_error("browsingContext.navigate", &e))
}

/// `browsingContext.reload` waiting for `load`.
pub(crate) async fn reload(tab: &Tab) -> Result<Value, BackendError> {
    tab.reload(ReadinessState::Complete)
        .await
        .map(|reply| reply.result.unwrap_or(Value::Null))
        .map_err(|e| protocol_error("browsingContext.reload", &e))
}

/// Raw `script.evaluate` reply (`type` success or exception).
async fn evaluate_raw(tab: &Tab, expression: &str, ownership: &str) -> Result<Value, BackendError> {
    let params = json!({
        "expression": expression,
        "target": tab.script_target(),
        "awaitPromise": true,
        "resultOwnership": ownership,
    });
    command(tab, "script.evaluate", params).await
}

/// Evaluates `expression`, returning its value as plain JSON; a thrown
/// exception is an error with its text and 1-based position (spike H4).
pub(crate) async fn evaluate(tab: &Tab, expression: &str) -> Result<Value, BackendError> {
    let reply = evaluate_raw(tab, expression, "none").await?;
    outcome(&reply)
}

/// The value of a `script.evaluate` reply, or its exception as an error.
pub(crate) fn outcome(reply: &Value) -> Result<Value, BackendError> {
    if reply.get("type").and_then(Value::as_str) == Some("exception") {
        let details = &reply["exceptionDetails"];
        let text = details
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("exception");
        let line = details
            .get("lineNumber")
            .and_then(Value::as_i64)
            .unwrap_or(0)
            + 1;
        let column = details
            .get("columnNumber")
            .and_then(Value::as_i64)
            .unwrap_or(0)
            + 1;
        return Err(BackendError::InvalidArgument(format!(
            "{text} (line {line}, column {column})"
        )));
    }
    Ok(remote_value::to_json(&reply["result"]))
}

/// `true`/`false`/value of a snippet built by `util::js`.
pub(crate) async fn run_snippet(tab: &Tab, expression: &str) -> Result<Value, BackendError> {
    evaluate(tab, expression).await
}

/// Clicks the first element matching `selector` with real pointer input
/// (spike H6); returns the viewport point reported for it.
pub(crate) async fn click(tab: &Tab, selector: &str) -> Result<(f64, f64), BackendError> {
    let not_found =
        || BackendError::InvalidArgument(format!("Element not found for selector: {selector}"));
    let center = run_snippet(tab, &crate::util::js::element_center(selector)).await?;
    let (Some(x), Some(y)) = (center["x"].as_f64(), center["y"].as_f64()) else {
        return Err(not_found());
    };
    let query = format!(
        "document.querySelector({})",
        crate::util::js::string_literal(selector)
    );
    let element = evaluate_raw(tab, &query, "root").await?;
    let shared = element
        .pointer("/result/sharedId")
        .cloned()
        .ok_or_else(not_found)?;
    let origin = json!({"type": "element", "element": {"sharedId": shared}});
    let pointer = json!({"type": "pointer", "id": "mouse", "parameters": {"pointerType": "mouse"}, "actions": [
        {"type": "pointerMove", "x": 0, "y": 0, "origin": origin},
        {"type": "pointerDown", "button": 0},
        {"type": "pointerUp", "button": 0},
    ]});
    let context = tab.context_id();
    command(
        tab,
        "input.performActions",
        json!({"context": context, "actions": [pointer]}),
    )
    .await?;
    command(tab, "input.releaseActions", json!({"context": context})).await?;
    if let Some(handle) = element.get("result").and_then(|r| r.get("handle")) {
        let disown = json!({"handles": [handle], "target": tab.script_target()});
        let _ = command(tab, "script.disown", disown).await;
    }
    Ok((x, y))
}

/// Focuses and clears the element, then types `text` key by key (spike H8).
pub(crate) async fn fill(tab: &Tab, selector: &str, text: &str) -> Result<(), BackendError> {
    let found = run_snippet(tab, &crate::util::js::focus_and_clear(selector)).await?;
    if found != Value::Bool(true) {
        return Err(BackendError::InvalidArgument(format!(
            "Element not found for selector: {selector}"
        )));
    }
    let keys: Vec<Value> = text
        .chars()
        .flat_map(|c| {
            let value = c.to_string();
            [
                json!({"type": "keyDown", "value": value}),
                json!({"type": "keyUp", "value": value}),
            ]
        })
        .collect();
    let actions = json!([{"type": "key", "id": "keyboard", "actions": keys}]);
    command(
        tab,
        "input.performActions",
        json!({"context": tab.context_id(), "actions": actions}),
    )
    .await?;
    command(
        tab,
        "input.releaseActions",
        json!({"context": tab.context_id()}),
    )
    .await?;
    Ok(())
}

/// What `capture_screenshot` asks for.
pub(crate) struct ShotRequest<'a> {
    /// `png` or `jpeg`.
    pub(crate) format: &'a str,
    /// 0–100, JPEG only.
    pub(crate) quality: Option<u32>,
    /// The whole document instead of the viewport.
    pub(crate) full_page: bool,
}

/// `browsingContext.captureScreenshot` params for a request (spike H7).
pub(crate) fn screenshot_params(context: &str, req: &ShotRequest<'_>) -> Value {
    let mut format = json!({"type": format!("image/{}", req.format)});
    if let (Some(q), Some(map)) = (req.quality, format.as_object_mut()) {
        map.insert("quality".into(), json!(f64::from(q.min(100)) / 100.0));
    }
    let origin = if req.full_page {
        "document"
    } else {
        "viewport"
    };
    json!({"context": context, "origin": origin, "format": format})
}

/// Base64 image data of the tab.
pub(crate) async fn screenshot(tab: &Tab, req: &ShotRequest<'_>) -> Result<String, BackendError> {
    let reply = command(
        tab,
        "browsingContext.captureScreenshot",
        screenshot_params(tab.context_id(), req),
    )
    .await?;
    reply["data"].as_str().map(str::to_string).ok_or_else(|| {
        BackendError::InvalidArgument(
            "No data returned from browsingContext.captureScreenshot".into(),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// BID-15: A script exception is a tool error with its text and position.
    #[test]
    fn script_exception_is_tool_error() {
        let reply: Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/bidi/script-evaluate-exception.json"
        ))
        .unwrap();
        let err = outcome(&reply["result"]).unwrap_err().to_string();
        assert_eq!(
            err,
            "ReferenceError: x_not_defined is not defined (line 1, column 1)"
        );
    }

    /// BID-18: capture_screenshot maps format, quality and origin.
    #[test]
    fn screenshot_maps_format_and_origin() {
        let png = screenshot_params(
            "C",
            &ShotRequest {
                format: "png",
                quality: None,
                full_page: false,
            },
        );
        assert_eq!(
            png,
            json!({"context": "C", "origin": "viewport", "format": {"type": "image/png"}})
        );
        let jpeg = screenshot_params(
            "C",
            &ShotRequest {
                format: "jpeg",
                quality: Some(80),
                full_page: true,
            },
        );
        assert_eq!(jpeg["origin"], "document");
        assert_eq!(
            jpeg["format"],
            json!({"type": "image/jpeg", "quality": 0.8})
        );
    }
}
