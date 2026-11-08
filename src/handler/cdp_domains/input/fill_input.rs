use super::keys::Shortcut;
use crate::handler::McpHandler;
use crate::handler::cdp_domains::cdp_target::CdpTarget;
use crate::util::js::FillMode;
use rust_mcp_sdk::{
    macros,
    schema::{CallToolError, CallToolRequestParams, CallToolResult},
};
use serde_json::{Value, json};

#[macros::mcp_tool(
    name = "fill_input",
    description = "Types into a text field, textarea or contenteditable, replacing its content (or after it with append), key by key so the page's handlers run (~1 ms per character); picks the <select> option(s) named; sets a date, time, range or colour input (YYYY-MM-DD, HH:MM, a number, #rrggbb). Fails without typing on disabled, read-only, non-text or unfocusable elements and when the field rejects the text; notes a reformatted value. For several fields at once use fill_form."
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct FillInputTool {
    /// Default: 'default'.
    pub instance_id: Option<String>,
    /// Default: the active tab.
    pub tab_id: Option<String>,
    /// CSS selector, 'host >>> inner' (into an open shadow root or same-origin iframe) or snapshot ref (e12).
    pub selector: String,
    /// Text to type, or a `<select>` option's value or label (`<select multiple>`: one per line, exactly those selected).
    pub text: String,
    /// Type the text at the end of the current content instead of replacing it. Default: false.
    pub append: Option<bool>,
}

/// What is left after the preparation snippet (`util::js::prepare_fill`).
pub(crate) enum FillStart {
    /// The field is focused, empty or with the caret at the end: click
    /// `click` (the visible container of an editor whose input has no box:
    /// Monaco takes the focus only from its own mouse handling), press
    /// `keys` (an editor of its own selects its content or reaches its end
    /// the way a user does), then type `prefix` (what an appended field
    /// without a caret held, such as a number field, emptied to be
    /// retyped) and the text.
    Type {
        click: Option<(f64, f64)>,
        prefix: String,
        keys: Vec<Shortcut>,
        /// A textarea or contenteditable: a newline is the Enter key.
        multiline: bool,
    },
    /// Nothing to type: a `<select>` option was picked, or a value set
    /// (date, time, range, colour); the reply.
    Done(CallToolResult),
}

/// Reads the preparation snippet's `{status}`; anything that must not be
/// typed into is an error, so the text never lands in another element.
pub(crate) fn fill_start(value: &Value, selector: &str) -> Result<FillStart, CallToolError> {
    match value["status"].as_str() {
        Some("ready") => Ok(FillStart::Type {
            click: value["click"]["x"]
                .as_f64()
                .zip(value["click"]["y"].as_f64()),
            prefix: value["prefix"].as_str().unwrap_or_default().to_string(),
            keys: shortcuts(value)?,
            multiline: value["multiline"].as_bool().unwrap_or(false),
        }),
        Some("selected") => Ok(FillStart::Done(selected_reply(
            value["count"].as_u64(),
            selector,
        ))),
        Some("set") => Ok(FillStart::Done(set_reply(value, selector))),
        Some("rejected") => Err(CallToolError::from_message(format!(
            "'{selector}' did not take the text: it expects {}; nothing was changed.",
            value["format"].as_str().unwrap_or("another format")
        ))),
        Some("not_editable") => Err(CallToolError::from_message(format!(
            "Element '{selector}' is not editable ({}); nothing was typed.",
            value["reason"].as_str().unwrap_or("unknown reason")
        ))),
        Some("no_option") => {
            let options: Vec<&str> = value["options"]
                .as_array()
                .map(|o| o.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            let which = value["missing"].as_str().map_or_else(
                || "that value or label".to_string(),
                |missing| format!("the value or label '{missing}'; nothing was changed"),
            );
            Err(CallToolError::from_message(format!(
                "No option of '{selector}' has {which}. Options: {}.",
                options.join(", ")
            )))
        }
        _ => Err(CallToolError::from_message(format!(
            "Element not found for selector: {selector}{}",
            crate::util::js::ref_hint(selector)
        ))),
    }
}

/// The keys the preparation snippet asks for (`keys: [{key, modifiers}]`).
fn shortcuts(value: &Value) -> Result<Vec<Shortcut>, CallToolError> {
    let Some(keys) = value["keys"].as_array() else {
        return Ok(Vec::new());
    };
    keys.iter()
        .map(|entry| {
            let name = entry["key"].as_str().unwrap_or_default();
            let key = super::keys::key(name).map_err(CallToolError::from_message)?;
            let held = serde_json::from_value(entry["modifiers"].clone()).unwrap_or_default();
            Ok((key, held))
        })
        .collect()
}

/// The reply once a `<select>` option was picked, or `count` options of a
/// `<select multiple>`.
pub(crate) fn selected_reply(count: Option<u64>, selector: &str) -> CallToolResult {
    let what = match count {
        None => "the option matching the text".to_string(),
        Some(1) => "1 option, one per line of the text,".to_string(),
        Some(count) => format!("{count} options, one per line of the text,"),
    };
    CallToolResult::text_content(vec![
        format!("Successfully selected {what} in '{selector}'.").into(),
    ])
}

/// The reply once a date, time, range or colour input got its value: a
/// note when the input holds another one (a range snaps to its step and
/// bounds, a colour is lowercased).
fn set_reply(value: &Value, selector: &str) -> CallToolResult {
    let mut text = format!("Successfully set the value of '{selector}'.");
    if value["matches"] == false {
        let held = value["value"].as_str().unwrap_or_default();
        text.push_str(&format!(
            "\nNote: '{selector}' holds {held:?}: the input adjusted the value (a range keeps to its step and bounds)."
        ));
    }
    CallToolResult::text_content(vec![text.into()])
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
    let done = if value["append"].as_bool() == Some(true) {
        format!("Successfully appended text to '{selector}'.")
    } else {
        format!("Successfully filled input '{selector}' with text.")
    };
    if value["matches"].as_bool() == Some(true) {
        return Ok(CallToolResult::text_content(vec![done.into()]));
    }
    if value["unchecked"].as_bool() == Some(true) {
        return Ok(CallToolResult::text_content(vec![
            format!("{done}\nNote: '{selector}' is an editor of its own (such as Monaco): its content could not be read back, so the text was typed but not checked.").into(),
        ]));
    }
    let empty = value["empty"].as_bool() == Some(true);
    if value["rejected"].as_bool().unwrap_or(empty) {
        let state = if empty { "is empty" } else { "did not change" };
        return Err(CallToolError::from_message(format!(
            "'{selector}' {state} after typing: the field rejected the text (for example letters in a number field, or a date field that needs its own format). Set the value with 'evaluate_js' instead."
        )));
    }
    Ok(CallToolResult::text_content(vec![
        format!("{done}\nNote: the field holds something other than the text (the page or the field reformatted it, for example a mask or maxlength).").into(),
    ]))
}

/// The CDP commands that type `c`: the key press that types it, so the
/// page sees keydown, keypress, beforeinput, input and keyup and its
/// handlers can cancel the key, as on Firefox (BiDi key actions;
/// `Input.insertText` skipped every key event). In a textarea or
/// contenteditable (`multiline`) a newline is the Enter key, as a user
/// types it (ISSUE-81). Another control character (a tab, a newline in a
/// one-line field) is inserted as text: as a key, Chrome would press Tab or
/// Enter, moving the focus or submitting the form.
fn typing(c: char, multiline: bool) -> Vec<(&'static str, Value)> {
    let key = c.to_string();
    if c == '\n' && multiline {
        let enter = json!({"key": "Enter", "code": "Enter", "windowsVirtualKeyCode": 13});
        let mut down = enter.clone();
        down["type"] = json!("keyDown");
        (down["text"], down["unmodifiedText"]) = (json!("\r"), json!("\r"));
        let mut up = enter;
        up["type"] = json!("keyUp");
        return vec![
            ("Input.dispatchKeyEvent", down),
            ("Input.dispatchKeyEvent", up),
        ];
    }
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
    /// Whether the text replaces the content or follows it.
    pub(crate) fn mode(&self) -> FillMode {
        if self.append == Some(true) {
            FillMode::Append
        } else {
            FillMode::Replace
        }
    }

    pub async fn handle(
        params: CallToolRequestParams,
        handler: &McpHandler,
    ) -> Result<CallToolResult, CallToolError> {
        let ids = |a: &Self| (a.instance_id.clone(), a.tab_id.clone());
        let (args, target) = crate::handler::args_and_target(params, handler, ids, |a: &Self| {
            Some(a.selector.clone())
        })
        .await?;
        fill(&target, &args.selector, &args.text, args.mode()).await
    }
}

/// Fills the element `selector` names with `text` on Chrome (`fill_input`,
/// and each field of `fill_form`).
pub(crate) async fn fill(
    target: &CdpTarget,
    selector: &str,
    text: &str,
    mode: FillMode,
) -> Result<CallToolResult, CallToolError> {
    let prepare = crate::util::js::prepare_fill(selector, text, mode);
    let prepared = crate::handler::snippet_value(target, &prepare).await?;
    let (click, prefix, keys, multiline) = match fill_start(&prepared, selector)? {
        FillStart::Type {
            click,
            prefix,
            keys,
            multiline,
        } => (click, prefix, keys, multiline),
        FillStart::Done(reply) => return Ok(reply),
    };
    if let Some(point) = click {
        let once = super::click_element::ClickOptions::default();
        super::click_element::press_and_release(target, point, &once).await?;
    }
    for (key, held) in &keys {
        let pressing = super::press_key::press(target, key, held);
        super::keys::holding(target, held, pressing).await?;
    }
    for c in prefix.chars().chain(text.chars()) {
        for (method, params) in typing(c, multiline) {
            crate::handler::send_or(target, method, params, "CDP Error").await?;
        }
    }
    let check = crate::util::js::fill_result(selector, text, mode);
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

    /// ISSUE-81 (Chrome): a newline in a textarea or contenteditable is the
    /// Enter key, so the page sees it; in a one-line field it stays text
    /// (Enter would submit the form).
    #[test]
    fn newlines_are_enter_in_multiline_fields() {
        let enter = typing('\n', true);
        assert_eq!(enter.len(), 2);
        assert_eq!(enter[0].1["key"], "Enter");
        assert_eq!(enter[0].1["text"], "\r");
        assert_eq!(enter[0].1["windowsVirtualKeyCode"], 13);
        assert_eq!(enter[1].1["type"], "keyUp");
        assert_eq!(
            typing('\n', false),
            vec![("Input.insertText", json!({"text": "\n"}))]
        );
        assert_eq!(typing('a', true)[0].1["key"], "a");
        let ready = json!({"status": "ready", "prefix": "", "keys": [], "multiline": true});
        assert!(matches!(
            fill_start(&ready, "#t"),
            Ok(FillStart::Type {
                multiline: true,
                ..
            })
        ));
    }

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

    /// ISSUE-28: a custom editor (Monaco) first gets the keys the
    /// preparation names (Control+A), then the text; the reply says its
    /// content could not be checked.
    #[tokio::test]
    async fn custom_editors_get_their_keys_first() {
        // Given an editor whose content the page does not expose
        let ready = json!({"status": "ready", "prefix": "", "keys": [{"key": "a", "modifiers": ["Control"]}]});
        let mock = page_answering(
            ready,
            json!({"status": "done", "unchecked": true, "append": false}),
        )
        .await;
        let handler = crate::handler::cdp_domains::tests::mock_handler(mock.http_port);
        // When it is filled
        let args = json!({"selector": "e12", "text": "ok"});
        let params = crate::handler::cdp_domains::tests::call("fill_input", args);
        let reply = FillInputTool::handle(params, &handler).await.unwrap();
        // Then Control+A comes before the typed keys, and the reply says unchecked
        let keys: Vec<(String, i64)> = mock
            .commands_named("Input.dispatchKeyEvent")
            .iter()
            .map(|c| {
                (
                    c.params["key"].as_str().unwrap_or_default().to_string(),
                    c.params["modifiers"].as_i64().unwrap_or(0),
                )
            })
            .collect();
        let first_a = keys.iter().position(|(k, _)| k == "a").unwrap();
        let first_o = keys.iter().position(|(k, _)| k == "o").unwrap();
        assert!(first_a < first_o, "{keys:?}");
        assert_eq!(keys[first_a].1, 2, "Control held: {keys:?}");
        let text = &reply.content[0].as_text_content().unwrap().text;
        assert!(text.contains("could not be read back"), "{text}");
    }

    /// ISSUE-38: a value set directly (date, time, range, colour) types
    /// nothing; the reply says so, notes a value the input changed, and an
    /// input that refused it fails naming the format.
    #[tokio::test]
    async fn set_values_are_reported() {
        let set = json!({"status": "set", "matches": true, "value": null});
        let mock = page_answering(set, json!({})).await;
        let handler = crate::handler::cdp_domains::tests::mock_handler(mock.http_port);
        let fill = |text: &str| {
            let args = json!({"selector": "#d", "text": text});
            FillInputTool::handle(
                crate::handler::cdp_domains::tests::call("fill_input", args),
                &handler,
            )
        };
        let reply = fill("2026-10-31").await.unwrap();
        let text = &reply.content[0].as_text_content().unwrap().text;
        assert_eq!(text, "Successfully set the value of '#d'.");
        assert!(mock.commands_named("Input.dispatchKeyEvent").is_empty());
        let snapped = json!({"status": "set", "matches": false, "value": "100"});
        let reply = fill_reply_for(&snapped);
        assert!(reply.contains("Note: '#d' holds \"100\""), "{reply}");
        let rejected = json!({"status": "rejected", "format": "YYYY-MM-DD"});
        let error = fill_start(&rejected, "#d").err().unwrap().to_string();
        assert!(error.contains("expects YYYY-MM-DD"), "{error}");
    }

    /// The reply text of a fill whose preparation answered `prepared`.
    fn fill_reply_for(prepared: &Value) -> String {
        let Ok(FillStart::Done(reply)) = fill_start(prepared, "#d") else {
            panic!("not done: {prepared}");
        };
        reply.content[0].as_text_content().unwrap().text.clone()
    }

    /// ISSUE-32: append types after the content, says so, and fails when
    /// the field did not change.
    #[tokio::test]
    async fn append_says_where_the_text_went() {
        // Given a field that took the text
        let done = json!({"status": "done", "matches": true, "empty": false, "rejected": false, "append": true});
        let mock = page_answering(json!({"status": "ready"}), done).await;
        let handler = crate::handler::cdp_domains::tests::mock_handler(mock.http_port);
        // When text is appended
        let args = json!({"selector": "#q", "text": "ab", "append": true});
        let params = crate::handler::cdp_domains::tests::call("fill_input", args);
        let reply = FillInputTool::handle(params, &handler).await.unwrap();
        // Then the preparation kept the content and the reply says appended
        let expressions: Vec<String> = mock
            .commands_named("Runtime.evaluate")
            .iter()
            .map(|c| {
                c.params["expression"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string()
            })
            .collect();
        assert!(
            expressions[0].contains("const append = true;"),
            "{expressions:?}"
        );
        let text = &reply.content[0].as_text_content().unwrap().text;
        assert!(
            text.starts_with("Successfully appended text to '#q'."),
            "{text}"
        );
        // And a field that did not change is an error
        let unchanged = json!({"status": "done", "matches": false, "empty": false, "rejected": true, "append": true});
        let error = fill_reply(&unchanged, "#q").unwrap_err().to_string();
        assert!(error.contains("did not change after typing"), "{error}");
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

    /// ISSUE-47: a `<select multiple>` gets exactly the options named one
    /// per line (the reply counts them); an unknown line fails naming it,
    /// with nothing changed.
    #[tokio::test]
    async fn multiple_select_picks_every_line() {
        // Given a multiple select the page set to the lines of the text
        let mock = page_answering(json!({"status": "selected", "count": 2}), json!(null)).await;
        // When fill_input targets it, Then the reply counts the options
        let reply = fill(mock, "#m").await.unwrap();
        let text = crate::handler::cdp_domains::tests::reply_text(&reply);
        assert_eq!(
            text,
            "Successfully selected 2 options, one per line of the text, in '#m'."
        );
        let prepare = &mock.commands_named("Runtime.evaluate")[0].params;
        let expression = prepare["expression"].as_str().unwrap();
        assert!(
            expression.contains("const wanted = el.multiple ? text.split(/\\r?\\n/)"),
            "{expression}"
        );
        // And a line no option matches is named
        let missing =
            json!({"status": "no_option", "missing": "Olives", "options": ["Cheese", "Ham"]});
        let mock = page_answering(missing, json!(null)).await;
        let error = fill(mock, "#m").await.unwrap_err().to_string();
        assert_eq!(
            error,
            "No option of '#m' has the value or label 'Olives'; nothing was changed. Options: Cheese, Ham."
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
