use crate::handler::McpHandler;
use rust_mcp_sdk::{
    macros,
    schema::{CallToolError, CallToolRequestParams, CallToolResult},
};
use serde_json::json;

#[macros::mcp_tool(
    name = "hover",
    description = "Moves the mouse over an element (scrolled into view; fails when hidden or covered) so hover menus, tooltips and :hover styles appear.",
    destructive_hint = false
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct HoverTool {
    /// Default: 'default'.
    pub instance_id: Option<String>,
    /// Default: the active tab.
    pub tab_id: Option<String>,
    /// CSS selector, 'host >>> inner' (into an open shadow root or same-origin iframe) or snapshot ref (e12).
    pub selector: String,
}

impl HoverTool {
    pub async fn handle(
        params: CallToolRequestParams,
        handler: &McpHandler,
    ) -> Result<CallToolResult, CallToolError> {
        let ids = |a: &Self| (a.instance_id.clone(), a.tab_id.clone());
        let (args, target) = crate::handler::args_and_target(params, handler, ids, |a: &Self| {
            Some(a.selector.clone())
        })
        .await?;
        let point = super::click_element::checked_point(&target, &args.selector, "hovered");
        let (x, y) = point.await?;
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
    /// ISSUE-37: hovering a disabled control is fine (tooltips explain why
    /// it is disabled): it is not refused nor waited for.
    #[tokio::test]
    async fn disabled_controls_can_be_hovered() {
        let mock = crate::handler::cdp_domains::tests::spawn_recording_mock().await;
        mock.respond_with("Runtime.evaluate", |_| {
            let value = serde_json::json!({"status": "ok", "x": 3, "y": 4, "disabled": true});
            serde_json::json!({"result": {"result": {"type": "object", "value": value}}})
        });
        let handler = crate::handler::cdp_domains::tests::mock_handler(mock.http_port);
        let params = crate::handler::cdp_domains::tests::call(
            "hover",
            serde_json::json!({"selector": "#d"}),
        );
        let started = std::time::Instant::now();
        super::HoverTool::handle(params, &handler).await.unwrap();
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        assert_eq!(mock.commands_named("Runtime.evaluate").len(), 1);
    }

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
        // The covered element only got the reveal move (ISSUE-26), and failed
        assert_eq!(mock.commands_named("Input.dispatchMouseEvent").len(), 2);
    }
}
