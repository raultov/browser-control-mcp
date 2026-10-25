//! Page, runtime and input operations on one Firefox tab.

use super::protocol_error;
use super::remote_value;
use crate::backend::error::BackendError;
use crate::handler::evaluation::{Evaluation, exception_message};
use crate::handler::navigation::{LOAD_TIMEOUT, NOT_LOADED_NOTE, NavigationOutcome, load_before};
use bidi_browser_lite::{BidiError, ReadinessState, Tab};
use serde_json::{Value, json};

/// Sends a command on the tab's session, mapping errors. A prompt the page
/// opens meanwhile fails the command at once (see [`unless_prompt`]).
pub(crate) async fn command(tab: &Tab, method: &str, params: Value) -> Result<Value, BackendError> {
    let send = tab.send_raw_command(method, params);
    let reply = if method == "browsingContext.handleUserPrompt" {
        send.await
    } else {
        unless_prompt(tab, method, send).await?
    };
    reply
        .map(|reply| reply.result.unwrap_or(Value::Null))
        .map_err(|e| protocol_error(method, &e))
}

/// Runs `command` unless the tab opens a JavaScript dialog first: the
/// command that opened it (an `alert()` in a script, a click) would only
/// answer once it is closed, so it fails at once quoting the dialog.
pub(crate) async fn unless_prompt<T>(
    tab: &Tab,
    method: &str,
    command: impl std::future::Future<Output = T>,
) -> Result<T, BackendError> {
    let opened = |event: &bidi_browser_lite::BidiResult<bidi_browser_lite::WsResponse>| match event
    {
        Ok(event) if event.method.as_deref() == Some("browsingContext.userPromptOpened") => event
            .params
            .as_ref()
            .map(crate::handler::dialogs::OpenDialog::from_firefox),
        _ => None,
    };
    crate::handler::dialogs::unless_dialog(command, tab.on_module("browsingContext"), opened)
        .await
        .map_err(|dialog| BackendError::InvalidArgument(dialog.opened_during(method)))
}

/// `browsingContext.navigate` waiting for `load` (spike H2). A network
/// error is a navigation error with Firefox's code (`NS_ERROR_UNKNOWN_HOST`).
pub(crate) async fn navigate(tab: &Tab, url: &str) -> Result<NavigationOutcome, BackendError> {
    let method = "browsingContext.navigate";
    let wait = tab.navigate(url, ReadinessState::Complete);
    let Some(reply) = outlasting_timeout(tab, method, wait).await? else {
        return Ok(not_loaded(tab, url).await);
    };
    let reply = reply.map_err(|e| navigation_error(method, url, &e))?;
    Ok(settled(tab, reply.result.unwrap_or_else(|| json!({})), url).await)
}

/// Runs a navigation command waiting for `load` (failing at once on a
/// dialog). When it outlasts the BiDi command timeout the navigation goes
/// on: `None` once `load` did not come within [`LOAD_TIMEOUT`] of the start,
/// else the command's reply (an empty success when `load` came late).
async fn outlasting_timeout(
    tab: &Tab,
    method: &str,
    command: impl std::future::Future<Output = Result<bidi_browser_lite::WsResponse, BidiError>>,
) -> Result<Option<Result<bidi_browser_lite::WsResponse, BidiError>>, BackendError> {
    // Subscribed first: `load` may come right after the timeout.
    let mut events = tab.on_module("browsingContext");
    let deadline = tokio::time::Instant::now() + LOAD_TIMEOUT;
    match unless_prompt(tab, method, command).await? {
        Err(BidiError::Timeout { .. }) => {
            let is_load = |event: &bidi_browser_lite::BidiResult<bidi_browser_lite::WsResponse>| {
                event
                    .as_ref()
                    .is_ok_and(|e| e.method.as_deref() == Some("browsingContext.load"))
            };
            let loaded = load_before(&mut events, deadline, is_load).await;
            Ok(loaded.then(|| Ok(bidi_browser_lite::WsResponse::default())))
        }
        other => Ok(Some(other)),
    }
}

/// The outcome of a navigation that did not fire `load` in time.
async fn not_loaded(tab: &Tab, requested: &str) -> NavigationOutcome {
    let mut outcome = settled(tab, json!({}), requested).await;
    outcome.notes.push(NOT_LOADED_NOTE.to_string());
    outcome
}

/// `browsingContext.reload` waiting for `load`.
pub(crate) async fn reload(tab: &Tab) -> Result<NavigationOutcome, BackendError> {
    let method = "browsingContext.reload";
    let Some(reply) = outlasting_timeout(tab, method, tab.reload(ReadinessState::Complete)).await?
    else {
        return Ok(not_loaded(tab, "").await);
    };
    let reply = reply.map_err(|e| navigation_error(method, "", &e))?;
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

/// Clicks the viewport point `(x, y)` with real pointer input (spike H6).
/// The input state is released even when the actions fail, so a later
/// action does not start with the button held; after a failure (a dialog
/// the click opened blocks the page) in the background, so the call does
/// not wait for the dialog.
pub(crate) async fn click_at(tab: &Tab, x: f64, y: f64) -> Result<(), BackendError> {
    // The click snippet returns whole CSS pixels; BiDi wants integers.
    let (x, y) = (x.round() as i64, y.round() as i64);
    let pointer = json!({"type": "pointer", "id": "mouse", "parameters": {"pointerType": "mouse"}, "actions": [
        {"type": "pointerMove", "x": x, "y": y, "origin": "viewport"},
        {"type": "pointerDown", "button": 0},
        {"type": "pointerUp", "button": 0},
    ]});
    let context = tab.context_id();
    let clicked = command(
        tab,
        "input.performActions",
        json!({"context": context, "actions": [pointer]}),
    )
    .await;
    let release = json!({"context": context});
    if clicked.is_err() {
        let tab = tab.clone();
        tokio::spawn(async move {
            let _ = tab.send_raw_command("input.releaseActions", release).await;
        });
        return clicked.map(|_| ());
    }
    command(tab, "input.releaseActions", release)
        .await
        .map(|_| ())
}

/// Types `text` key by key into the focused element (spike H8).
pub(crate) async fn type_text(tab: &Tab, text: &str) -> Result<(), BackendError> {
    if text.is_empty() {
        return Ok(());
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
