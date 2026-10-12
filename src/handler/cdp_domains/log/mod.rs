pub mod get_console_logs;

use crate::handler::MAX_CONSOLE_MESSAGES;
use crate::util::json::{opt_string, str_field, string_or_empty};
use cdp_browser_lite::WsResponse;
use serde_json::Value;
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Clone, Debug, ::serde::Serialize, ::serde::Deserialize)]
pub struct ConsoleMessage {
    pub source: String,
    pub level: String,
    pub text: String,
    pub timestamp: f64,
    pub url: Option<String>,
    pub line_number: Option<i64>,
}

#[derive(Default)]
pub(crate) struct LogState {
    pub messages: VecDeque<ConsoleMessage>,
}

impl LogState {
    pub(crate) fn push_message(&mut self, msg: ConsoleMessage) {
        self.messages.push_back(msg);
        if self.messages.len() > MAX_CONSOLE_MESSAGES {
            self.messages.pop_front();
        }
    }
}

pub(crate) async fn process_log_event(event: &WsResponse, state: &Arc<Mutex<LogState>>) {
    let (Some(method), Some(params)) = (event.method.as_deref(), &event.params) else {
        return;
    };
    if let Some(message) = console_message(method, params) {
        state.lock().await.push_message(message);
    }
}

/// The console message an event describes, if it is one of the three
/// console events.
fn console_message(method: &str, params: &Value) -> Option<ConsoleMessage> {
    match method {
        "Log.entryAdded" => params.get("entry").map(log_entry),
        "Runtime.consoleAPICalled" => Some(console_api_call(params)),
        "Runtime.exceptionThrown" => params
            .get("exceptionDetails")
            .map(|details| exception(params, details)),
        _ => None,
    }
}

fn timestamp(value: &Value) -> f64 {
    value
        .get("timestamp")
        .and_then(Value::as_f64)
        .unwrap_or(0.0)
}

fn log_entry(entry: &Value) -> ConsoleMessage {
    ConsoleMessage {
        source: string_or_empty(entry, "source"),
        level: string_or_empty(entry, "level"),
        text: string_or_empty(entry, "text"),
        timestamp: timestamp(entry),
        url: opt_string(entry, "url"),
        line_number: entry.get("lineNumber").and_then(Value::as_i64),
    }
}

fn console_api_call(params: &Value) -> ConsoleMessage {
    let text = params
        .get("args")
        .and_then(Value::as_array)
        .map(|args| {
            args.iter()
                .filter_map(arg_text)
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();
    ConsoleMessage {
        source: "console-api".to_string(),
        level: string_or_empty(params, "type"),
        text,
        timestamp: timestamp(params),
        url: None, // Could extract from stackTrace if needed
        line_number: None,
    }
}

/// One `console.*` argument as text: strings as is, numbers and booleans
/// printed, objects by their description.
fn arg_text(arg: &Value) -> Option<String> {
    let description = str_field(arg, "description").map(str::to_string);
    match arg.get("value") {
        Some(Value::String(s)) => Some(s.clone()),
        Some(val) if val.is_number() || val.is_boolean() => Some(val.to_string()),
        Some(val) => Some(description.unwrap_or_else(|| val.to_string())),
        None => description,
    }
}

fn exception(params: &Value, details: &Value) -> ConsoleMessage {
    let text = details
        .get("exception")
        .and_then(|e| str_field(e, "description"))
        .map(str::to_string)
        .unwrap_or_else(|| string_or_empty(details, "text"));
    ConsoleMessage {
        source: "exception".to_string(),
        level: "error".to_string(),
        text,
        timestamp: timestamp(params),
        url: opt_string(details, "url"),
        line_number: details.get("lineNumber").and_then(Value::as_i64),
    }
}

pub(crate) fn start_log_listener(
    target: &crate::handler::cdp_domains::cdp_target::CdpTarget,
    state: Arc<Mutex<LogState>>,
) -> crate::handler::cdp_domains::event_pump::ListenerHandles {
    use crate::handler::cdp_domains::event_pump::{ListenerHandles, spawn_state_listener};
    let mut handles = ListenerHandles::default();
    for domain in ["Log", "Runtime"] {
        handles.push(spawn_state_listener(
            target,
            domain,
            state.clone(),
            |event, state| async move { process_log_event(&event, &state).await },
        ));
    }
    handles
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn log_entry_becomes_a_console_message() {
        let params = json!({"entry": {"source": "network", "level": "error", "text": "404",
            "timestamp": 1.5, "url": "https://a/x", "lineNumber": 3}});
        let m = console_message("Log.entryAdded", &params).unwrap();
        assert_eq!(
            (m.source.as_str(), m.level.as_str(), m.text.as_str()),
            ("network", "error", "404")
        );
        assert_eq!(
            (m.timestamp, m.url.as_deref(), m.line_number),
            (1.5, Some("https://a/x"), Some(3))
        );
    }

    #[test]
    fn console_api_arguments_are_joined() {
        let params = json!({"type": "log", "timestamp": 2.0, "args": [
            {"type": "string", "value": "hi"}, {"type": "number", "value": 3},
            {"type": "object", "value": {"a": 1}, "description": "Object"},
            {"type": "object", "description": "Window"}, {"type": "undefined"}]});
        let m = console_message("Runtime.consoleAPICalled", &params).unwrap();
        assert_eq!(m.text, "hi 3 Object Window");
        assert_eq!(
            (m.source.as_str(), m.level.as_str()),
            ("console-api", "log")
        );
    }

    #[test]
    fn exception_prefers_the_exception_description() {
        let params = json!({"timestamp": 4.0, "exceptionDetails": {"text": "Uncaught",
            "url": "https://a/app.js", "lineNumber": 7,
            "exception": {"description": "TypeError: x is undefined"}}});
        let m = console_message("Runtime.exceptionThrown", &params).unwrap();
        assert_eq!(m.text, "TypeError: x is undefined");
        assert_eq!(
            (m.source.as_str(), m.level.as_str(), m.line_number),
            ("exception", "error", Some(7))
        );
        let bare = json!({"exceptionDetails": {"text": "Uncaught"}});
        assert_eq!(
            console_message("Runtime.exceptionThrown", &bare)
                .unwrap()
                .text,
            "Uncaught"
        );
        assert!(console_message("Runtime.executionContextCreated", &bare).is_none());
    }
}
