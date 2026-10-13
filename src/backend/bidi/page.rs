//! Page, runtime and input operations on one Firefox tab.

use super::protocol_error;
use super::remote_value;
use crate::backend::error::BackendError;
use crate::handler::evaluation::{Evaluation, exception_message};
use crate::handler::navigation::NavigationOutcome;
use bidi_browser_lite::{BidiError, ReadinessState, Tab};
use serde_json::{Value, json};

/// Sends a command on the tab's session, mapping errors.
pub(crate) async fn command(tab: &Tab, method: &str, params: Value) -> Result<Value, BackendError> {
    tab.send_raw_command(method, params)
        .await
        .map(|reply| reply.result.unwrap_or(Value::Null))
        .map_err(|e| protocol_error(method, &e))
}

/// `browsingContext.navigate` waiting for `load` (spike H2). A network
/// error is a navigation error with Firefox's code (`NS_ERROR_UNKNOWN_HOST`).
pub(crate) async fn navigate(tab: &Tab, url: &str) -> Result<NavigationOutcome, BackendError> {
    let reply = tab
        .navigate(url, ReadinessState::Complete)
        .await
        .map_err(|e| navigation_error("browsingContext.navigate", url, &e))?;
    Ok(settled(tab, reply.result.unwrap_or_else(|| json!({})), url).await)
}

/// `browsingContext.reload` waiting for `load`.
pub(crate) async fn reload(tab: &Tab) -> Result<NavigationOutcome, BackendError> {
    let reply = tab
        .reload(ReadinessState::Complete)
        .await
        .map_err(|e| navigation_error("browsingContext.reload", "", &e))?;
    let browser = reply.result.unwrap_or_else(|| json!({}));
    let requested = browser
        .get("url")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    Ok(settled(tab, browser, &requested).await)
}

/// A failed navigation: Firefox reports network errors as `unknown error`
/// with `Error: <code>` (captured from Firefox 157); anything else stays a
/// protocol error.
fn navigation_error(method: &str, url: &str, error: &BidiError) -> BackendError {
    match error {
        BidiError::ProtocolError { error, message, .. } if error == "unknown error" => {
            BackendError::Navigation {
                reason: message.trim_start_matches("Error: ").to_string(),
                url: url.to_string(),
            }
        }
        other => protocol_error(method, other),
    }
}

/// The outcome of a finished navigation: the committed URL of the tab
/// (after redirects; BiDi's reply carries the requested one) and the BiDi
/// ids of the reply.
async fn settled(tab: &Tab, browser: Value, requested: &str) -> NavigationOutcome {
    let params = json!({"root": tab.context_id(), "maxDepth": 0});
    let tree = command(tab, "browsingContext.getTree", params).await.ok();
    let url = tree
        .as_ref()
        .and_then(|t| t.pointer("/contexts/0/url"))
        .and_then(Value::as_str)
        .unwrap_or(requested)
        .to_string();
    NavigationOutcome {
        url,
        browser,
        notes: Vec::new(),
    }
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

/// `evaluate_js`: the result in the shared shape, or the exception as an
/// error worded as on Chrome.
pub(crate) async fn evaluate_js(tab: &Tab, expression: &str) -> Result<Evaluation, BackendError> {
    let reply = evaluate_raw(tab, expression, "none").await?;
    thrown(&reply)?;
    Ok(remote_value::evaluation(&reply["result"]))
}

/// The value of a `script.evaluate` reply, or its exception as an error.
pub(crate) fn outcome(reply: &Value) -> Result<Value, BackendError> {
    thrown(reply)?;
    Ok(remote_value::to_json(&reply["result"]))
}

/// The exception of a `script.evaluate` reply as an error (Firefox puts
/// what was thrown in `text`).
fn thrown(reply: &Value) -> Result<(), BackendError> {
    if reply.get("type").and_then(Value::as_str) != Some("exception") {
        return Ok(());
    }
    let details = &reply["exceptionDetails"];
    let text = details
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or("exception");
    Err(BackendError::InvalidArgument(exception_message(
        text,
        details["lineNumber"].as_i64().unwrap_or(0),
        details["columnNumber"].as_i64().unwrap_or(0),
    )))
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

    /// BID-15 / EVL-04: A script exception is a tool error with its text and
    /// position, worded as on Chrome.
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
        let string: Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/bidi/script-evaluate-throw-string.json"
        ))
        .unwrap();
        let err = outcome(&string["result"]).unwrap_err().to_string();
        assert_eq!(err, "boom (line 1, column 7)");
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
