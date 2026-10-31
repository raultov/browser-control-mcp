use super::keys::{Modifier, MouseButton};
use crate::handler::McpHandler;
use crate::handler::cdp_domains::cdp_target::CdpTarget;
use crate::handler::interruption::Interruption;
use rust_mcp_sdk::{
    macros,
    schema::{CallToolError, CallToolRequestParams, CallToolResult},
};
use serde_json::{Value, json};

#[macros::mcp_tool(
    name = "click_element",
    description = "Clicks an element with real mouse input after scrolling it into view; fails without clicking when it is hidden or covered (naming the overlay). A dialog the click opens or a breakpoint its handler hits does not fail it: the reply notes it with {dialog} or {paused, location} (see 'handle_dialog', 'evaluate_on_call_frame', 'resume')."
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct ClickElementTool {
    /// Instance id; omit for the default instance.
    pub instance_id: Option<String>,
    /// Tab id; omit for the active tab.
    pub tab_id: Option<String>,
    /// Element: CSS selector, 'host >>> inner' (inside an open shadow root or same-origin iframe), or a snapshot ref (e12).
    pub selector: String,
    /// Mouse button ('right' opens context menus). Default: 'left'.
    pub button: Option<MouseButton>,
    /// 1, 2 (double click) or 3. Default: 1.
    pub click_count: Option<u8>,
    /// Keys held during the click.
    pub modifiers: Option<Vec<Modifier>>,
}

/// How a click is made: button, clicks in a row, held modifiers.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ClickOptions {
    pub(crate) button: MouseButton,
    pub(crate) count: u8,
    pub(crate) modifiers: Vec<Modifier>,
}

impl Default for ClickOptions {
    /// One left click.
    fn default() -> Self {
        Self {
            button: MouseButton::Left,
            count: 1,
            modifiers: Vec::new(),
        }
    }
}

impl ClickOptions {
    /// ` (right button, 2 clicks, Control+)`-like words for a reply; empty
    /// for one plain left click.
    fn described(&self) -> String {
        let mut parts = Vec::new();
        if self.button != MouseButton::Left {
            parts.push(format!("{} button", self.button.cdp()));
        }
        if self.count > 1 {
            parts.push(format!("{} clicks", self.count));
        }
        if !self.modifiers.is_empty() {
            let chord = super::keys::chord(&self.modifiers);
            parts.push(format!("with {}", chord.trim_end_matches('+')));
        }
        if parts.is_empty() {
            return String::new();
        }
        format!(" ({})", parts.join(", "))
    }
}

impl ClickElementTool {
    /// The click asked for, or why it makes no sense.
    pub(crate) fn options(&self) -> Result<ClickOptions, CallToolError> {
        let count = self.click_count.unwrap_or(1);
        if !(1..=3).contains(&count) {
            return Err(CallToolError::from_message(format!(
                "click_count must be 1, 2 or 3, got {count}"
            )));
        }
        Ok(ClickOptions {
            button: self.button.unwrap_or_default(),
            count,
            modifiers: self.modifiers.clone().unwrap_or_default(),
        })
    }

    pub async fn handle(
        params: CallToolRequestParams,
        handler: &McpHandler,
    ) -> Result<CallToolResult, CallToolError> {
        let ids = |a: &Self| (a.instance_id.clone(), a.tab_id.clone());
        let (args, target) = crate::handler::args_and_target(params, handler, ids).await?;
        let options = args.options()?;
        click(&target, &args.selector, &options).await
    }
}

/// Clicks the element `selector` names on Chrome (`click_element`, and the
/// checkboxes of `fill_form`).
pub(crate) async fn click(
    target: &CdpTarget,
    selector: &str,
    options: &ClickOptions,
) -> Result<CallToolResult, CallToolError> {
    let point = checked_point(target, selector, "clicked").await?;
    let clicking = press_and_release(target, point, options);
    let interrupted = super::keys::holding(target, &options.modifiers, clicking).await?;
    Ok(clicked_reply(
        selector,
        point,
        options,
        interrupted.as_ref(),
    ))
}

/// Presses and releases the button at `point`, `options.count` times; what
/// interrupted the click (a dialog or a pause its handlers caused), if
/// anything. The click still happened: the page handles the events once it
/// runs again.
async fn press_and_release(
    target: &CdpTarget,
    (x, y): (f64, f64),
    options: &ClickOptions,
) -> Result<Option<Interruption>, CallToolError> {
    let modifiers = super::keys::cdp_modifiers(&options.modifiers);
    let button = options.button.cdp();
    let event = |kind, count| json!({"type": kind, "button": button, "x": x, "y": y, "clickCount": count, "modifiers": modifiers});
    let method = "Input.dispatchMouseEvent";
    for count in 1..=options.count {
        let pressed = target
            .send_interruptible(method, event("mousePressed", count))
            .await;
        let released = match pressed {
            Ok(Ok(_)) => {
                target
                    .send_interruptible(method, event("mouseReleased", count))
                    .await
            }
            other => {
                // A press that failed or was interrupted may still be
                // delivered once the page runs again: so is the release, or
                // the button stays held.
                let (target, release) = (target.clone(), event("mouseReleased", count));
                tokio::spawn(async move { target.send_raw_command(method, release).await });
                other
            }
        };
        match released {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => {
                let failure = crate::handler::cdp_failure("CDP Error", &e);
                return Err(CallToolError::from_message(failure));
            }
            Err(interruption) => return Ok(Some(interruption)),
        }
    }
    Ok(None)
}

/// The reply once the click was sent; a click a dialog or a pause
/// interrupted says so, with its JSON after the first line.
pub(crate) fn clicked_reply(
    selector: &str,
    (x, y): (f64, f64),
    options: &ClickOptions,
    interrupted: Option<&Interruption>,
) -> CallToolResult {
    let how = options.described();
    let done =
        format!("Successfully clicked{how} on element '{selector}' at coordinates ({x}, {y})");
    let text = interrupted.map_or_else(|| done.clone(), |stopped| stopped.after(&done));
    CallToolResult::text_content(vec![text.into()])
}

/// How long the page gets to show what a pointer over a covered element
/// reveals (hover toolbars, menus) before the point is checked again.
pub(crate) const HOVER_REVEAL: std::time::Duration = std::time::Duration::from_millis(100);

/// The point of a `covered` click check, where the mouse goes before the
/// check runs again: controls shown only on hover (a message's actions, a
/// row's toolbar) are under an element that gives way once hovered.
pub(crate) fn covered_at(value: &Value) -> Option<(f64, f64)> {
    (value["status"].as_str() == Some("covered"))
        .then(|| Some((value["x"].as_f64()?, value["y"].as_f64()?)))
        .flatten()
}

/// The point where `selector`'s element can be `action` on Chrome (clicked,
/// hovered): checked by the shared snippet, and when covered checked again
/// with the mouse over it ([`covered_at`]).
pub(crate) async fn checked_point(
    target: &CdpTarget,
    selector: &str,
    action: &str,
) -> Result<(f64, f64), CallToolError> {
    let expression = crate::util::js::click_point(selector);
    let mut point = crate::handler::snippet_value(target, &expression).await?;
    if let Some((x, y)) = covered_at(&point) {
        let moved = json!({"type": "mouseMoved", "x": x, "y": y, "button": "none"});
        crate::handler::send_or(target, "Input.dispatchMouseEvent", moved, "CDP Error").await?;
        tokio::time::sleep(HOVER_REVEAL).await;
        point = crate::handler::snippet_value(target, &expression).await?;
    }
    click_point(&point, selector, action)
}

/// The point the click snippet (`util::js::click_point`) checked, or why
/// the element must not be `action` (clicked, hovered).
pub(crate) fn click_point(
    value: &Value,
    selector: &str,
    action: &str,
) -> Result<(f64, f64), CallToolError> {
    let fail = |message: String| Err(CallToolError::from_message(message));
    let point = (value["x"].as_f64(), value["y"].as_f64());
    match (value["status"].as_str(), point) {
        (Some("ok"), (Some(x), Some(y))) => Ok((x, y)),
        (None | Some("missing"), _) => fail(format!(
            "Element not found for selector: {selector}{}",
            crate::util::js::ref_hint(selector)
        )),
        (Some("hidden"), _) => fail(format!(
            "Element '{selector}' is not visible (display:none, visibility:hidden or zero size); nothing was {action}. Click a visible element instead, such as its label."
        )),
        (Some("outside"), _) => fail(format!(
            "Element '{selector}' is outside the viewport even after scrolling it into view; nothing was {action}."
        )),
        (Some("covered"), (Some(x), Some(y))) => fail(format!(
            "Element '{selector}' is covered by '{}' at ({x}, {y}); nothing was {action}. Close or remove the covering element first, or click it if that is the intent.",
            value["by"].as_str().unwrap_or("another element")
        )),
        _ => fail("Could not determine coordinates for the element.".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_mcp_sdk::schema::CallToolRequestParams;
    use serde_json::json;

    #[tokio::test]
    async fn test_click_element_params_deserialization() {
        let params: Result<CallToolRequestParams, _> = serde_json::from_value(json!({
            "name": "click_element",
            "arguments": {
                "selector": "#login-button"
            }
        }));
        assert!(params.is_ok());
    }

    #[tokio::test]
    async fn test_click_element_tool_deserialization() {
        let tool: Result<ClickElementTool, _> = serde_json::from_value(json!({
            "selector": "#login-button"
        }));
        assert!(tool.is_ok());
        assert_eq!(tool.unwrap().selector, "#login-button");
    }

    /// A Chrome mock whose click snippet answers `point` (by value).
    async fn page_answering(
        point: Value,
    ) -> &'static cdp_browser_lite::test_support::mock_devtools::MockDevTools {
        let mock = crate::handler::cdp_domains::tests::spawn_recording_mock().await;
        mock.respond_with(
            "Runtime.evaluate",
            move |_| json!({"result": {"result": {"type": "object", "value": point.clone()}}}),
        );
        mock
    }

    async fn click(
        mock: &cdp_browser_lite::test_support::mock_devtools::MockDevTools,
    ) -> Result<CallToolResult, CallToolError> {
        let handler = crate::handler::cdp_domains::tests::mock_handler(mock.http_port);
        let params =
            crate::handler::cdp_domains::tests::call("click_element", json!({"selector": "#b"}));
        ClickElementTool::handle(params, &handler).await
    }

    /// The click check's reply: the point is free, or covered by the row.
    fn check_reply(free: bool) -> Value {
        let value = if free {
            json!({"status": "ok", "x": 30, "y": 40})
        } else {
            json!({"status": "covered", "by": "div.row", "x": 30, "y": 40})
        };
        json!({"result": {"result": {"type": "object", "value": value}}})
    }

    /// ISSUE-26: a covered element gets the mouse first and is checked
    /// again: a control shown only on hover is then on top and clicked; one
    /// still covered fails without a click.
    #[tokio::test]
    async fn covered_elements_get_the_mouse_first() {
        for revealed in [true, false] {
            // Given an element covered until the mouse is over it (or always)
            let mock = crate::handler::cdp_domains::tests::spawn_recording_mock().await;
            let checks = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let counted = checks.clone();
            mock.respond_with("Runtime.evaluate", move |_| {
                let n = counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                check_reply(n > 0 && revealed)
            });
            // When it is clicked
            let outcome = click(mock).await;
            // Then the mouse moved to it first, and the click happens only when revealed
            let events: Vec<String> = mock
                .commands_named("Input.dispatchMouseEvent")
                .iter()
                .map(|c| c.params["type"].as_str().unwrap_or_default().to_string())
                .collect();
            assert_eq!(
                events.first().map(String::as_str),
                Some("mouseMoved"),
                "{events:?}"
            );
            assert_eq!(checks.load(std::sync::atomic::Ordering::SeqCst), 2);
            assert_eq!(
                events.contains(&"mousePressed".to_string()),
                revealed,
                "{events:?}"
            );
            assert_eq!(outcome.is_ok(), revealed);
        }
    }

    /// AUD-IN-03: a covered element fails naming what covers it, and no
    /// mouse event is sent (the overlay used to get the click).
    #[tokio::test]
    async fn covered_element_is_not_clicked() {
        // Given "#b" under an overlay
        let covered = json!({"status": "covered", "by": "div#overlay", "x": 81, "y": 240});
        let mock = page_answering(covered).await;
        // When click_element targets it
        let error = click(mock).await.unwrap_err().to_string();
        // Then it fails naming the overlay and nothing was clicked
        assert_eq!(
            error,
            "Element '#b' is covered by 'div#overlay' at (81, 240); nothing was clicked. Close or remove the covering element first, or click it if that is the intent."
        );
        // The mouse only moved there (to reveal a hover control, ISSUE-26)
        let events: Vec<_> = mock
            .commands_named("Input.dispatchMouseEvent")
            .into_iter()
            .map(|c| c.params["type"].clone())
            .collect();
        assert_eq!(events, [json!("mouseMoved")]);
    }

    /// AUD-IN-02: a hidden element fails (Chrome used to click at (0, 0)).
    #[tokio::test]
    async fn hidden_element_is_not_clicked() {
        let mock = page_answering(json!({"status": "hidden"})).await;
        let error = click(mock).await.unwrap_err().to_string();
        assert!(error.starts_with("Element '#b' is not visible"), "{error}");
        assert!(mock.commands_named("Input.dispatchMouseEvent").is_empty());
    }

    /// AUD-IN-01: the element is scrolled into view, then clicked at the
    /// point the snippet checked.
    #[tokio::test]
    async fn visible_element_is_clicked_at_the_checked_point() {
        let mock = page_answering(json!({"status": "ok", "x": 16, "y": 497})).await;
        let reply = click(mock).await.unwrap();
        assert_eq!(
            crate::handler::cdp_domains::tests::reply_text(&reply),
            "Successfully clicked on element '#b' at coordinates (16, 497)"
        );
        let expression = mock.commands_named("Runtime.evaluate")[0].params["expression"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(expression.contains("scrollIntoView"), "{expression}");
        assert!(expression.contains("elementFromPoint"), "{expression}");
        let events = mock.commands_named("Input.dispatchMouseEvent");
        assert_eq!(events.len(), 2);
        assert_eq!(
            (events[0].params["x"].clone(), events[0].params["y"].clone()),
            (json!(16.0), json!(497.0))
        );
    }

    /// ISSUE-7 (Chrome): a double right click with Control sends two
    /// press/release pairs counting 1 then 2, with the button and the
    /// modifier bits; the reply says how it clicked.
    #[tokio::test]
    async fn click_options_reach_the_mouse_events() {
        let mock = page_answering(json!({"status": "ok", "x": 16, "y": 497})).await;
        let handler = crate::handler::cdp_domains::tests::mock_handler(mock.http_port);
        let args = json!({"selector": "#b", "button": "right", "click_count": 2, "modifiers": ["Control"]});
        let params = crate::handler::cdp_domains::tests::call("click_element", args);
        let reply = ClickElementTool::handle(params, &handler).await.unwrap();
        let text = crate::handler::cdp_domains::tests::reply_text(&reply);
        assert_eq!(
            text,
            "Successfully clicked (right button, 2 clicks, with Control) on element '#b' at coordinates (16, 497)"
        );
        let events: Vec<(String, u64)> = mock
            .commands_named("Input.dispatchMouseEvent")
            .iter()
            .map(|e| {
                (
                    e.params["type"].as_str().unwrap().to_string(),
                    e.params["clickCount"].as_u64().unwrap(),
                )
            })
            .collect();
        let expected = [
            ("mousePressed", 1),
            ("mouseReleased", 1),
            ("mousePressed", 2),
            ("mouseReleased", 2),
        ];
        assert_eq!(events, expected.map(|(k, n)| (k.to_string(), n)));
        let first = &mock.commands_named("Input.dispatchMouseEvent")[0].params;
        assert_eq!(
            (first["button"].clone(), first["modifiers"].clone()),
            (json!("right"), json!(2))
        );
        let error =
            serde_json::from_value::<ClickElementTool>(json!({"selector": "#b", "click_count": 4}))
                .unwrap()
                .options()
                .unwrap_err()
                .to_string();
        assert_eq!(error, "click_count must be 1, 2 or 3, got 4");
    }

    /// ISSUE-11: a click a dialog or a pause interrupted is a success (the
    /// click happened) that says so, with its JSON after the first line.
    #[test]
    fn interrupted_click_is_a_noted_success() {
        // Given a click that opened a confirm, and one that paused
        let dialog = Interruption::Dialog(crate::handler::dialogs::OpenDialog::from_chrome(
            &json!({"type": "confirm", "message": "Delete?"}),
        ));
        let paused = Interruption::Paused(None);
        // When their replies are built
        let once = ClickOptions::default();
        let reply = clicked_reply("#del", (4.0, 9.0), &once, Some(&dialog));
        let text = crate::handler::cdp_domains::tests::reply_text(&reply);
        // Then they succeed, noting the dialog or the pause
        assert_ne!(reply.is_error, Some(true));
        assert!(
            text.starts_with("Successfully clicked on element '#del' at coordinates (4, 9). The page opened a JavaScript confirm dialog (\"Delete?\")"),
            "{text}"
        );
        let body: Value = serde_json::from_str(&text[text.find('{').unwrap()..]).unwrap();
        assert_eq!(body, dialog.to_json());
        let reply = clicked_reply("#b", (1.0, 2.0), &once, Some(&paused));
        let text = crate::handler::cdp_domains::tests::reply_text(&reply);
        assert!(
            text.contains("(1, 2). The page paused in the debugger:"),
            "{text}"
        );
    }

    /// ISSUE-1: a click interrupted on the press (a breakpoint in a
    /// `mousedown` handler) still sends the release, delivered once the page
    /// runs again, so a later click does not start with the button held.
    #[tokio::test]
    async fn interrupted_press_still_releases_the_button() {
        // Given a press Chrome fails
        let mock = page_answering(json!({"status": "ok", "x": 16, "y": 497})).await;
        mock.respond_with("Input.dispatchMouseEvent", |params| {
            if params["type"] == "mousePressed" {
                json!({"error": {"code": -32000, "message": "interrupted"}})
            } else {
                json!({})
            }
        });
        // When click_element runs
        assert!(click(mock).await.is_err());
        // Then the release follows in the background
        for _ in 0..100 {
            if mock.commands_named("Input.dispatchMouseEvent").len() == 2 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let kinds: Vec<Value> = mock
            .commands_named("Input.dispatchMouseEvent")
            .iter()
            .map(|c| c.params["type"].clone())
            .collect();
        assert_eq!(kinds, [json!("mousePressed"), json!("mouseReleased")]);
    }
}
