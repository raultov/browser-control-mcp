use crate::handler::{McpHandler, insights};
use rust_mcp_sdk::{
    macros,
    schema::{CallToolError, CallToolRequestParams, CallToolResult},
};
use serde_json::json;
use std::time::Duration;
use tokio::sync::mpsc;

#[macros::mcp_tool(
    name = "profile_page_performance",
    description = "Traces the page for duration_ms and returns FCP, LCP, DCL, Load, total blocking time and the top 5 long tasks; 'action': reload with disable_cache measures a cold load; 'insights': true adds what explains them, with findings. On Firefox: partial, no trace (vitals from the Web Performance APIs; blocking time, long tasks, layout shifts and render-blocking requests are null, with notes)."
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct ProfilePagePerformanceTool {
    /// Instance id; omit for the default instance.
    pub instance_id: Option<String>,
    /// Tab id; omit for the active tab.
    pub tab_id: Option<String>,
    /// 500 to 15000. Default: 3000.
    #[serde(default)]
    pub duration_ms: Option<u64>,

    /// 'none' (default) or 'reload'.
    #[serde(default)]
    pub action: Option<String>,

    /// Bypass the cache while tracing. Default: false.
    #[serde(default)]
    pub disable_cache: Option<bool>,

    /// Add what explains the numbers (LCP element and breakdown, layout shifts, render-blocking and slowest requests, scripts of long frames, findings). Default: false.
    #[serde(default)]
    pub insights: Option<bool>,
}

#[derive(Debug, serde::Serialize)]
struct LongTask {
    name: String,
    duration_ms: f64,
    start_time_ms: f64,
    args: Option<serde_json::Value>,
}

#[derive(Debug, Default, serde::Serialize)]
struct PerformanceSummary {
    profile_duration_ms: u64,
    first_contentful_paint_ms: Option<f64>,
    largest_contentful_paint_ms: Option<f64>,
    dom_content_loaded_ms: Option<f64>,
    load_event_ms: Option<f64>,
    total_blocking_time_ms: f64,
    top_long_tasks: Vec<LongTask>,
}

type Target = crate::handler::cdp_domains::cdp_target::CdpTarget;

/// How many long tasks the summary keeps (the description promises 5).
const TOP_LONG_TASKS: usize = 5;

/// How long to wait for `Tracing.tracingComplete` after `Tracing.end`.
#[cfg(not(test))]
const COMPLETION_TIMEOUT: Duration = Duration::from_secs(10);
#[cfg(test)]
const COMPLETION_TIMEOUT: Duration = Duration::from_millis(100);

/// Trace categories recorded by `profile_page_performance`.
const CATEGORIES: &str = "devtools.timeline,v8.execute,blink.user_timing,blink,cc,gpu,toplevel";

impl ProfilePagePerformanceTool {
    pub async fn handle(
        params: CallToolRequestParams,
        handler: &McpHandler,
    ) -> Result<CallToolResult, CallToolError> {
        let args: ProfilePagePerformanceTool = crate::handler::parse_args(params)?;
        let session = handler.session(args.instance_id.clone()).await?;
        let target = session.target(args.tab_id.clone()).await?;
        // The completion channel lives in the tab's tracing state, matching
        // where the tracing listener pumps Tracing.tracingComplete for that tab.
        let tracing_state = session.tracing_state(args.tab_id.clone())?;
        let duration = args.duration_ms.unwrap_or(3000).clamp(500, 15000);
        let raw_json_data = traced(&target, &tracing_state, &args, duration)
            .await
            .map_err(CallToolError::from_message)?;
        let summary = Self::analyze_trace(&raw_json_data, duration);
        reply(&target, &summary, args.insights.unwrap_or(false)).await
    }

    fn analyze_trace(raw_json: &str, duration_ms: u64) -> PerformanceSummary {
        let mut summary = PerformanceSummary {
            profile_duration_ms: duration_ms,
            ..Default::default()
        };
        let Ok(root) = serde_json::from_str::<serde_json::Value>(raw_json) else {
            return summary;
        };
        // Chrome trace is usually a top-level array OR an object with "traceEvents" array
        let Some(events) = root
            .as_array()
            .or_else(|| root.get("traceEvents").and_then(|v| v.as_array()))
        else {
            return summary;
        };
        // Timestamps (microseconds) are relative to the first navigationStart.
        let base_ts = events
            .iter()
            .filter(|ev| ev.get("name").and_then(|v| v.as_str()) == Some("navigationStart"))
            .find_map(|ev| ev.get("ts").and_then(|v| v.as_f64()))
            .unwrap_or(0.0);
        let mut long_tasks = Vec::new();
        for ev in events {
            apply_mark(&mut summary, ev, base_ts);
            if let Some(task) = long_task(ev, base_ts) {
                summary.total_blocking_time_ms += task.duration_ms - 50.0;
                long_tasks.push(task);
            }
        }
        // Sort long tasks by duration descending and keep the top 5 (as described)
        long_tasks.sort_by(|a, b| {
            b.duration_ms
                .partial_cmp(&a.duration_ms)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        long_tasks.truncate(TOP_LONG_TASKS);
        summary.top_long_tasks = long_tasks;
        summary
    }
}

/// Traces the page for `duration` ms (reloading and bypassing the cache as
/// `args` say) and returns the raw trace. The cache and the completion
/// channel are restored on every path, failures included. Errors are text:
/// the tool error type is not `Send` across the awaits.
async fn traced(
    target: &Target,
    tracing_state: &std::sync::Arc<tokio::sync::Mutex<super::TracingState>>,
    args: &ProfilePagePerformanceTool,
    duration: u64,
) -> Result<String, String> {
    let disable_cache = args.disable_cache.unwrap_or(false);
    if disable_cache {
        set_cache_disabled(target, true).await;
    }
    let (tx, mut rx) = mpsc::channel(1);
    tracing_state.lock().await.completion_channel = Some(tx);
    let reload = args.action.as_deref() == Some("reload");
    let recorded = record(target, &mut rx, (duration, reload, disable_cache))
        .await
        .map_err(|e| e.to_string());
    let raw_json_data = match &recorded {
        Ok(handle) => read_stream(target, handle).await,
        Err(_) => String::new(),
    };
    tracing_state.lock().await.completion_channel = None;
    if disable_cache {
        set_cache_disabled(target, false).await;
    }
    recorded.map(|_| raw_json_data)
}

/// The reply: the summary, with the page's insights when asked for.
async fn reply(
    target: &Target,
    summary: &PerformanceSummary,
    with_insights: bool,
) -> Result<CallToolResult, CallToolError> {
    let mut result_json = serde_json::to_value(summary).unwrap_or_default();
    if with_insights {
        let found = crate::handler::snippet_value(target, insights::SNIPPET).await?;
        result_json["insights"] = insights::annotate(found);
    }
    let text = serde_json::to_string_pretty(&result_json).unwrap_or_default();
    Ok(CallToolResult::text_content(vec![text.into()]))
}

/// Starts tracing, optionally reloads, waits `duration` ms, stops, and
/// returns the trace stream handle announced by `Tracing.tracingComplete`.
async fn record(
    target: &Target,
    completion: &mut mpsc::Receiver<String>,
    (duration, reload, ignore_cache): (u64, bool, bool),
) -> Result<String, CallToolError> {
    let start = json!({"transferMode": "ReturnAsStream", "categories": CATEGORIES});
    crate::handler::send_or(target, "Tracing.start", start, "Failed to start tracing").await?;
    if reload {
        let _ = target
            .send_raw_command("Page.reload", json!({ "ignoreCache": ignore_cache }))
            .await;
    }
    tokio::time::sleep(Duration::from_millis(duration)).await;
    crate::handler::send_or(target, "Tracing.end", json!({}), "Failed to stop tracing").await?;
    match tokio::time::timeout(COMPLETION_TIMEOUT, completion.recv()).await {
        Ok(Some(handle)) => Ok(handle),
        _ => Err(CallToolError::from_message(
            "Timeout waiting for Tracing.tracingComplete",
        )),
    }
}

async fn set_cache_disabled(target: &Target, disabled: bool) {
    let params = json!({ "cacheDisabled": disabled });
    let _ = target
        .send_raw_command("Network.setCacheDisabled", params)
        .await;
}

/// Reads a CDP IO stream to the end, then closes it.
async fn read_stream(target: &Target, handle: &str) -> String {
    let mut data = String::new();
    loop {
        let reply = target
            .send_raw_command("IO.read", json!({ "handle": handle }))
            .await;
        let Some(result) = reply.ok().and_then(|r| r.result) else {
            break;
        };
        data.push_str(result.get("data").and_then(|v| v.as_str()).unwrap_or(""));
        if result.get("eof").and_then(|v| v.as_bool()) == Some(true) {
            break;
        }
    }
    let _ = target
        .send_raw_command("IO.close", json!({ "handle": handle }))
        .await;
    data
}

fn event_str<'a>(ev: &'a serde_json::Value, key: &str) -> &'a str {
    ev.get(key).and_then(|v| v.as_str()).unwrap_or("")
}

fn event_f64(ev: &serde_json::Value, key: &str) -> f64 {
    ev.get(key).and_then(|v| v.as_f64()).unwrap_or(0.0)
}

/// Web Vitals marks (`ph` R, I or O) after the navigation start.
fn apply_mark(summary: &mut PerformanceSummary, ev: &serde_json::Value, base_ts: f64) {
    if !matches!(event_str(ev, "ph"), "R" | "I" | "O") {
        return;
    }
    let ms = (event_f64(ev, "ts") - base_ts) / 1000.0;
    if ms <= 0.0 {
        return;
    }
    let slot = match event_str(ev, "name") {
        "firstContentfulPaint" => &mut summary.first_contentful_paint_ms,
        // LCP can have multiple candidates, keep the latest
        "largestContentfulPaint::Candidate" => &mut summary.largest_contentful_paint_ms,
        "domContentLoadedEventEnd" => &mut summary.dom_content_loaded_ms,
        "loadEventEnd" => &mut summary.load_event_ms,
        _ => return,
    };
    *slot = Some(ms);
}

/// A complete event (`ph` X) longer than 50 ms that runs script.
fn long_task(ev: &serde_json::Value, base_ts: f64) -> Option<LongTask> {
    let name = event_str(ev, "name");
    let dur_ms = event_f64(ev, "dur") / 1000.0;
    let start_ms = (event_f64(ev, "ts") - base_ts) / 1000.0;
    let is_script = matches!(name, "RunTask" | "EvaluateScript" | "FunctionCall");
    (event_str(ev, "ph") == "X" && dur_ms > 50.0 && is_script && start_ms > 0.0).then(|| LongTask {
        name: name.to_string(),
        duration_ms: dur_ms,
        start_time_ms: start_ms,
        args: ev.get("args").cloned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_profile_page_performance_tool_deserialization() {
        let json = json!({
            "duration_ms": 5000,
            "action": "reload",
            "disable_cache": true
        });
        let tool: ProfilePagePerformanceTool = serde_json::from_value(json).unwrap();
        assert_eq!(tool.duration_ms, Some(5000));
        assert_eq!(tool.action, Some("reload".to_string()));
        assert_eq!(tool.disable_cache, Some(true));
    }

    #[test]
    fn test_analyze_trace() {
        let trace_data = json!({
            "traceEvents": [
                { "name": "navigationStart", "ph": "R", "ts": 1000000 },
                { "name": "firstContentfulPaint", "ph": "R", "ts": 1200000 },
                { "name": "largestContentfulPaint::Candidate", "ph": "R", "ts": 1500000 },
                { "name": "domContentLoadedEventEnd", "ph": "R", "ts": 1800000 },
                { "name": "loadEventEnd", "ph": "R", "ts": 2000000 },
                // Long task (RunTask) - 100ms duration (dur is in micros)
                { "name": "RunTask", "ph": "X", "ts": 1300000, "dur": 100000, "args": { "src": "test.js" } },
                // Short task (ignored)
                { "name": "RunTask", "ph": "X", "ts": 1450000, "dur": 10000 },
                // Another long task (EvaluateScript) - 200ms
                { "name": "EvaluateScript", "ph": "X", "ts": 1600000, "dur": 200000 }
            ]
        });

        let summary = ProfilePagePerformanceTool::analyze_trace(&trace_data.to_string(), 3000);

        assert_eq!(summary.first_contentful_paint_ms, Some(200.0)); // (1.2s - 1.0s) = 200ms
        assert_eq!(summary.largest_contentful_paint_ms, Some(500.0));
        assert_eq!(summary.dom_content_loaded_ms, Some(800.0));
        assert_eq!(summary.load_event_ms, Some(1000.0));

        // TBT: (100-50) + (200-50) = 50 + 150 = 200ms
        assert_eq!(summary.total_blocking_time_ms, 200.0);
        assert_eq!(summary.top_long_tasks.len(), 2);
        assert_eq!(summary.top_long_tasks[0].duration_ms, 200.0);
        assert_eq!(summary.top_long_tasks[1].duration_ms, 100.0);
    }

    #[tokio::test]
    async fn test_profile_unknown_tab_fails_before_tracing_starts() {
        let handler = McpHandler::new_test();
        let params: CallToolRequestParams = serde_json::from_value(json!({
            "name": "profile_page_performance",
            "arguments": { "duration_ms": 500, "tab_id": "tab-nope" }
        }))
        .unwrap();
        let err = ProfilePagePerformanceTool::handle(params, &handler)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("not found"),
            "expected tab-not-found error instead of a 10s tracing timeout, got: {err}"
        );
    }

    /// FIX-06: profile_page_performance always restores state.
    #[tokio::test]
    async fn profiling_restores_state_on_failure() {
        use crate::handler::chrome_instance::MockChromeManager;
        use std::sync::Arc;
        // Given a browser that never announces Tracing.tracingComplete
        let port = crate::handler::cdp_domains::tests::spawn_mock_chrome_server().await;
        let mut handler = McpHandler::new_test();
        Arc::get_mut(&mut handler.default_session)
            .unwrap()
            .chrome_manager = Arc::new(tokio::sync::Mutex::new(MockChromeManager::new(port)));
        // When profiling with the cache disabled times out
        let params = serde_json::from_value(json!({"name": "profile_page_performance",
            "arguments": {"duration_ms": 500, "disable_cache": true}}))
        .unwrap();
        let result = ProfilePagePerformanceTool::handle(params, &handler).await;
        // Then it fails and the completion channel is cleared
        assert!(result.is_err());
        let session = handler.session(None).await.unwrap();
        assert!(
            session
                .tracing_state
                .lock()
                .await
                .completion_channel
                .is_none()
        );
        // And the code and the description agree on the number of long tasks
        let description = ProfilePagePerformanceTool::tool()
            .description
            .unwrap_or_default();
        assert!(description.contains(&format!("top {TOP_LONG_TASKS} long tasks")));
    }
}
