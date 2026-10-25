use crate::handler::McpHandler;
use rust_mcp_sdk::{
    macros,
    schema::{CallToolError, CallToolRequestParams, CallToolResult},
};
use serde_json::{Value, json};

#[macros::mcp_tool(
    name = "click_element",
    description = "Triggers a native mouse click event on a DOM element identified by CSS selector, executing click handlers and form submissions. Side effects: may trigger page navigation, form submission, or modify DOM state. Prerequisites: element must exist and be visible; it is scrolled into view first, and the call fails without clicking when the element has no visible box or another element (an overlay, a cookie banner) covers the point. Returns: success confirmation with click coordinates (viewport CSS pixels). Use this to interact with buttons, links, checkboxes. Alternatives: 'fill_input' for text input, 'evaluate_js' for complex interactions."
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
        for kind in ["mousePressed", "mouseReleased"] {
            let event = json!({"type": kind, "button": "left", "x": x, "y": y, "clickCount": 1});
            crate::handler::send_or(&target, "Input.dispatchMouseEvent", event, "CDP Error")
                .await?;
        }
        Ok(clicked_reply(&args.selector, x, y))
    }
}

/// The reply once the click was sent.
pub(crate) fn clicked_reply(selector: &str, x: f64, y: f64) -> CallToolResult {
    CallToolResult::text_content(vec![
        format!("Successfully clicked on element '{selector}' at coordinates ({x}, {y})").into(),
    ])
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
}
