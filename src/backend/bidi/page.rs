//! Page, runtime and input operations on one Firefox tab.

use super::protocol_error;
use super::remote_value;
use crate::backend::error::BackendError;
use crate::handler::cdp_domains::page::wait_for::CheckError;
use crate::handler::evaluation::{self, Evaluation, exception_message};
use crate::handler::interruption::Interruption;
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
    race_prompt(tab, command)
        .await
        .map_err(|dialog| BackendError::InvalidArgument(dialog.during(method)))
}

/// Runs `command` unless the tab opens a JavaScript dialog first: then the
/// dialog, for input actions whose effect happened anyway.
async fn race_prompt<T>(
    tab: &Tab,
    command: impl std::future::Future<Output = T>,
) -> Result<T, Interruption> {
    let opened = |event: &bidi_browser_lite::BidiResult<bidi_browser_lite::WsResponse>| match event
    {
        Ok(event) if event.method.as_deref() == Some("browsingContext.userPromptOpened") => event
            .params
            .as_ref()
            .map(crate::handler::dialogs::OpenDialog::from_firefox),
        _ => None,
    };
    crate::handler::dialogs::unless_blocked(command, tab.on_module("browsingContext"), opened)
        .await
        .map_err(Interruption::Dialog)
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
        status: None,
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
/// error worded as on Chrome. An object is held by reference (a handle,
/// without its contents) and read by value only when its JSON form fits
/// ([`evaluation::MEASURE`]): a large one would cross the connection whole.
pub(crate) async fn evaluate_js(tab: &Tab, expression: &str) -> Result<Evaluation, BackendError> {
    let params = json!({
        "expression": expression,
        "target": tab.script_target(),
        "awaitPromise": true,
        "resultOwnership": "root",
        "serializationOptions": {"maxObjectDepth": 0},
    });
    let method = "script.evaluate";
    let reply = unless_prompt(tab, method, tab.send_raw_command(method, params))
        .await?
        .map(|reply| reply.result.unwrap_or(Value::Null))
        .map_err(|e| evaluation_error(&e))?;
    let outcome = match thrown(&reply) {
        Ok(()) => measured(tab, &reply["result"]).await,
        Err(error) => Err(error),
    };
    let handles: Vec<&Value> = [&reply["result"], &reply["exceptionDetails"]["exception"]]
        .into_iter()
        .filter_map(|remote| remote.get("handle"))
        .collect();
    if !handles.is_empty() {
        let release = json!({"handles": handles, "target": tab.script_target()});
        // Best effort: the handles also go with the page's realm.
        let _ = command(tab, "script.disown", release).await;
    }
    outcome
}

/// A failed `script.evaluate` as the agent should read it: without a
/// result in time a script may still run, and Firefox offers no way to stop
/// it from outside (Chrome stops it).
fn evaluation_error(error: &BidiError) -> BackendError {
    match error {
        BidiError::Timeout { .. } => BackendError::InvalidArgument(
            "no result after 10 s: the expression waits for a promise that never settles, or a script still runs (an endless loop freezes the tab, and Firefox cannot stop it from outside). If the tab no longer answers, close it with close_tab.".into(),
        ),
        other => protocol_error("script.evaluate", other),
    }
}

/// Reads a value held by a handle as it is (for those without a JSON form:
/// maps, sets, nodes, errors...).
const RETURN_THIS: &str = "function () { return this; }";

/// The shared result for `remote`, read by value when it is held by a
/// handle: an object or array through [`evaluation::MEASURE`] (its JSON
/// form, as on Chrome), anything else as it is.
async fn measured(tab: &Tab, remote: &Value) -> Result<Evaluation, BackendError> {
    let Some(handle) = remote.get("handle") else {
        return Ok(remote_value::evaluation(remote));
    };
    let kind = remote.get("type").and_then(Value::as_str);
    let json_like = matches!(kind, Some("object" | "array"));
    let declaration = if json_like {
        evaluation::MEASURE
    } else {
        RETURN_THIS
    };
    let params = json!({
        "functionDeclaration": declaration,
        "this": {"handle": handle},
        "arguments": [{"type": "number", "value": evaluation::EVAL_MAX_CHARS}],
        "target": tab.script_target(),
        "awaitPromise": false,
        "resultOwnership": "none",
    });
    let reply = command(tab, "script.callFunction", params).await?;
    thrown(&reply)?;
    let read = &reply["result"];
    let json = remote_value::json_form(read).filter(|_| json_like);
    let Some(json) = json else {
        return Ok(remote_value::evaluation(read));
    };
    let subtype = (kind == Some("array")).then_some("array");
    Ok(evaluation::from_measured("object", subtype, json))
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

/// One check of a wait (a `util::js` snippet): a failed command is retried
/// (a navigation replaced the realm), the page's exception ends the wait.
/// No dialog race per check: the caller races the whole wait.
pub(crate) async fn poll_snippet(tab: &Tab, expression: &str) -> Result<Value, CheckError> {
    let params = json!({
        "expression": expression,
        "target": tab.script_target(),
        "awaitPromise": false,
        "resultOwnership": "none",
    });
    let method = "script.evaluate";
    let reply = tab
        .send_raw_command(method, params)
        .await
        .map_err(|e| CheckError::Retry(protocol_error(method, &e).to_string()))?;
    outcome(&reply.result.unwrap_or(Value::Null)).map_err(|e| CheckError::Stop(e.into()))
}

/// `true`/`false`/value of a snippet built by `util::js`.
pub(crate) async fn run_snippet(tab: &Tab, expression: &str) -> Result<Value, BackendError> {
    evaluate(tab, expression).await
}

/// Clicks the viewport point `(x, y)` with real pointer input (spike H6);
/// the dialog the click opened, if any (the click happened). The input
/// state is released even when the actions fail or a dialog opened, so a
/// later action does not start with the button held: then in the
/// background, so the call does not wait for the dialog.
pub(crate) async fn click_at(
    tab: &Tab,
    x: f64,
    y: f64,
) -> Result<Option<Interruption>, BackendError> {
    // The click snippet returns whole CSS pixels; BiDi wants integers.
    let (x, y) = (x.round() as i64, y.round() as i64);
    let pointer = json!({"type": "pointer", "id": "mouse", "parameters": {"pointerType": "mouse"}, "actions": [
        {"type": "pointerMove", "x": x, "y": y, "origin": "viewport"},
        {"type": "pointerDown", "button": 0},
        {"type": "pointerUp", "button": 0},
    ]});
    let context = tab.context_id();
    let method = "input.performActions";
    let actions = tab.send_raw_command(method, json!({"context": context, "actions": [pointer]}));
    let clicked = match race_prompt(tab, actions).await {
        Ok(reply) => reply.map(|_| None).map_err(|e| protocol_error(method, &e)),
        Err(dialog) => Ok(Some(dialog)),
    };
    let release = json!({"context": context});
    if !matches!(clicked, Ok(None)) {
        let tab = tab.clone();
        tokio::spawn(async move {
            let _ = tab.send_raw_command("input.releaseActions", release).await;
        });
        return clicked;
    }
    command(tab, "input.releaseActions", release)
        .await
        .map(|_| None)
}

/// Characters typed per `input.performActions`: Firefox types about a
/// thousand a second, and one command must end within the command timeout
/// (20 000 characters at once timed out half typed).
pub(crate) const KEYS_PER_ACTION: usize = 500;

/// Types `text` key by key into the focused element (spike H8), in
/// batches of [`KEYS_PER_ACTION`] characters.
pub(crate) async fn type_text(tab: &Tab, text: &str) -> Result<(), BackendError> {
    if text.is_empty() {
        return Ok(());
    }
    let chars: Vec<char> = text.chars().collect();
    for batch in chars.chunks(KEYS_PER_ACTION) {
        let keys: Vec<Value> = batch
            .iter()
            .flat_map(|c| {
                let value = c.to_string();
                [
                    json!({"type": "keyDown", "value": value}),
                    json!({"type": "keyUp", "value": value}),
                ]
            })
            .collect();
        let actions = json!([{"type": "key", "id": "keyboard", "actions": keys}]);
        let params = json!({"context": tab.context_id(), "actions": actions});
        command(tab, "input.performActions", params).await?;
    }
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
    /// Width and height of the document's top-left box to capture, when the
    /// whole document is too large.
    pub(crate) clip: Option<(f64, f64)>,
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
    let mut params = json!({"context": context, "origin": origin, "format": format});
    if let (Some((width, height)), Some(map)) = (req.clip, params.as_object_mut()) {
        let clip = json!({"type": "box", "x": 0, "y": 0, "width": width, "height": height});
        map.insert("clip".into(), clip);
    }
    params
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

    /// AUD-EV-06 (Firefox): an evaluation without a result in time says
    /// what may be going on and how to get the tab back.
    #[test]
    fn evaluation_timeout_is_explained() {
        let error = evaluation_error(&BidiError::Timeout {
            method: "script.evaluate".into(),
            timeout: std::time::Duration::from_secs(10),
        })
        .to_string();
        assert!(error.contains("no result after 10 s"), "{error}");
        assert!(error.contains("close_tab"), "{error}");
        let other = evaluation_error(&BidiError::Disconnected).to_string();
        assert!(other.contains("connection to Firefox dropped"), "{other}");
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
                clip: None,
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
                clip: Some((800.0, 16384.0)),
            },
        );
        assert_eq!(jpeg["origin"], "document");
        assert_eq!(
            jpeg["clip"],
            json!({"type": "box", "x": 0, "y": 0, "width": 800.0, "height": 16384.0})
        );
        assert_eq!(
            jpeg["format"],
            json!({"type": "image/jpeg", "quality": 0.8})
        );
    }
}
