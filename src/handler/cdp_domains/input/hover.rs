use crate::handler::McpHandler;
use rust_mcp_sdk::{
    macros,
    schema::{CallToolError, CallToolRequestParams, CallToolResult},
};
use serde_json::json;

#[macros::mcp_tool(
    name = "hover",
    description = "Moves the mouse over an element (scrolled into view; fails when hidden or covered) so hover menus, tooltips and :hover styles appear."
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct HoverTool {
    /// Instance id; omit for the default instance.
    pub instance_id: Option<String>,
    /// Tab id; omit for the active tab.
    pub tab_id: Option<String>,
    /// Element: CSS selector, 'host >>> inner' (inside an open shadow root or same-origin iframe), or a snapshot ref (e12).
    pub selector: String,
}

impl HoverTool {
    pub async fn handle(
        params: CallToolRequestParams,
        handler: &McpHandler,
    ) -> Result<CallToolResult, CallToolError> {
        let ids = |a: &Self| (a.instance_id.clone(), a.tab_id.clone());
        let (args, target) = crate::handler::args_and_target(params, handler, ids).await?;
        let expression = crate::util::js::click_point(&args.selector);
        let point = crate::handler::snippet_value(&target, &expression).await?;
        let (x, y) = super::click_element::click_point(&point, &args.selector, "hovered")?;
        let moved = json!({"type": "mouseMoved", "x": x, "y": y, "button": "none"});
        crate::handler::send_or(&target, "Input.dispatchMouseEvent", moved, "CDP Error").await?;
        Ok(hovered_reply(&args.selector, (x, y)))
    }
}

/// The reply once the mouse is over the element.
pub(crate) fn hovered_reply(selector: &str, (x, y): (f64, f64)) -> CallToolResult {
    CallToolResult::text_content(vec![
        format!("Hovering over '{selector}' at ({x}, {y}).").into(),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ISSUE-7 (Chrome): the mouse moves to the point the click snippet
    /// checked; a covered element gets no move.
    #[tokio::test]
    async fn hover_moves_to_the_checked_point() {
        let mock = crate::handler::cdp_domains::tests::spawn_recording_mock().await;
        mock.respond_with("Runtime.evaluate", |params| {
            let covered = params["expression"]
                .as_str()
                .unwrap_or_default()
                .contains("#c");
            let value = if covered {
                json!({"status": "covered", "by": "div", "x": 1, "y": 2})
            } else {
                json!({"status": "ok", "x": 30, "y": 40})
            };
            json!({"result": {"result": {"type": "object", "value": value}}})
        });
        let handler = crate::handler::cdp_domains::tests::mock_handler(mock.http_port);
        let call = |selector| {
            crate::handler::cdp_domains::tests::call("hover", json!({"selector": selector}))
        };
        let reply = HoverTool::handle(call("#menu"), &handler).await.unwrap();
        let text = crate::handler::cdp_domains::tests::reply_text(&reply);
        assert_eq!(text, "Hovering over '#menu' at (30, 40).");
        let moved = &mock.commands_named("Input.dispatchMouseEvent")[0].params;
        assert_eq!(
            (moved["type"].clone(), moved["x"].clone()),
            (json!("mouseMoved"), json!(30.0))
        );
        let error = HoverTool::handle(call("#c"), &handler)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("nothing was hovered"), "{error}");
        assert_eq!(mock.commands_named("Input.dispatchMouseEvent").len(), 1);
    }
}
