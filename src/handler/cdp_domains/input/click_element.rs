use crate::handler::McpHandler;
use rust_mcp_sdk::{
    macros,
    schema::{CallToolError, CallToolRequestParams, CallToolResult},
};
use serde_json::json;

#[macros::mcp_tool(
    name = "click_element",
    description = "Triggers a native mouse click event on a DOM element identified by CSS selector, executing click handlers and form submissions. Side effects: may trigger page navigation, form submission, or modify DOM state. Prerequisites: element must exist and be visible (getBoundingClientRect must return valid coordinates). Returns: success confirmation with click coordinates. Use this to interact with buttons, links, checkboxes. Alternatives: 'fill_input' for text input, 'evaluate_js' for complex interactions."
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct ClickElementTool {
    /// Chrome instance id from open_instance/list_instances. Omit for the default instance.
    pub instance_id: Option<String>,
    /// The Tab ID of the target tab. Omit to use the active tab.
    pub tab_id: Option<String>,
    /// CSS selector identifying the target element. Constraints: valid CSS selector string matching a single DOM element. Interactions: must resolve to exactly one visible element or operation fails. Defaults to: None (required).
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
        let expression = crate::util::js::element_center(&args.selector);
        let params = json!({"expression": expression, "returnByValue": true});
        let val = crate::handler::send_or(&target, "Runtime.evaluate", params, "CDP Error").await?;
        let (x, y) = element_point(&val, &args.selector)?;
        for kind in ["mousePressed", "mouseReleased"] {
            let event = json!({"type": kind, "button": "left", "x": x, "y": y, "clickCount": 1});
            crate::handler::send_or(&target, "Input.dispatchMouseEvent", event, "CDP Error")
                .await?;
        }
        Ok(CallToolResult::text_content(vec![
            format!(
                "Successfully clicked on element '{}' at coordinates ({}, {})",
                args.selector, x, y
            )
            .into(),
        ]))
    }
}

/// The `{x, y}` returned by the element-centre snippet, or why there is none.
fn element_point(
    reply: &cdp_browser_lite::WsResponse,
    selector: &str,
) -> Result<(f64, f64), CallToolError> {
    let not_found =
        || CallToolError::from_message(format!("Element not found for selector: {}", selector));
    let result = reply.result.as_ref().and_then(|r| r.get("result"));
    let is_null = result
        .and_then(|r| r.get("subtype"))
        .and_then(|s| s.as_str())
        == Some("null")
        && result.and_then(|r| r.get("type")).and_then(|t| t.as_str()) == Some("object");
    if is_null {
        return Err(not_found());
    }
    let value = result.and_then(|r| r.get("value")).ok_or_else(not_found)?;
    match (
        value.get("x").and_then(|v| v.as_f64()),
        value.get("y").and_then(|v| v.as_f64()),
    ) {
        (Some(x), Some(y)) => Ok((x, y)),
        _ => Err(CallToolError::from_message(
            "Could not determine coordinates for the element.",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::cdp_domains::tests::spawn_mock_chrome_server;
    use crate::handler::chrome_instance::MockChromeManager;
    use rust_mcp_sdk::schema::CallToolRequestParams;
    use serde_json::json;
    use std::sync::Arc;
    use tokio::sync::Mutex;

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

    #[tokio::test]
    async fn test_click_element_handle() {
        let port = spawn_mock_chrome_server().await;

        let mut handler = McpHandler::new_test();
        Arc::get_mut(&mut handler.default_session)
            .unwrap()
            .chrome_manager = Arc::new(Mutex::new(MockChromeManager::new(port)));

        let params: CallToolRequestParams = serde_json::from_value(json!({
            "name": "click_element",
            "arguments": {
                "selector": "#test"
            }
        }))
        .unwrap();

        let result = ClickElementTool::handle(params, &handler).await;
        // In the mock server, if the result is not exactly right, we might get an error because the mock server
        // doesn't return the mocked coordinates. We'll update the mock server to return valid coords for this test.
        assert!(result.is_ok(), "Handle should succeed: {:?}", result.err());
    }
}
