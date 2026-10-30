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
    description = "Triggers a native mouse click event on a DOM element identified by CSS selector, executing click handlers and form submissions. Side effects: may trigger page navigation, form submission, or modify DOM state. Prerequisites: element must exist and be visible; it is scrolled into view first, and the call fails without clicking when the element has no visible box or another element (an overlay, a cookie banner) covers the point. Returns: success confirmation with click coordinates (viewport CSS pixels). A JavaScript dialog the click opens, or a debugger pause its handlers hit, does not fail it (the click happened): the reply notes it, followed by JSON {dialog: {type, message, default_prompt?}} (close it with 'handle_dialog') or {paused: true, location: {url, line_number, column_number} | null} (inspect with 'evaluate_on_call_frame', continue with 'resume'). Use this to interact with buttons, links, checkboxes. Alternatives: 'fill_input' for text input, 'evaluate_js' for complex interactions."
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct ClickElementTool {
    /// Chrome instance id from open_instance/list_instances. Omit for the default instance.
    pub instance_id: Option<String>,
    /// The Tab ID of the target tab. Omit to use the active tab.
    pub tab_id: Option<String>,
    /// CSS selector identifying the target element. Constraints: valid CSS selector string; the first matching element is clicked. Interactions: fails when that element is hidden or covered. Defaults to: None (required).
    pub selector: String,
}

impl ClickElementTool {
    pub async fn handle(
        params: CallToolRequestParams,
        handler: &McpHandler,
    ) -> Result<CallToolResult, CallToolError> {
        let args: ClickElementTool = crate::handler::parse_args(params)?;
        let target =
            crate::handler::resolve_target(handler, args.instance_id.clone(), args.tab_id.clone())
                .await?;
        let expression = crate::util::js::click_point(&args.selector);
        let point = crate::handler::snippet_value(&target, &expression).await?;
        let (x, y) = click_point(&point, &args.selector)?;
        let interrupted = press_and_release(&target, x, y).await?;
        Ok(clicked_reply(&args.selector, x, y, interrupted.as_ref()))
    }
}

/// Presses and releases the left button at `(x, y)`; what interrupted the
/// click (a dialog or a pause its handlers caused), if anything. The click
/// still happened: the page handles the events once it runs again.
async fn press_and_release(
    target: &CdpTarget,
    x: f64,
    y: f64,
) -> Result<Option<Interruption>, CallToolError> {
    let event = |kind| json!({"type": kind, "button": "left", "x": x, "y": y, "clickCount": 1});
    let method = "Input.dispatchMouseEvent";
    let pressed = target
        .send_interruptible(method, event("mousePressed"))
        .await;
    let released = match pressed {
        Ok(Ok(_)) => {
            target
                .send_interruptible(method, event("mouseReleased"))
                .await
        }
        other => {
            // A press that failed or was interrupted may still be delivered
            // once the page runs again: so is the release, or the button
            // stays held.
            let (target, release) = (target.clone(), event("mouseReleased"));
            tokio::spawn(async move { target.send_raw_command(method, release).await });
            other
        }
    };
    match released {
        Ok(Ok(_)) => Ok(None),
        Ok(Err(e)) => Err(CallToolError::from_message(crate::handler::cdp_failure(
            "CDP Error",
            &e,
        ))),
        Err(interruption) => Ok(Some(interruption)),
    }
}

/// The reply once the click was sent; a click a dialog or a pause
/// interrupted says so, with its JSON after the first line.
pub(crate) fn clicked_reply(
    selector: &str,
    x: f64,
    y: f64,
    interrupted: Option<&Interruption>,
) -> CallToolResult {
    let done = format!("Successfully clicked on element '{selector}' at coordinates ({x}, {y})");
    let text = match interrupted {
        None => done,
        Some(interruption) => {
            let body = serde_json::to_string_pretty(&interruption.to_json()).unwrap_or_default();
            format!("{done}. {}\n{body}", interruption.note())
        }
    };
    CallToolResult::text_content(vec![text.into()])
}

/// The point the click snippet (`util::js::click_point`) checked, or why
/// the element must not be clicked.
pub(crate) fn click_point(value: &Value, selector: &str) -> Result<(f64, f64), CallToolError> {
    let fail = |message: String| Err(CallToolError::from_message(message));
    let point = (value["x"].as_f64(), value["y"].as_f64());
    match (value["status"].as_str(), point) {
        (Some("ok"), (Some(x), Some(y))) => Ok((x, y)),
        (None | Some("missing"), _) => fail(format!("Element not found for selector: {selector}")),
        (Some("hidden"), _) => fail(format!(
            "Element '{selector}' is not visible (display:none, visibility:hidden or zero size); nothing was clicked. Click a visible element instead, such as its label."
        )),
        (Some("outside"), _) => fail(format!(
            "Element '{selector}' is outside the viewport even after scrolling it into view; nothing was clicked."
        )),
        (Some("covered"), (Some(x), Some(y))) => fail(format!(
            "Element '{selector}' is covered by '{}' at ({x}, {y}); nothing was clicked. Close or remove the covering element first, or click it if that is the intent.",
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
        assert!(mock.commands_named("Input.dispatchMouseEvent").is_empty());
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
        let reply = clicked_reply("#del", 4.0, 9.0, Some(&dialog));
        let text = crate::handler::cdp_domains::tests::reply_text(&reply);
        // Then they succeed, noting the dialog or the pause
        assert_ne!(reply.is_error, Some(true));
        assert!(
            text.starts_with("Successfully clicked on element '#del' at coordinates (4, 9). The page opened a JavaScript confirm dialog (\"Delete?\")"),
            "{text}"
        );
        let body: Value = serde_json::from_str(&text[text.find('{').unwrap()..]).unwrap();
        assert_eq!(body, dialog.to_json());
        let text = crate::handler::cdp_domains::tests::reply_text(&clicked_reply(
            "#b",
            1.0,
            2.0,
            Some(&paused),
        ));
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
