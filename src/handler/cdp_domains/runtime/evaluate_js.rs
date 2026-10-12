use crate::handler::McpHandler;
use rust_mcp_sdk::{
    macros,
    schema::{CallToolError, CallToolRequestParams, CallToolResult},
};
use serde_json::json;

#[macros::mcp_tool(
    name = "evaluate_js",
    description = "Executes arbitrary JavaScript code in the page context, returning evaluated results as JSON-serializable values. Side effects: may modify DOM, state, or trigger network requests. Prerequisites: requires an active Chrome tab; script context must allow execution. Returns: JSON-serialized return value (or error if promise rejected). Use this for dynamic inspection, DOM manipulation, complex interactions. Alternatives: 'inspect_dom' for read-only DOM queries, 'click_element' for UI interactions."
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct EvaluateJsTool {
    /// Chrome instance id from open_instance/list_instances. Omit for the default instance.
    pub instance_id: Option<String>,
    /// The Tab ID of the target tab. Omit to use the active tab.
    pub tab_id: Option<String>,
    /// JavaScript code expression to execute. Constraints: valid JavaScript (single expression or IIFE). Interactions: automatically awaits promises; 'returnByValue' returns serialized results. Defaults to: None (required).
    pub expression: String,
}

impl EvaluateJsTool {
    pub async fn handle(
        params: CallToolRequestParams,
        handler: &McpHandler,
    ) -> Result<CallToolResult, CallToolError> {
        let args: EvaluateJsTool = crate::handler::parse_args(params)?;
        let ids = crate::handler::TargetArgs {
            instance_id: args.instance_id,
            tab_id: args.tab_id,
        };
        let params = json!({
            "expression": args.expression,
            "returnByValue": true,
            "awaitPromise": true
        });
        let command = crate::handler::Command {
            method: "Runtime.evaluate",
            params,
            failure: "CDP Error",
        };
        let val = crate::handler::run_on_target(handler, ids, command).await?;
        Ok(CallToolResult::text_content(vec![
            crate::handler::reply_text("", &val).into(),
        ]))
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
    async fn test_evaluate_js_params_deserialization() {
        let params: Result<CallToolRequestParams, _> = serde_json::from_value(json!({
            "name": "evaluate_js",
            "arguments": {
                "expression": "console.log('test')"
            }
        }));
        assert!(params.is_ok());
    }

    #[tokio::test]
    async fn test_evaluate_js_tool_deserialization() {
        let tool: Result<EvaluateJsTool, _> = serde_json::from_value(json!({
            "expression": "2 + 2"
        }));
        assert!(tool.is_ok());
        assert_eq!(tool.unwrap().expression, "2 + 2");
    }

    #[tokio::test]
    async fn test_evaluate_js_missing_expression_fails() {
        let handler = McpHandler::new_test();
        let params: CallToolRequestParams = serde_json::from_value(json!({
            "name": "evaluate_js",
            "arguments": {}
        }))
        .unwrap();

        let result = EvaluateJsTool::handle(params, &handler).await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("missing field `expression`")
        );
    }

    #[tokio::test]
    async fn test_evaluate_js_handle() {
        let port = spawn_mock_chrome_server().await;

        let mut handler = McpHandler::new_test();
        Arc::get_mut(&mut handler.default_session)
            .unwrap()
            .chrome_manager = Arc::new(Mutex::new(MockChromeManager::new(port)));

        let params: CallToolRequestParams = serde_json::from_value(json!({
            "name": "evaluate_js",
            "arguments": {
                "expression": "2 + 2"
            }
        }))
        .unwrap();

        let result = EvaluateJsTool::handle(params, &handler).await;
        assert!(result.is_ok(), "Handle should succeed: {:?}", result.err());

        let call_result = result.unwrap();
        assert!(!call_result.content.is_empty());
        // TOL-08: the reply is JSON, not the Rust Debug of the protocol message
        let text = call_result.content[0]
            .as_text_content()
            .unwrap()
            .text
            .clone();
        assert!(!text.contains("WsResponse"), "{text}");
        assert!(
            serde_json::from_str::<serde_json::Value>(&text).is_ok(),
            "{text}"
        );
    }
}
