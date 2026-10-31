use crate::handler::McpHandler;
use rust_mcp_sdk::{
    macros,
    schema::{CallToolError, CallToolRequestParams, CallToolResult},
};

#[macros::mcp_tool(
    name = "get_console_logs",
    description = "Returns the tab's console messages and uncaught exceptions, oldest first, one line each: [level] text @ url:line (lines from 1; objects previewed; repeats folded; an error's stack indented under it). include_details: true gives {messages, total, truncated, notes?} as JSON. Keeps the last 1000 (10000 characters each), cleared when the tab moves to another site."
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

    /// Every field (source, timestamp...) as JSON. Default: false (one line per message).
    #[serde(default)]
    pub include_details: Option<bool>,
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
        if !self.include_details.unwrap_or(false) {
            let text = summary(&messages, total, &notes);
            return CallToolResult::text_content(vec![text.into()]);
        }
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

/// Most lines of a message's stack (or other continuation lines) shown.
const STACK_LINES: usize = 5;

/// Most characters of one line of a message.
const LINE_MAX_CHARS: usize = 1000;

/// The default reply: a header, then one line per message (repeats in a
/// row folded), oldest first, then the notes.
fn summary(messages: &[super::ConsoleMessage], total: usize, notes: &[String]) -> String {
    let shown = messages.len();
    let header = if total > shown {
        format!("{shown} of {total} console message(s), oldest first (raise 'limit' for more):")
    } else {
        format!("{shown} console message(s), oldest first:")
    };
    let mut lines = vec![header];
    let mut rest = messages;
    while let Some(first) = rest.first() {
        let repeats = rest.iter().take_while(|m| same_message(m, first)).count();
        lines.push(message_lines(first, repeats));
        rest = &rest[repeats..];
    }
    lines.extend(notes.iter().map(|note| format!("Note: {note}")));
    lines.join("\n")
}

/// Whether `a` and `b` are the same message (a repeat).
fn same_message(a: &super::ConsoleMessage, b: &super::ConsoleMessage) -> bool {
    (&a.source, &a.level, &a.text, &a.url, a.line_number)
        == (&b.source, &b.level, &b.text, &b.url, b.line_number)
}

/// `[level] text @ url:line [xN]`, lines counted from 1, then the rest of
/// the text (a stack) indented, bounded.
fn message_lines(message: &super::ConsoleMessage, repeats: usize) -> String {
    let level = super::canonical_level(&message.level);
    let mut text = message.text.lines();
    let first = text.next().unwrap_or_default();
    // Uncaught: Chrome's exceptions, and Firefox's script errors (its
    // watcher labels them `javascript`; console.error is `console-api`).
    let thrown =
        message.source == "exception" || (message.source == "javascript" && level == "error");
    let uncaught = if thrown && !first.starts_with("Uncaught") {
        "Uncaught "
    } else {
        ""
    };
    let at = match (&message.url, message.line_number) {
        (Some(url), Some(line)) => format!(" @ {url}:{}", line + 1),
        (Some(url), None) => format!(" @ {url}"),
        _ => String::new(),
    };
    let times = if repeats > 1 {
        format!(" [x{repeats}]")
    } else {
        String::new()
    };
    let first = cut(first, LINE_MAX_CHARS);
    let mut out = format!("[{level}] {uncaught}{first}{at}{times}");
    let more: Vec<&str> = text.collect();
    for line in more.iter().take(STACK_LINES) {
        out.push_str("\n    ");
        out.push_str(&cut(line.trim_start(), LINE_MAX_CHARS));
    }
    if more.len() > STACK_LINES {
        let hidden = more.len() - STACK_LINES;
        out.push_str(&format!(
            "\n    … {hidden} more line(s) (include_details: true shows them)"
        ));
    }
    out
}

/// `text` cut at `max` characters with an ellipsis.
fn cut(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((at, _)) => format!("{}…", &text[..at]),
        None => text.to_string(),
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

    async fn read(state: &Arc<Mutex<LogState>>, mut args: Value) -> Value {
        args["include_details"] = json!(true);
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

    fn message(
        source: &str,
        level: &str,
        text: &str,
        at: Option<(&str, Option<i64>)>,
    ) -> ConsoleMessage {
        ConsoleMessage {
            source: source.into(),
            level: level.into(),
            text: text.into(),
            timestamp: 0.0,
            url: at.map(|(url, _)| url.to_string()),
            line_number: at.and_then(|(_, line)| line),
            channel: Default::default(),
        }
    }

    /// ISSUE-19: by default one line per message, `[level] text @
    /// url:line` (lines from 1), repeats folded, uncaught errors marked with
    /// their stack indented, and a header that says when older ones were
    /// left out.
    #[tokio::test]
    async fn default_reply_is_one_line_per_message() {
        // Given a log, a warning, a 404 twice and an uncaught error
        let mut state = LogState::default();
        let app = Some(("http://a/app.js", Some(2)));
        state.push_message(message("console-api", "log", "hello {n: 1}", app));
        state.push_message(message("console-api", "warning", "careful", None));
        for _ in 0..2 {
            let missing = Some(("http://a/x", None));
            state.push_message(message(
                "network",
                "error",
                "Failed to load resource: 404",
                missing,
            ));
        }
        let stack = "Error: boom\n    at f (http://a/app.js:3:5)\n    at g (http://a/app.js:9:1)";
        state.push_message(message("exception", "error", stack, app));
        let state = Arc::new(Mutex::new(state));
        let text = |args: Value| {
            let state = state.clone();
            async move {
                let tool: GetConsoleLogsTool = serde_json::from_value(args).unwrap();
                let reply = tool.report(&state, None).await;
                reply.content[0].as_text_content().unwrap().text.clone()
            }
        };
        // When read without options
        let all = text(json!({})).await;
        // Then each message is one line (the stack under its error)
        assert_eq!(
            all,
            "5 console message(s), oldest first:\n\
             [log] hello {n: 1} @ http://a/app.js:3\n\
             [warning] careful\n\
             [error] Failed to load resource: 404 @ http://a/x [x2]\n\
             [error] Uncaught Error: boom @ http://a/app.js:3\n\
             \x20   at f (http://a/app.js:3:5)\n\
             \x20   at g (http://a/app.js:9:1)"
        );
        // And a limit says how many were left out
        let last = text(json!({"limit": 1})).await;
        assert!(
            last.starts_with("1 of 5 console message(s), oldest first (raise 'limit' for more):\n[error] Uncaught"),
            "{last}"
        );
    }

    /// ISSUE-19: Firefox's script errors (labelled `javascript` by its
    /// watcher) are marked uncaught like Chrome's exceptions; its other
    /// messages are not.
    #[test]
    fn firefox_script_errors_are_uncaught() {
        let error = message(
            "javascript",
            "error",
            "ReferenceError: x is not defined",
            None,
        );
        assert_eq!(
            message_lines(&error, 1),
            "[error] Uncaught ReferenceError: x is not defined"
        );
        let warning = message("javascript", "warning", "unreachable code", None);
        assert_eq!(message_lines(&warning, 1), "[warning] unreachable code");
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
