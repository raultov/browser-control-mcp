use crate::handler::McpHandler;
use rust_mcp_sdk::{
    macros,
    schema::{CallToolError, CallToolRequestParams, CallToolResult},
};

#[macros::mcp_tool(
    name = "get_console_logs",
    description = "Returns the tab's console messages and uncaught exceptions, oldest first: {messages, total, truncated, notes?}. Keeps the last 1000 (10000 characters each), cleared when the tab moves to another site."
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct GetConsoleLogsTool {
    /// Instance id; omit for the default instance.
    pub instance_id: Option<String>,
    /// Tab id; omit for the active tab.
    pub tab_id: Option<String>,
    /// Remove the returned messages. Default: false.
    #[serde(default)]
    pub clear: Option<bool>,

    /// 'error', 'warning', 'info', 'log', 'debug'... Default: every level.
    #[serde(default)]
    pub level_filter: Option<String>,

    /// Most recent messages to return, 1 to 1000. Default: 200.
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
        let level = self.level_filter.as_deref().map(super::canonical_level);
        let mut st = log_state.lock().await;
        let matching: Vec<usize> = (0..st.messages.len())
            .filter(|&i| {
                level
                    .as_deref()
                    .is_none_or(|l| super::canonical_level(&st.messages[i].level) == l)
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
        // Connected first: on an instance nothing has connected to yet (an
        // attached Chrome) the buffers are empty until the page's listeners
        // run, and the page's earlier messages are replayed on connection.
        let first = !session.is_connected().await;
        session.target(args.tab_id.clone()).await?;
        if first {
            session.settle_replayed_events().await;
        }
        let log_state = session.log_state(args.tab_id.clone())?;
        Ok(args.report(&log_state, None).await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AUD-OB-30: get_console_logs and get_network_logs connect first, so
    /// on an instance nothing has connected to yet (an attached Chrome)
    /// they read the page's buffers, not an empty session state.
    #[tokio::test]
    async fn observation_tools_connect_first() {
        use crate::handler::cdp_domains::network::get_network_logs::GetNetworkLogsTool;
        use crate::handler::cdp_domains::tests::{call, mock_handler, spawn_recording_mock};
        for name in ["get_console_logs", "get_network_logs"] {
            let mock = spawn_recording_mock().await;
            let handler = mock_handler(mock.http_port);
            let params = call(name, serde_json::json!({}));
            let reply = match name {
                "get_console_logs" => GetConsoleLogsTool::handle(params, &handler).await,
                _ => GetNetworkLogsTool::handle(params, &handler).await,
            };
            assert!(reply.is_ok(), "{name}: {:?}", reply.err());
            assert!(
                !mock.commands_named("Runtime.enable").is_empty(),
                "{name} read the buffers without connecting"
            );
            let text = crate::handler::cdp_domains::tests::reply_text(&reply.unwrap());
            let noted = text.contains("requests sent before it are not captured");
            assert_eq!(noted, name == "get_network_logs", "{name}: {text}");
        }
    }
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

    /// AUD-OB-02: `warn` and `warning` name the same level, so one filter
    /// works on both browsers (Firefox's console API calls it `warn`).
    #[tokio::test]
    async fn warn_and_warning_filter_the_same_messages() {
        let mut state = LogState::default();
        for (level, text) in [("warning", "w"), ("log", "l")] {
            state.push_message(ConsoleMessage {
                source: "console-api".into(),
                level: level.into(),
                text: text.into(),
                timestamp: 0.0,
                url: None,
                line_number: None,
                channel: Default::default(),
            });
        }
        let state = Arc::new(Mutex::new(state));
        for filter in ["warning", "warn", "WARN"] {
            let reply = read(&state, json!({"level_filter": filter})).await;
            assert_eq!(texts(&reply), ["w"], "{filter}");
        }
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
