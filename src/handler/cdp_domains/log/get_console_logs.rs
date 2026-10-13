use crate::handler::McpHandler;
use rust_mcp_sdk::{
    macros,
    schema::{CallToolError, CallToolRequestParams, CallToolResult},
};

#[macros::mcp_tool(
    name = "get_console_logs",
    description = "Retrieves the tab's console messages (log, warning, error, info levels and uncaught exceptions), oldest first. The buffer is per tab, holds the last 1000 messages, and is cleared when the tab moves to another site (kept across subdomains of one site, reloads and same-page navigations). Side effects: with 'clear' true the returned messages are removed from the buffer. Returns: JSON {messages: [{source, level, text, timestamp, url, line_number}], total (matching messages), truncated (more matched than 'limit'), notes?}. Use this to debug script errors, monitor page health, inspect exception traces. Alternatives: browser DevTools Console, error logging services."
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct GetConsoleLogsTool {
    /// Chrome instance id from open_instance/list_instances. Omit for the default instance.
    pub instance_id: Option<String>,
    /// The Tab ID of the target tab. Omit to use the active tab.
    pub tab_id: Option<String>,
    /// Remove the returned messages from the buffer. Constraints: boolean. Interactions: only the returned messages (after level_filter and limit) are removed; later calls return the rest plus new messages. Defaults to: false.
    #[serde(default)]
    pub clear: Option<bool>,

    /// Filter logs by severity level (case-insensitive). Constraints: 'error', 'warning', 'info', 'log', or similar CDP log level. Interactions: when provided, returns only matching level; empty returns all. Defaults to: None (no filtering).
    #[serde(default)]
    pub level_filter: Option<String>,

    /// Most messages to return: the most recent ones that match. Constraints: integer between 1 and 1000. Defaults to: 200.
    #[serde(default)]
    pub limit: Option<u32>,
}

/// Messages returned when the agent gives no `limit`.
const DEFAULT_LIMIT: usize = 200;

impl GetConsoleLogsTool {
    /// The tool's output for a tab's console buffer (shared by every
    /// browser); `note` (e.g. lost events) joins the reply's notes.
    pub(crate) async fn report(
        &self,
        log_state: &std::sync::Arc<tokio::sync::Mutex<super::LogState>>,
        note: Option<String>,
    ) -> CallToolResult {
        let limit = self.limit.map_or(DEFAULT_LIMIT, |l| {
            usize::try_from(l)
                .unwrap_or(usize::MAX)
                .clamp(1, crate::handler::MAX_CONSOLE_MESSAGES)
        });
        let level = self.level_filter.as_deref().map(str::to_lowercase);
        let mut st = log_state.lock().await;
        let matching: Vec<usize> = (0..st.messages.len())
            .filter(|&i| {
                level
                    .as_deref()
                    .is_none_or(|l| st.messages[i].level.to_lowercase() == l)
            })
            .collect();
        let total = matching.len();
        let returned = &matching[total.saturating_sub(limit)..];
        let messages: Vec<super::ConsoleMessage> =
            returned.iter().map(|&i| st.messages[i].clone()).collect();
        if self.clear.unwrap_or(false) {
            let mut index = 0;
            st.messages.retain(|_| {
                let keep = !returned.contains(&index);
                index += 1;
                keep
            });
        }
        let notes: Vec<String> = st.take_cleared_note().into_iter().chain(note).collect();
        drop(st);
        let mut reply = serde_json::json!({
            "messages": messages,
            "total": total,
            "truncated": total > messages.len(),
        });
        if !notes.is_empty() {
            reply["notes"] = serde_json::json!(notes);
        }
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
        let args_value = serde_json::Value::Object(params.arguments.unwrap_or_default());
        let args: GetConsoleLogsTool = serde_json::from_value(args_value)
            .map_err(|e| CallToolError::from_message(e.to_string()))?;
        let session = handler.session(args.instance_id.clone()).await?;

        let log_state = session.log_state(args.tab_id.clone())?;
        Ok(args.report(&log_state, None).await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::cdp_domains::log::{ConsoleMessage, LogState};
    use serde_json::{Value, json};
    use std::sync::Arc;
    use tokio::sync::Mutex;

    /// A buffer of `n` messages `m0..`, every third one an error.
    fn buffer(n: usize) -> Arc<Mutex<LogState>> {
        let mut state = LogState::default();
        for i in 0..n {
            state.push_message(ConsoleMessage {
                source: "console-api".into(),
                level: if i % 3 == 0 { "error" } else { "log" }.into(),
                text: format!("m{i}"),
                timestamp: 0.0,
                url: None,
                line_number: None,
                channel: Default::default(),
            });
        }
        Arc::new(Mutex::new(state))
    }

    async fn read(state: &Arc<Mutex<LogState>>, args: Value) -> Value {
        let tool: GetConsoleLogsTool = serde_json::from_value(args).unwrap();
        let reply = tool.report(state, None).await;
        serde_json::from_str(&reply.content[0].as_text_content().unwrap().text).unwrap()
    }

    fn texts(reply: &Value) -> Vec<&str> {
        reply["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["text"].as_str().unwrap())
            .collect()
    }

    /// CON-05: `limit` returns the most recent matches, oldest first, and
    /// says when it cut.
    #[tokio::test]
    async fn limit_returns_the_latest_and_reports_truncation() {
        // Given 300 messages
        let state = buffer(300);
        // When read without and with a limit
        let default = read(&state, json!({})).await;
        let three = read(&state, json!({"limit": 3})).await;
        // Then 200 by default, the last ones, with the total
        assert_eq!(default["messages"].as_array().unwrap().len(), 200);
        assert_eq!(texts(&default)[0], "m100");
        assert_eq!(
            (default["total"].as_u64(), default["truncated"].as_bool()),
            (Some(300), Some(true))
        );
        assert_eq!(texts(&three), ["m297", "m298", "m299"]);
        // And nothing is cut when everything fits
        let all = read(&state, json!({"limit": 1000})).await;
        assert_eq!(all["truncated"], false);
    }

    /// CON-06 (F-24): `clear` removes only the returned messages.
    #[tokio::test]
    async fn clear_removes_only_the_returned_messages() {
        // Given 7 messages, errors at m0, m3, m6
        let state = buffer(7);
        // When the last two errors are read with clear
        let read_errors = read(
            &state,
            json!({"level_filter": "error", "limit": 2, "clear": true}),
        )
        .await;
        assert_eq!(texts(&read_errors), ["m3", "m6"]);
        // Then the rest stays, in order
        let rest = read(&state, json!({})).await;
        assert_eq!(texts(&rest), ["m0", "m1", "m2", "m4", "m5"]);
    }
}
