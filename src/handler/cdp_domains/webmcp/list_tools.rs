use crate::handler::McpHandler;
use crate::handler::cdp_domains::webmcp::WebmcpAvailability;
use rust_mcp_sdk::{
    macros,
    schema::{CallToolError, CallToolRequestParams, CallToolResult},
};
use serde_json::{Value, json};

#[macros::mcp_tool(
    name = "webmcp_list_tools",
    description = "Lists the WebMCP tools the page registered, with schemas, frame ids and the page's annotations (readOnly, consequential, untrustedContent...). Chrome only (not available on Firefox).",
    read_only_hint = true
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct ListWebmcpToolsTool {
    /// Default: 'default'.
    pub instance_id: Option<String>,
    /// Default: the active tab.
    pub tab_id: Option<String>,
}

/// The tools as compact JSON (ISSUE-73: 7 tools cost ~1700 tokens):
/// `{frameId, tools}` when they share one frame, else `{tools}`, each with
/// its own;
/// no empty `required`, no false annotations.
pub(crate) fn listing(tools: &[crate::handler::cdp_domains::webmcp::WebmcpTool]) -> Value {
    let shared = tools
        .first()
        .map(|t| &t.frame_id)
        .filter(|frame| tools.iter().all(|t| &t.frame_id == *frame));
    let rows: Vec<Value> = tools
        .iter()
        .map(|tool| {
            let mut row = serde_json::to_value(tool).unwrap_or_default();
            if let Some(row) = row.as_object_mut() {
                compact(row, shared.is_some());
            }
            row
        })
        .collect();
    match shared {
        Some(frame) => json!({"frameId": frame, "tools": rows}),
        None => json!({"tools": rows}),
    }
}

/// Drops from a tool's JSON what says nothing: its frame when shared, an
/// empty `required`, false annotations (and the annotations when none is
/// left).
fn compact(row: &mut serde_json::Map<String, Value>, shared_frame: bool) {
    if shared_frame {
        row.remove("frameId");
    }
    if let Some(schema) = row.get_mut("inputSchema").and_then(Value::as_object_mut)
        && schema
            .get("required")
            .and_then(Value::as_array)
            .is_some_and(Vec::is_empty)
    {
        schema.remove("required");
    }
    if let Some(annotations) = row.get_mut("annotations").and_then(Value::as_object_mut) {
        annotations.retain(|_, flag| flag != &Value::Bool(false));
        if annotations.is_empty() {
            row.remove("annotations");
        }
    }
}

impl ListWebmcpToolsTool {
    pub async fn handle(
        params: CallToolRequestParams,
        handler: &McpHandler,
    ) -> Result<CallToolResult, CallToolError> {
        let tool: ListWebmcpToolsTool = serde_json::from_value(serde_json::Value::Object(
            params.arguments.unwrap_or_default(),
        ))
        .map_err(|e| CallToolError::from_message(format!("Failed to parse arguments: {}", e)))?;
        let session = handler.session(tool.instance_id.clone()).await?;

        let webmcp_state = session.webmcp_state(tool.tab_id.clone())?;
        let st = webmcp_state.lock().await;
        let mut all_tools = Vec::new();

        for frame_tools in st.tools.values() {
            for tool in frame_tools.values() {
                all_tools.push(tool.clone());
            }
        }
        all_tools.sort_by(|a, b| (&a.frame_id, &a.name).cmp(&(&b.frame_id, &b.name)));

        let content = match all_tools.is_empty() {
            true => "[]".to_string(),
            false => listing(&all_tools).to_string(),
        };

        let mut content_list = vec![content.into()];

        if all_tools.is_empty() {
            let warn_text = match st.availability {
                WebmcpAvailability::NotRequested => {
                    "\n\n[Warning] No tools registered. WebMCP testing features are not active. Make sure the server was started with --enable-webmcp."
                }
                WebmcpAvailability::Pending => {
                    "\n\n[Note] No tools registered yet: WebMCP is being enabled on this tab and the page may still be loading. Retry webmcp_list_tools in a moment."
                }
                WebmcpAvailability::Unsupported => {
                    "\n\n[Warning] No tools registered. WebMCP was enabled via --enable-webmcp, but this Chrome instance does not support or expose the WebMCP CDP domain."
                }
                WebmcpAvailability::Enabled => {
                    "\n\n[Note] WebMCP is active, but this page has not registered any tools yet. Pages often register tools shortly after load or after client-side navigation: retry in a moment. If it stays empty, the page may not expose WebMCP tools."
                }
            };
            content_list.push(warn_text.to_string().into());
        }

        Ok(CallToolResult::text_content(content_list))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::cdp_domains::webmcp::{WebmcpState, WebmcpTool};
    use serde_json::json;

    fn make_tool(name: &str, frame_id: &str) -> WebmcpTool {
        WebmcpTool {
            name: name.into(),
            description: "desc".into(),
            input_schema: Some(json!({})),
            annotations: None,
            frame_id: frame_id.into(),
            backend_node_id: None,
        }
    }

    fn extract_text(result: &CallToolResult) -> String {
        let content_val = serde_json::to_value(&result.content).unwrap();
        content_val[0]["text"].as_str().unwrap().to_string()
    }

    /// ISSUE-73: the listing is compact: one frameId when the tools share
    /// it, no empty `required`, no false annotations.
    #[test]
    fn listing_is_compact() {
        // Given two tools of one frame, with empty and false fields
        let mut tool = make_tool("a", "F1");
        tool.input_schema = Some(json!({"type": "object", "properties": {}, "required": []}));
        tool.annotations = serde_json::from_value(
            json!({"readOnly": true, "untrustedContent": false, "consequential": false}),
        )
        .ok();
        let other = make_tool("b", "F1");
        // When they are listed
        let listed = listing(&[tool.clone(), other]);
        // Then the frame is said once and the empty fields are gone
        assert_eq!(listed["frameId"], "F1");
        let first = &listed["tools"][0];
        assert!(first.get("frameId").is_none());
        assert_eq!(
            first["inputSchema"],
            json!({"type": "object", "properties": {}})
        );
        assert_eq!(first["annotations"], json!({"readOnly": true}));
        // And tools of several frames keep their own
        let listed = listing(&[tool, make_tool("c", "F2")]);
        assert!(listed.get("frameId").is_none());
        assert_eq!(listed["tools"][1]["frameId"], "F2");
    }

    #[tokio::test]
    async fn test_webmcp_list_tools_handle() {
        let handler = McpHandler::new_test();
        {
            let mut st = handler.default_session.webmcp_state.lock().await;
            st.tools
                .entry("frame-1".into())
                .or_default()
                .insert("mockTool".into(), make_tool("mockTool", "frame-1"));
        }

        let params: CallToolRequestParams = serde_json::from_value(json!({
            "name": "webmcp_list_tools",
            "arguments": {}
        }))
        .unwrap();

        let result = ListWebmcpToolsTool::handle(params, &handler).await.unwrap();
        let text = extract_text(&result);
        assert!(text.contains("mockTool"));
    }

    #[tokio::test]
    async fn test_list_tools_reads_tab_state_not_session_state() {
        let handler = McpHandler::new_test();
        let session = handler.session(None).await.unwrap();

        // Put a tool in the SESSION state (the fallback).
        {
            let mut st = session.webmcp_state.lock().await;
            st.tools
                .entry("frame-s".into())
                .or_default()
                .insert("sessionTool".into(), make_tool("sessionTool", "frame-s"));
        }

        // Register a tab and put a different tool in the TAB state.
        let tab_id = crate::handler::cdp_domains::tests::register_mock_tab(&session).await;
        let state = session.webmcp_state(Some(tab_id.clone())).unwrap();
        state
            .lock()
            .await
            .tools
            .entry("frame-t".into())
            .or_default()
            .insert("tabTool".into(), make_tool("tabTool", "frame-t"));

        // Explicit tab_id: must return the TAB tool, not the session tool.
        let params: CallToolRequestParams = serde_json::from_value(json!({
            "name": "webmcp_list_tools",
            "arguments": { "tab_id": tab_id }
        }))
        .unwrap();
        let result = ListWebmcpToolsTool::handle(params, &handler).await.unwrap();
        let text = extract_text(&result);
        assert!(text.contains("tabTool"), "expected tab tool, got: {text}");
        assert!(
            !text.contains("sessionTool"),
            "must not leak session state into tab result"
        );

        // Omitted tab_id (active tab = the registered tab): same result.
        let params: CallToolRequestParams = serde_json::from_value(json!({
            "name": "webmcp_list_tools",
            "arguments": {}
        }))
        .unwrap();
        let result = ListWebmcpToolsTool::handle(params, &handler).await.unwrap();
        let text = extract_text(&result);
        assert!(text.contains("tabTool"), "active-tab lookup failed: {text}");
    }

    #[tokio::test]
    async fn test_list_tools_unknown_tab_id_errors() {
        let handler = McpHandler::new_test();
        let params: CallToolRequestParams = serde_json::from_value(json!({
            "name": "webmcp_list_tools",
            "arguments": { "tab_id": "tab-nope" }
        }))
        .unwrap();
        let err = ListWebmcpToolsTool::handle(params, &handler)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("not found"),
            "expected tab-not-found error, got: {err}"
        );
    }

    #[test]
    fn test_list_tools_schema_no_unknown_types() {
        let schema = ListWebmcpToolsTool::json_schema();
        let serialized = serde_json::to_string(&schema).unwrap();
        assert!(
            !serialized.contains("\"unknown\""),
            "schema must not contain type=unknown: {serialized}"
        );
    }

    #[tokio::test]
    async fn given_fresh_state_with_enable_webmcp_when_list_tools_then_returns_pending_warning() {
        // Given: state newly created with enable_webmcp = true (Pending availability)
        let st = WebmcpState {
            availability: WebmcpAvailability::initial(true),
            ..Default::default()
        };
        let handler = McpHandler::new_test();
        *handler.default_session.webmcp_state.lock().await = st;

        // When: calling webmcp_list_tools
        let params: CallToolRequestParams = serde_json::from_value(json!({
            "name": "webmcp_list_tools",
            "arguments": {}
        }))
        .unwrap();
        let result = ListWebmcpToolsTool::handle(params, &handler).await.unwrap();

        // Then: message contains "retry" and DOES NOT contain "--enable-webmcp"
        let content_val = serde_json::to_value(&result.content).unwrap();
        let warn_text = content_val[1]["text"].as_str().unwrap();
        assert!(
            warn_text.to_lowercase().contains("retry"),
            "expected retry in pending message, got: {warn_text}"
        );
        assert!(
            !warn_text.contains("--enable-webmcp"),
            "pending warning must not suggest server misconfiguration (--enable-webmcp): {warn_text}"
        );
    }

    #[tokio::test]
    async fn given_flag_inactive_when_list_tools_then_returns_not_requested_warning() {
        // Given: state created with enable_webmcp = false (NotRequested availability)
        let st = WebmcpState {
            availability: WebmcpAvailability::initial(false),
            ..Default::default()
        };
        let handler = McpHandler::new_test();
        *handler.default_session.webmcp_state.lock().await = st;

        // When: calling webmcp_list_tools
        let params: CallToolRequestParams = serde_json::from_value(json!({
            "name": "webmcp_list_tools",
            "arguments": {}
        }))
        .unwrap();
        let result = ListWebmcpToolsTool::handle(params, &handler).await.unwrap();

        // Then: message mentions --enable-webmcp
        let content_val = serde_json::to_value(&result.content).unwrap();
        let warn_text = content_val[1]["text"].as_str().unwrap();
        assert!(
            warn_text.contains("--enable-webmcp"),
            "expected --enable-webmcp in NotRequested message, got: {warn_text}"
        );
    }

    #[tokio::test]
    async fn given_enabled_state_with_no_tools_when_list_tools_then_returns_generic_active_note() {
        // Given: WebMCP enabled availability with empty tools
        let st = WebmcpState {
            availability: WebmcpAvailability::Enabled,
            ..Default::default()
        };
        let handler = McpHandler::new_test();
        *handler.default_session.webmcp_state.lock().await = st;

        // When: calling webmcp_list_tools
        let params: CallToolRequestParams = serde_json::from_value(json!({
            "name": "webmcp_list_tools",
            "arguments": {}
        }))
        .unwrap();
        let result = ListWebmcpToolsTool::handle(params, &handler).await.unwrap();

        // Then: message is generic active note and does not contain knot.kz URL
        let content_val = serde_json::to_value(&result.content).unwrap();
        let warn_text = content_val[1]["text"].as_str().unwrap();
        assert!(
            warn_text.contains("WebMCP is active, but this page has not registered any tools yet"),
            "expected generic active note, got: {warn_text}"
        );
        assert!(
            !warn_text.contains("knot.kz"),
            "warning message must not reference hardcoded URL knot.kz: {warn_text}"
        );
    }
}
