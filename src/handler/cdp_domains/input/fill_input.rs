use crate::handler::McpHandler;
use crate::handler::cdp_domains::cdp_target::CdpTarget;
use rust_mcp_sdk::{
    macros,
    schema::{CallToolError, CallToolRequestParams, CallToolResult},
};
use serde_json::{Value, json};

#[macros::mcp_tool(
    name = "fill_input",
    description = "Replaces the content of a text field, textarea or contenteditable element by typing key by key (the page's key handlers run; about 1 ms per character), or picks the <select> option whose value or label is the text. Fails without typing on disabled, read-only, non-text or unfocusable elements, and when the field rejects the text; notes when the page reformatted it."
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct FillInputTool {
    /// Instance id; omit for the default instance.
    pub instance_id: Option<String>,
    /// Tab id; omit for the active tab.
    pub tab_id: Option<String>,
    /// Element: CSS selector, 'host >>> inner' (inside an open shadow root or same-origin iframe), or a snapshot ref (e12).
    pub selector: String,
    /// Text to type, or the value or label of the option of a `<select>`.
    pub text: String,
}

/// What is left after the preparation snippet (`util::js::prepare_fill`).
pub(crate) enum FillStart {
    /// The field is focused and empty: type the text.
    Type,
    /// A `<select>` option was picked: nothing to type.
    Selected,
}

/// Reads the preparation snippet's `{status}`; anything that must not be
/// typed into is an error, so the text never lands in another element.
pub(crate) fn fill_start(value: &Value, selector: &str) -> Result<FillStart, CallToolError> {
    match value["status"].as_str() {
        Some("ready") => Ok(FillStart::Type),
        Some("selected") => Ok(FillStart::Selected),
        Some("not_editable") => Err(CallToolError::from_message(format!(
            "Element '{selector}' is not editable ({}); nothing was typed.",
            value["reason"].as_str().unwrap_or("unknown reason")
        ))),
        Some("no_option") => {
            let options: Vec<&str> = value["options"]
                .as_array()
                .map(|o| o.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            Err(CallToolError::from_message(format!(
                "No option of '{selector}' has that value or label. Options: {}.",
                options.join(", ")
            )))
        }
        _ => Err(CallToolError::from_message(format!(
            "Element not found for selector: {selector}{}",
            crate::util::js::ref_hint(selector)
        ))),
    }
}

/// The reply once a `<select>` option was picked.
pub(crate) fn selected_reply(selector: &str) -> CallToolResult {
    CallToolResult::text_content(vec![
        format!("Successfully selected the option matching the text in '{selector}'.").into(),
    ])
}

/// The reply from the result snippet (`util::js::fill_result`): an error
/// when the field ended empty (it rejected the text), a note when it holds
/// something else. Neither repeats the text, which may be a password.
pub(crate) fn fill_reply(value: &Value, selector: &str) -> Result<CallToolResult, CallToolError> {
    if value["status"].as_str() != Some("done") {
        return Err(CallToolError::from_message(format!(
            "Element '{selector}' disappeared while typing (the page replaced it)."
        )));
    }
    let done = format!("Successfully filled input '{selector}' with text.");
    if value["matches"].as_bool() == Some(true) {
        return Ok(CallToolResult::text_content(vec![done.into()]));
    }
    if value["empty"].as_bool() == Some(true) {
        return Err(CallToolError::from_message(format!(
            "'{selector}' is empty after typing: the field rejected the text (for example letters in a number field, or a date field that needs its own format). Set the value with 'evaluate_js' instead."
        )));
    }
    Ok(CallToolResult::text_content(vec![
        format!("{done}\nNote: the field holds something other than the text (the page or the field reformatted it, for example a mask or maxlength).").into(),
    ]))
}

/// The CDP commands that type `c`: the key press that types it, so the
/// page sees keydown, keypress, beforeinput, input and keyup and its
/// handlers can cancel the key, as on Firefox (BiDi key actions;
/// `Input.insertText` skipped every key event). A control character
/// (newline, tab) is inserted as text instead: as a key, Chrome would press
/// Tab or Enter, moving the focus or submitting the form.
fn typing(c: char) -> Vec<(&'static str, Value)> {
    let key = c.to_string();
    if c.is_control() {
        return vec![("Input.insertText", json!({"text": key}))];
    }
    vec![
        (
            "Input.dispatchKeyEvent",
            json!({"type": "keyDown", "key": key, "text": key, "unmodifiedText": key}),
        ),
        (
            "Input.dispatchKeyEvent",
            json!({"type": "keyUp", "key": key}),
        ),
    ]
}

impl FillInputTool {
    pub async fn handle(
        params: CallToolRequestParams,
        handler: &McpHandler,
    ) -> Result<CallToolResult, CallToolError> {
        let ids = |a: &Self| (a.instance_id.clone(), a.tab_id.clone());
        let (args, target) = crate::handler::args_and_target(params, handler, ids).await?;
        fill(&target, &args.selector, &args.text).await
    }
}

/// Fills the element `selector` names with `text` on Chrome (`fill_input`,
/// and each field of `fill_form`).
pub(crate) async fn fill(
    target: &CdpTarget,
    selector: &str,
    text: &str,
) -> Result<CallToolResult, CallToolError> {
    let prepare = crate::util::js::prepare_fill(selector, text);
    let prepared = crate::handler::snippet_value(target, &prepare).await?;
    if let FillStart::Selected = fill_start(&prepared, selector)? {
        return Ok(selected_reply(selector));
    }
    for c in text.chars() {
        for (method, params) in typing(c) {
            crate::handler::send_or(target, method, params, "CDP Error").await?;
        }
    }
    let check = crate::util::js::fill_result(selector, text);
    fill_reply(
        &crate::handler::snippet_value(target, &check).await?,
        selector,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_mcp_sdk::schema::CallToolRequestParams;
    use serde_json::json;

    #[tokio::test]
    async fn test_fill_input_params_deserialization() {
        let params: Result<CallToolRequestParams, _> = serde_json::from_value(json!({
            "name": "fill_input",
            "arguments": {
                "selector": "#username",
                "text": "my_user"
            }
        }));
        assert!(params.is_ok());
    }

    #[tokio::test]
    async fn test_fill_input_tool_deserialization() {
        let tool: Result<FillInputTool, _> = serde_json::from_value(json!({
            "selector": "#username",
            "text": "my_user"
        }));
        assert!(tool.is_ok());
        let tool = tool.unwrap();
        assert_eq!(tool.selector, "#username");
        assert_eq!(tool.text, "my_user");
    }

    /// A Chrome mock whose preparation snippet answers `prepared` and whose
    /// result snippet answers `typed` (both by value).
    async fn page_answering(
        prepared: Value,
        typed: Value,
    ) -> &'static cdp_browser_lite::test_support::mock_devtools::MockDevTools {
        let mock = crate::handler::cdp_domains::tests::spawn_recording_mock().await;
        mock.respond_with("Runtime.evaluate", move |params| {
            let expression = params["expression"].as_str().unwrap_or_default();
            let value = if expression.contains("'ready'") {
                prepared.clone()
            } else {
                typed.clone()
            };
            json!({"result": {"result": {"type": "object", "value": value}}})
        });
        mock
    }

    async fn fill(
        mock: &cdp_browser_lite::test_support::mock_devtools::MockDevTools,
        selector: &str,
    ) -> Result<CallToolResult, CallToolError> {
        let handler = crate::handler::cdp_domains::tests::mock_handler(mock.http_port);
        let args = json!({"selector": selector, "text": "SECRET"});
        let params = crate::handler::cdp_domains::tests::call("fill_input", args);
        FillInputTool::handle(params, &handler).await
    }

    /// AUD-IN-08: an element that cannot take text fails and nothing is typed
    /// (the text used to land in the previously focused field).
    #[tokio::test]
    async fn non_editable_element_fails_without_typing() {
        // Given a page where "#d" is a plain div
        let reason = "not an input, textarea or contenteditable element";
        let mock = page_answering(
            json!({"status": "not_editable", "reason": reason}),
            json!(null),
        )
        .await;
        // When fill_input targets it
        let error = fill(mock, "#d").await.unwrap_err().to_string();
        // Then the error names the element and the reason, and no text was inserted
        assert_eq!(
            error,
            format!("Element '#d' is not editable ({reason}); nothing was typed.")
        );
        assert!(mock.commands_named("Input.dispatchKeyEvent").is_empty());
    }

    /// AUD-IN-09: a <select> gets the option matching the text, without typing.
    #[tokio::test]
    async fn select_picks_the_matching_option() {
        // Given a select with an option labelled like the text
        let mock = page_answering(json!({"status": "selected"}), json!(null)).await;
        // When fill_input targets it
        let reply = fill(mock, "#s").await.unwrap();
        // Then the option was selected in the page and no text was inserted
        let text = crate::handler::cdp_domains::tests::reply_text(&reply);
        assert_eq!(
            text,
            "Successfully selected the option matching the text in '#s'."
        );
        assert!(mock.commands_named("Input.dispatchKeyEvent").is_empty());
        let prepare = &mock.commands_named("Runtime.evaluate")[0].params;
        let expression = prepare["expression"].as_str().unwrap();
        assert!(expression.contains("HTMLSelectElement"), "{expression}");
    }

    /// AUD-IN-09: a <select> without a matching option fails listing them.
    #[tokio::test]
    async fn select_without_matching_option_fails() {
        let options = json!({"status": "no_option", "options": ["Red", "Blue"]});
        let mock = page_answering(options, json!(null)).await;
        let error = fill(mock, "#s").await.unwrap_err().to_string();
        assert_eq!(
            error,
            "No option of '#s' has that value or label. Options: Red, Blue."
        );
    }

    /// AUD-IN-10: a field left empty after typing rejected the text: error.
    #[tokio::test]
    async fn field_left_empty_is_an_error() {
        // Given a field that drops what is typed (e.g. letters in type=number)
        let mock = page_answering(
            json!({"status": "ready"}),
            json!({"status": "done", "matches": false, "empty": true}),
        )
        .await;
        // When fill_input types into it
        let error = fill(mock, "#num").await.unwrap_err().to_string();
        // Then the text was inserted, but the call fails saying the field rejected it
        assert_eq!(mock.commands_named("Input.dispatchKeyEvent").len(), 12);
        assert!(error.starts_with("'#num' is empty after typing"), "{error}");
    }

    /// AUD-IN-10: a field holding something else than the text (mask,
    /// maxlength) succeeds with a note that never echoes the text.
    #[tokio::test]
    async fn reformatted_field_succeeds_with_a_note() {
        let mock = page_answering(
            json!({"status": "ready"}),
            json!({"status": "done", "matches": false, "empty": false}),
        )
        .await;
        let reply = fill(mock, "#phone").await.unwrap();
        let text = crate::handler::cdp_domains::tests::reply_text(&reply);
        assert!(
            text.starts_with("Successfully filled input '#phone' with text.\nNote:"),
            "{text}"
        );
        assert!(!text.contains("SECRET"), "{text}");
    }

    /// AUD-IN-12: Chrome types key by key, as Firefox does, so the page's
    /// keydown handlers run and can cancel a key (insertText skipped them:
    /// Chrome wrote `abc` where Firefox wrote `bc`).
    #[tokio::test]
    async fn text_is_typed_key_by_key() {
        let mock = page_answering(
            json!({"status": "ready"}),
            json!({"status": "done", "matches": true, "empty": false}),
        )
        .await;
        let handler = crate::handler::cdp_domains::tests::mock_handler(mock.http_port);
        let args = json!({"selector": "#a", "text": "a😀\n"});
        let params = crate::handler::cdp_domains::tests::call("fill_input", args);
        FillInputTool::handle(params, &handler).await.unwrap();
        let keys: Vec<Value> = mock
            .commands_named("Input.dispatchKeyEvent")
            .into_iter()
            .map(|c| c.params)
            .collect();
        assert_eq!(
            keys,
            [
                json!({"type": "keyDown", "key": "a", "text": "a", "unmodifiedText": "a"}),
                json!({"type": "keyUp", "key": "a"}),
                json!({"type": "keyDown", "key": "😀", "text": "😀", "unmodifiedText": "😀"}),
                json!({"type": "keyUp", "key": "😀"}),
            ]
        );
        // And a control character is inserted as text (a key would move the focus)
        let inserted = mock.commands_named("Input.insertText");
        assert_eq!(inserted.len(), 1);
        assert_eq!(inserted[0].params, json!({"text": "\n"}));
    }

    /// AUD-IN-08: the happy path types the text once, then checks the field.
    #[tokio::test]
    async fn editable_field_is_typed_and_checked() {
        let mock = page_answering(
            json!({"status": "ready"}),
            json!({"status": "done", "matches": true, "empty": false}),
        )
        .await;
        let reply = fill(mock, "#a").await.unwrap();
        assert_eq!(
            crate::handler::cdp_domains::tests::reply_text(&reply),
            "Successfully filled input '#a' with text."
        );
        let keys = mock.commands_named("Input.dispatchKeyEvent");
        assert_eq!(keys.len(), 12);
        assert_eq!(keys[0].params["text"], "S");
        assert_eq!(mock.commands_named("Runtime.evaluate").len(), 2);
    }
}
