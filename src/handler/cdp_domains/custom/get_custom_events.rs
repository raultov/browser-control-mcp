use crate::handler::McpHandler;
use rust_mcp_sdk::{
    macros,
    schema::{CallToolError, CallToolRequestParams, CallToolResult},
};

#[macros::mcp_tool(
    name = "get_custom_events",
    description = "Returns events of the protocol domains or modules the agent enabled (with 'send_cdp_command' or 'send_bidi_command') that no other tool captures: {events, total, truncated}, newest last."
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct GetCustomEventsTool {
    /// Instance id; omit for the default instance.
    pub instance_id: Option<String>,
    /// Tab id; omit for the active tab.
    pub tab_id: Option<String>,
    /// Event method, e.g. 'Target.targetCreated'.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filter_method: Option<String>,
    /// Newest events to return (0 = all). Default: 100.
    #[serde(default = "default_limit")]
    pub limit: u32,
    /// Remove the returned events. Default: false.
    #[serde(default)]
    pub clear: Option<bool>,
}

fn default_limit() -> u32 {
    100
}

impl GetCustomEventsTool {
    /// The tool's output for a tab's custom-event buffer (every browser).
    pub(crate) async fn report(
        &self,
        custom_state: &std::sync::Arc<tokio::sync::Mutex<crate::handler::CustomState>>,
    ) -> CallToolResult {
        let mut st = custom_state.lock().await;
        let matching: Vec<usize> = (0..st.events.len())
            .filter(|&i| {
                let method = &st.events[i].method;
                self.filter_method.as_ref().is_none_or(|f| method == f)
            })
            .collect();
        let total = matching.len();
        let returned = &matching[total.saturating_sub(effective_limit(self.limit))..];
        let events: Vec<_> = returned.iter().map(|&i| st.events[i].clone()).collect();
        if self.clear.unwrap_or(false) {
            let mut index = 0;
            st.events.retain(|_| {
                let keep = !returned.contains(&index);
                index += 1;
                keep
            });
        }
        drop(st);
        let reply = serde_json::json!({
            "events": events,
            "total": total,
            "truncated": total > events.len(),
        });
        CallToolResult::text_content(vec![
            serde_json::to_string_pretty(&reply)
                .unwrap_or_default()
                .into(),
        ])
    }

    pub async fn handle(
        params: CallToolRequestParams,
        handler: &McpHandler,
    ) -> Result<CallToolResult, CallToolError> {
        let tool: GetCustomEventsTool = serde_json::from_value(serde_json::Value::Object(
            params.arguments.unwrap_or_default(),
        ))
        .map_err(|e| CallToolError::from_message(format!("Failed to parse arguments: {}", e)))?;

        let session = handler.session(tool.instance_id.clone()).await?;
        let custom_state = session.custom_state(tool.tab_id.clone())?;
        Ok(tool.report(&custom_state).await)
    }
}

/// `0` means unlimited, as the schema says.
fn effective_limit(limit: u32) -> usize {
    match limit {
        0 => usize::MAX,
        n => n as usize,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::CustomEvent;
    use serde_json::json;

    #[tokio::test]
    async fn test_get_custom_events_tool_schema() {
        let tool_def = GetCustomEventsTool::tool();
        println!("{}", serde_json::to_string_pretty(&tool_def).unwrap());
    }

    #[tokio::test]
    async fn test_get_custom_events_tool_deserialization() {
        let tool: Result<GetCustomEventsTool, _> = serde_json::from_value(json!({
            "filter_method": "Target.targetCreated",
            "limit": 10
        }));
        assert!(tool.is_ok());
        let tool = tool.unwrap();
        assert_eq!(tool.filter_method, Some("Target.targetCreated".to_string()));
        assert_eq!(tool.limit, 10);
    }

    #[tokio::test]
    async fn test_get_custom_events_tool_deserialization_default() {
        let tool: Result<GetCustomEventsTool, _> = serde_json::from_value(json!({}));
        assert!(tool.is_ok());
        let tool = tool.unwrap();
        assert_eq!(tool.limit, 100);
    }

    #[tokio::test]
    async fn test_get_custom_events_handle() {
        let handler = McpHandler::new_test();
        {
            let mut st = handler.default_session.custom_state.lock().await;
            st.events.push_back(CustomEvent {
                method: "Target.targetCreated".to_string(),
                params: json!({"targetId": "1"}),
                timestamp: "2023-01-01T00:00:00Z".to_string(),
            });
            st.events.push_back(CustomEvent {
                method: "Target.targetInfoChanged".to_string(),
                params: json!({"targetId": "1"}),
                timestamp: "2023-01-01T00:00:01Z".to_string(),
            });
        }

        let params: CallToolRequestParams = serde_json::from_value(json!({
            "name": "get_custom_events",
            "arguments": {
                "instance_id": "default",
                "filter_method": "Target.targetCreated"
            }
        }))
        .unwrap();

        let result = GetCustomEventsTool::handle(params, &handler).await;
        assert!(result.is_ok());
        let call_result = result.unwrap();
        let content_str = format!("{:?}", call_result.content);
        assert!(content_str.contains("\\\"total\\\": 1"), "{content_str}");
        assert!(content_str.contains("Target.targetCreated"));
    }

    /// The `report` of `args` over a buffer of events `k0..k{n}`.
    async fn report_over(
        n: usize,
        args: serde_json::Value,
    ) -> (
        serde_json::Value,
        std::sync::Arc<tokio::sync::Mutex<crate::handler::CustomState>>,
    ) {
        let state = std::sync::Arc::new(tokio::sync::Mutex::new(
            crate::handler::CustomState::default(),
        ));
        for i in 0..n {
            state.lock().await.events.push_back(CustomEvent {
                method: "Page.k".into(),
                params: json!({"key": format!("k{i}")}),
                timestamp: String::new(),
            });
        }
        let tool: GetCustomEventsTool = serde_json::from_value(args).unwrap();
        let reply = tool.report(&state).await;
        let text = reply.content[0].as_text_content().unwrap().text.clone();
        (serde_json::from_str(&text).unwrap(), state)
    }

    /// AUD-OB-12: `limit` keeps the newest events (it kept the oldest, so
    /// new ones never showed), the reply is JSON, and `clear` removes what
    /// was returned.
    #[tokio::test]
    async fn newest_events_are_returned_and_cleared() {
        let (reply, state) = report_over(5, json!({"limit": 3, "clear": true})).await;
        let keys: Vec<_> = reply["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["params"]["key"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(keys, ["k2", "k3", "k4"]);
        assert_eq!(
            (reply["total"].clone(), reply["truncated"].clone()),
            (json!(5), json!(true))
        );
        let left: Vec<_> = state
            .lock()
            .await
            .events
            .iter()
            .map(|e| e.params["key"].clone())
            .collect();
        assert_eq!(left, [json!("k0"), json!("k1")]);
        let (reply, _) = report_over(2, json!({})).await;
        assert_eq!(reply["truncated"], false);
    }

    /// FIX-04: get_custom_events limit 0 means unlimited.
    #[tokio::test]
    async fn custom_events_limit_zero_is_unlimited() {
        // Given 150 buffered custom events
        let handler = McpHandler::new_test();
        let session = handler.session(None).await.unwrap();
        for i in 0..150 {
            session
                .custom_state
                .lock()
                .await
                .events
                .push_back(CustomEvent {
                    method: format!("DOM.e{i}"),
                    params: serde_json::json!({}),
                    timestamp: String::new(),
                });
        }
        // When get_custom_events is called with limit 0
        let params: CallToolRequestParams = serde_json::from_value(
            serde_json::json!({"name": "get_custom_events", "arguments": {"limit": 0}}),
        )
        .unwrap();
        let result = GetCustomEventsTool::handle(params, &handler).await.unwrap();
        // Then all 150 are returned
        let text = result.content[0].as_text_content().unwrap().text.clone();
        let reply: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(reply["events"].as_array().unwrap().len(), 150, "{text}");
    }
}
