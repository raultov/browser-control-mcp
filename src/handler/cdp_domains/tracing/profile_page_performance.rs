use crate::handler::McpHandler;
use rust_mcp_sdk::{
    macros,
    schema::{CallToolError, CallToolRequestParams, CallToolResult},
};
use serde_json::json;
use std::time::Duration;
use tokio::sync::mpsc;

#[macros::mcp_tool(
    name = "profile_page_performance",
    description = "Records a performance trace of page execution, calculating Core Web Vitals (FCP, LCP, DCL, Load) and identifying Long Tasks (>50ms blocking). Side effects: may temporarily disable cache ('disable_cache' defaults to false); impacts page memory/CPU. Prerequisites: requires an active Chrome tab; trace recording uses background bandwidth. Returns: JSON with vitals, blocking time, and top 5 long tasks. Use this to optimize performance, identify bottlenecks, measure cold starts. Alternatives: browser DevTools Performance tab, real user monitoring (RUM). Chrome only (not available on Firefox)."
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct ProfilePagePerformanceTool {
    /// Chrome instance id from open_instance/list_instances. Omit for the default instance.
    pub instance_id: Option<String>,
    /// The Tab ID of the target tab. Omit to use the active tab.
    pub tab_id: Option<String>,
    /// Recording duration in milliseconds. Constraints: integer between 500 and 15000. Interactions: longer duration captures more data; use 3000-5000 for typical pages. Defaults to: 3000.
    #[serde(default)]
    pub duration_ms: Option<u64>,

    /// Action to trigger during tracing. Constraints: 'none' or 'reload'. Interactions: 'reload' restarts page recording from initial load (useful with disable_cache=true). Defaults to: "none".
    #[serde(default)]
    pub action: Option<String>,

    /// Disable network cache during trace. Constraints: boolean. Interactions: when true with action='reload', simulates cold start; cache restored after profiling. Defaults to: false.
    #[serde(default)]
    pub disable_cache: Option<bool>,
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
        let duration = args.duration_ms.unwrap_or(3000).clamp(500, 15000);
        let disable_cache = args.disable_cache.unwrap_or(false);

        if disable_cache {
            set_cache_disabled(&target, true).await;
        }
        // The completion channel lives in the tab's tracing state, matching
        // where the tracing listener pumps Tracing.tracingComplete for that tab.
        let (tx, mut rx) = mpsc::channel(1);
        let tracing_state = session.tracing_state(args.tab_id.clone())?;
        tracing_state.lock().await.completion_channel = Some(tx);

        let reload = args.action.as_deref() == Some("reload");
        let stream_handle = record(&target, &mut rx, (duration, reload, disable_cache)).await?;
        let raw_json_data = read_stream(&target, &stream_handle).await;
        if disable_cache {
            set_cache_disabled(&target, false).await;
        }
        let summary = Self::analyze_trace(&raw_json_data, duration);
        let result_json = serde_json::to_value(&summary).unwrap_or_default();
        Ok(CallToolResult::text_content(vec![
            serde_json::to_string_pretty(&result_json)
                .unwrap_or_default()
                .into(),
        ]))
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
        // Sort long tasks by duration descending and keep top 10
        long_tasks.sort_by(|a, b| {
            b.duration_ms
                .partial_cmp(&a.duration_ms)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        long_tasks.truncate(10);
        summary.top_long_tasks = long_tasks;
        summary
    }
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
    let _ = target.send_raw_command("Tracing.end", json!({})).await;
    match tokio::time::timeout(Duration::from_secs(10), completion.recv()).await {
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
}
