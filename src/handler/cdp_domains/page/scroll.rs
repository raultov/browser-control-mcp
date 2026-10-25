use crate::handler::McpHandler;
use rust_mcp_sdk::{
    macros,
    schema::{CallToolError, CallToolRequestParams, CallToolResult},
};
use serde_json::{Value, json};

#[macros::mcp_tool(
    name = "scroll",
    description = "Scrolls the page by pixel offset, viewport pages, or to a specific element using CSS selector. Side effects: modifies DOM scroll position (observable but reversible). Prerequisites: requires an active tab with content. Returns: a first line ('Successfully scrolled.', or why the window did not move) and JSON {before: {x, y}, after: {x, y}, element_in_view (selector only)} with the window scroll positions in CSS pixels; pixel and page scrolls move the window, so on pages that scroll an inner element pass a 'selector' inside it. Use this to navigate within long pages or bring elements into view. Alternatives: 'click_element' to trigger scroll by clicking, 'evaluate_js' for custom scroll logic."
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct ScrollTool {
    /// Chrome instance id from open_instance/list_instances. Omit for the default instance.
    pub instance_id: Option<String>,
    /// The Tab ID of the target tab. Omit to use the active tab.
    pub tab_id: Option<String>,
    /// Horizontal scroll distance in pixels. Constraints: integer (positive=right, negative=left). Interactions: ignored if 'selector' is provided; combined with 'y' or 'pages' for diagonal scrolling. Defaults to: 0 (no horizontal scroll).
    pub x: Option<i32>,
    /// Vertical scroll distance in pixels. Constraints: integer (positive=down, negative=up). Interactions: ignored if 'selector' or 'pages' is provided; overridden by 'pages'. Defaults to: 0 (no vertical scroll).
    pub y: Option<i32>,
    /// Number of viewport heights to scroll vertically. Constraints: float (e.g., 1.5 = 1.5× viewport height; negative scrolls up). Interactions: takes precedence over 'y' parameter if both provided; ignored if 'selector' provided. Defaults to: None.
    pub pages: Option<f64>,
    /// CSS selector of element to scroll into view. Constraints: valid CSS selector string. Interactions: takes precedence over 'x', 'y', 'pages' if provided; fails if element not found or not visible. Defaults to: None.
    pub selector: Option<String>,
}

impl ScrollTool {
    /// The JavaScript that performs the scroll (shared by every browser).
    /// Precedence: selector, then pages, then x/y.
    pub(crate) fn expression(&self) -> String {
        let dx = self.x.unwrap_or(0).to_string();
        if let Some(selector) = &self.selector {
            crate::util::js::scroll_into_view(selector)
        } else if let Some(pages) = self.pages {
            crate::util::js::scroll_by(&dx, &format!("window.innerHeight * {pages}"))
        } else {
            crate::util::js::scroll_by(&dx, &self.y.unwrap_or(0).to_string())
        }
    }

    /// The reply from the scroll snippet's outcome (shared by every browser):
    /// the window positions, and why nothing moved when it did not.
    pub(crate) fn reply(&self, outcome: &Value) -> Result<CallToolResult, CallToolError> {
        let selector = self.selector.as_deref().unwrap_or_default();
        match outcome["status"].as_str() {
            Some("done") => {}
            Some("hidden") => {
                return Err(CallToolError::from_message(format!(
                    "Element '{selector}' is not visible (no layout box); there is nothing to scroll to."
                )));
            }
            _ if self.selector.is_some() => {
                return Err(CallToolError::from_message(format!(
                    "Element not found for scrolling: {selector}"
                )));
            }
            _ => {
                return Err(CallToolError::from_message(
                    "The page returned no scroll positions.",
                ));
            }
        }
        let mut body = json!({"before": outcome["before"], "after": outcome["after"]});
        let first = if let Some(in_view) = outcome["in_view"].as_bool() {
            body["element_in_view"] = json!(in_view);
            if in_view {
                "Successfully scrolled.".to_string()
            } else {
                format!(
                    "'{selector}' is still outside the viewport (a fixed or clipped container hides it)."
                )
            }
        } else if outcome["before"] == outcome["after"] {
            "The window did not move: it is already at that edge, or the page scrolls an inner element (pass a 'selector' inside it to bring that element into view).".to_string()
        } else {
            "Successfully scrolled.".to_string()
        };
        let body = serde_json::to_string_pretty(&body).unwrap_or_default();
        Ok(CallToolResult::text_content(vec![
            format!("{first}\n{body}").into(),
        ]))
    }

    pub async fn handle(
        params: CallToolRequestParams,
        handler: &McpHandler,
    ) -> Result<CallToolResult, CallToolError> {
        let args: ScrollTool = crate::handler::parse_args(params)?;
        let target =
            crate::handler::resolve_target(handler, args.instance_id.clone(), args.tab_id.clone())
                .await?;
        let outcome = crate::handler::snippet_value(&target, &args.expression()).await?;
        args.reply(&outcome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_mcp_sdk::schema::CallToolRequestParams;
    use serde_json::json;

    #[tokio::test]
    async fn test_scroll_params_deserialization() {
        let params: Result<CallToolRequestParams, _> = serde_json::from_value(json!({
            "name": "scroll",
            "arguments": {
                "y": 500,
                "pages": 1.5,
                "selector": "#bottom"
            }
        }));
        assert!(params.is_ok());
    }

    /// A Chrome mock whose scroll snippet answers `outcome` (by value).
    async fn scroll_with(outcome: Value, args: Value) -> Result<CallToolResult, CallToolError> {
        let mock = crate::handler::cdp_domains::tests::spawn_recording_mock().await;
        mock.respond_with(
            "Runtime.evaluate",
            move |_| json!({"result": {"result": {"type": "object", "value": outcome.clone()}}}),
        );
        let handler = crate::handler::cdp_domains::tests::mock_handler(mock.http_port);
        let params = crate::handler::cdp_domains::tests::call("scroll", args);
        ScrollTool::handle(params, &handler).await
    }

    /// AUD-IN-15: a scroll that moved reports the positions before and after.
    #[tokio::test]
    async fn scroll_reports_positions() {
        let moved =
            json!({"status": "done", "before": {"x": 0, "y": 0}, "after": {"x": 0, "y": 500}});
        let reply = scroll_with(moved, json!({"y": 500})).await.unwrap();
        let text = crate::handler::cdp_domains::tests::reply_text(&reply);
        let (first, body) = text.split_once('\n').unwrap();
        assert_eq!(first, "Successfully scrolled.");
        let body: Value = serde_json::from_str(body).unwrap();
        assert_eq!(
            body,
            json!({"before": {"x": 0, "y": 0}, "after": {"x": 0, "y": 500}})
        );
    }

    /// AUD-IN-15: a window that did not move says so (it used to report
    /// success on pages that scroll an inner element).
    #[tokio::test]
    async fn scroll_that_did_not_move_says_so() {
        let still =
            json!({"status": "done", "before": {"x": 0, "y": 0}, "after": {"x": 0, "y": 0}});
        let reply = scroll_with(still, json!({"y": 500})).await.unwrap();
        let text = crate::handler::cdp_domains::tests::reply_text(&reply);
        assert!(text.starts_with("The window did not move"), "{text}");
    }

    /// AUD-IN-15: scrolling to a hidden element fails.
    #[tokio::test]
    async fn scroll_to_hidden_element_fails() {
        let hidden = json!({"status": "hidden"});
        let error = scroll_with(hidden, json!({"selector": "#h"}))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.starts_with("Element '#h' is not visible"), "{error}");
    }

    /// AUD-IN-16: `x` is kept when scrolling by pages.
    #[test]
    fn pages_keep_the_horizontal_offset() {
        let tool: ScrollTool = serde_json::from_value(json!({"x": 10, "pages": 1.5})).unwrap();
        let expression = tool.expression();
        assert!(expression.contains("left: 10,"), "{expression}");
        assert!(
            expression.contains("window.innerHeight * 1.5"),
            "{expression}"
        );
    }
}
