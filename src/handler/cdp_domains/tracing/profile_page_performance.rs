use super::coverage;
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
    description = "Traces the page for duration_ms and returns FCP, LCP, DCL, Load, total blocking time and the top 5 long tasks; 'action': reload with disable_cache measures a cold load; 'save_path' saves the raw trace (.json or .json.gz) on Chrome; 'insights': true adds what explains them (LCP breakdown and discovery, INP, layout shifts, render-blocking and slowest requests, long scripts, DOM size, oversized/unsized images, third-party weight, forced reflows on Chrome, findings), and with 'reload' on Chrome the JavaScript and CSS each file left unused. On Firefox: partial, no trace (vitals and insights from the Web Performance APIs and DOM; blocking time, long tasks, layout shifts, render-blocking requests, long scripts, forced reflows and unused code are null, with notes; 'save_path' fails)."
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
    pub action: Option<ProfileAction>,

    /// Bypass the cache while tracing. Default: false.
    #[serde(default)]
    pub disable_cache: Option<bool>,

    /// Absolute path of a .json or .json.gz file (under the upload roots) to save the raw trace to, for Chrome DevTools or Perfetto (Chrome only).
    #[serde(default)]
    pub save_path: Option<String>,

    /// Add what explains the numbers (LCP element, breakdown and discovery, INP, layout shifts, render-blocking and slowest requests, scripts of long frames, DOM size, oversized/unsized images, third-party origins, forced reflows on Chrome, findings; with action 'reload' on Chrome, the unused JavaScript and CSS of the load). Default: false.
    #[serde(default)]
    pub insights: Option<bool>,
}

/// What `profile_page_performance` does to the page while it traces. Each
/// variant is renamed on its own: the schema derive ignores `rename_all`
/// (ISSUE-82).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema,
)]
pub enum ProfileAction {
    /// Trace the page as it is.
    #[serde(rename = "none")]
    None,
    /// Reload the page, to measure its load.
    #[serde(rename = "reload")]
    Reload,
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
const CATEGORIES: &str = "devtools.timeline,v8.execute,blink.user_timing,blink,cc,gpu,toplevel,disabled-by-default-devtools.timeline.stack";

impl ProfilePagePerformanceTool {
    pub async fn handle(
        params: CallToolRequestParams,
        handler: &McpHandler,
    ) -> Result<CallToolResult, CallToolError> {
        let args: ProfilePagePerformanceTool = crate::handler::parse_args(params)?;
        let trace_file = args.trace_file(&handler.upload_roots)?;
        let session = handler.session(args.instance_id.clone()).await?;
        let target = session.target(args.tab_id.clone()).await?;
        // The completion channel lives in the tab's tracing state, matching
        // where the tracing listener pumps Tracing.tracingComplete for that tab.
        let tracing_state = session.tracing_state(args.tab_id.clone())?;
        let duration = args.duration_ms.unwrap_or(3000).clamp(500, 15000);
        let with_insights = args.insights.unwrap_or(false);
        // Coverage spans the reload; it is stopped whatever the trace does.
        let recording = match with_insights && args.reloads() {
            true => Some(coverage::start(&target).await),
            false => None,
        };
        let raw_json_data = traced(&target, &tracing_state, &args, duration).await;
        let unused = match recording {
            Some(Ok(recording)) => recording.finish(&target).await,
            Some(Err(why)) => Err(why),
            None => Err(coverage::NEEDS_RELOAD.to_string()),
        };
        let raw_json_data = raw_json_data.map_err(CallToolError::from_message)?;
        let summary = Self::analyze_trace(&raw_json_data, duration);
        let extras = with_insights.then(|| TraceInsights {
            unused,
            reflows: forced_reflows(&trace_events(&raw_json_data)),
        });
        let saved = match &trace_file {
            Some(path) => Some(
                write_trace(path, &raw_json_data)
                    .await
                    .map_err(CallToolError::from_message)?,
            ),
            None => None,
        };
        reply(&target, &summary, (extras, saved)).await
    }

    /// The file the raw trace is to be saved to, checked before anything
    /// is recorded (`save_pdf`'s rules, with .json or .json.gz).
    pub(crate) fn trace_file(
        &self,
        roots: &[std::path::PathBuf],
    ) -> Result<Option<std::path::PathBuf>, CallToolError> {
        let Some(path) = &self.save_path else {
            return Ok(None);
        };
        let endings = (".json or .json.gz", [".json", ".json.gz"].as_slice());
        crate::handler::cdp_domains::page::save_pdf::output_path(path, roots, endings)
            .map(Some)
            .map_err(CallToolError::from_message)
    }

    /// Whether the page is reloaded while profiling.
    pub(crate) fn reloads(&self) -> bool {
        self.action == Some(ProfileAction::Reload)
    }

    fn analyze_trace(raw_json: &str, duration_ms: u64) -> PerformanceSummary {
        let mut summary = PerformanceSummary {
            profile_duration_ms: duration_ms,
            ..Default::default()
        };
        let events = trace_events(raw_json);
        // Timestamps (microseconds) are relative to the first navigationStart.
        let base_ts = events
            .iter()
            .filter(|ev| ev.get("name").and_then(|v| v.as_str()) == Some("navigationStart"))
            .find_map(|ev| ev.get("ts").and_then(|v| v.as_f64()))
            .unwrap_or(0.0);
        let mut long_tasks = Vec::new();
        for ev in &events {
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
    let reload = args.reloads();
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

/// Writes the raw trace to `path`, gzipped when it ends in `.gz`:
/// `{path, bytes}` (the bytes written).
async fn write_trace(path: &std::path::Path, raw: &str) -> Result<serde_json::Value, String> {
    let gzipped = path.to_string_lossy().to_ascii_lowercase().ends_with(".gz");
    let bytes = match gzipped {
        true => {
            use std::io::Write;
            let mut encoder =
                flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            encoder
                .write_all(raw.as_bytes())
                .and_then(|()| encoder.finish())
                .map_err(|e| format!("could not compress the trace: {e}"))?
        }
        false => raw.as_bytes().to_vec(),
    };
    tokio::fs::write(path, &bytes)
        .await
        .map_err(|e| format!("could not write '{}': {e}", path.display()))?;
    Ok(json!({"path": path.display().to_string(), "bytes": bytes.len()}))
}

/// The trace's events: Chrome writes a top-level array or an object with
/// `traceEvents`; nothing for anything else.
fn trace_events(raw: &str) -> Vec<serde_json::Value> {
    let root = serde_json::from_str::<serde_json::Value>(raw).unwrap_or_default();
    match root {
        serde_json::Value::Array(events) => events,
        mut object => match object.get_mut("traceEvents").map(serde_json::Value::take) {
            Some(serde_json::Value::Array(events)) => events,
            _ => Vec::new(),
        },
    }
}

/// Most call sites `forced_reflows` lists.
const TOP_CALL_SITES: usize = 5;

/// The forced reflows of the trace: `Layout` and `UpdateLayoutTree` events
/// with a JavaScript stack (Chrome records one only when a script forced
/// them, with `disabled-by-default-devtools.timeline.stack`), grouped by the
/// frame that forced them: `{count, total_ms, top_call_sites: [{function,
/// url, line, count, total_ms}]}`.
fn forced_reflows(events: &[serde_json::Value]) -> serde_json::Value {
    let round = |ms: f64| (ms * 10.0).round() / 10.0 + 0.0;
    let mut sites: Vec<(serde_json::Value, u64, f64)> = Vec::new();
    for ev in events {
        let name = event_str(ev, "name");
        if event_str(ev, "ph") != "X" || !matches!(name, "Layout" | "UpdateLayoutTree") {
            continue;
        }
        let stack = &ev["args"]["beginData"]["stackTrace"];
        let stack = if stack.is_array() {
            stack
        } else {
            &ev["args"]["data"]["stackTrace"]
        };
        let Some(frame) = stack.get(0) else {
            continue;
        };
        let site = json!({"function": frame["functionName"], "url": frame["url"], "line": frame["lineNumber"]});
        let ms = event_f64(ev, "dur") / 1000.0;
        match sites.iter_mut().find(|(known, _, _)| *known == site) {
            Some((_, count, total)) => {
                *count += 1;
                *total += ms;
            }
            None => sites.push((site, 1, ms)),
        }
    }
    let count: u64 = sites.iter().map(|s| s.1).sum();
    let total: f64 = sites.iter().map(|s| s.2).sum();
    sites.sort_by(|a, b| b.2.total_cmp(&a.2));
    let top: Vec<serde_json::Value> = sites
        .into_iter()
        .take(TOP_CALL_SITES)
        .map(|(mut site, count, ms)| {
            site["count"] = json!(count);
            site["total_ms"] = json!(round(ms));
            site
        })
        .collect();
    json!({"count": count, "total_ms": round(total), "top_call_sites": top})
}

/// What insights add from the recording itself (Chrome).
struct TraceInsights {
    /// The reload's unused code, or why there is none.
    unused: Result<serde_json::Value, String>,
    /// The trace's forced reflows.
    reflows: serde_json::Value,
}

/// The reply: the summary, with the page's insights, the reload's unused
/// code (or why there is none) and the forced reflows when insights were
/// asked for, and where the trace was saved.
async fn reply(
    target: &Target,
    summary: &PerformanceSummary,
    (extras, saved): (Option<TraceInsights>, Option<serde_json::Value>),
) -> Result<CallToolResult, CallToolError> {
    let mut result_json = serde_json::to_value(summary).unwrap_or_default();
    // Nothing loaded while tracing: the page's own timings (ISSUE-92).
    if nothing_loaded(&result_json) {
        let page = crate::util::js::PAGE_VITALS;
        let vitals = crate::handler::snippet_value(target, page).await;
        with_page_vitals(&mut result_json, vitals.ok().as_ref());
    }
    if let Some(saved) = saved {
        result_json["saved_trace"] = saved;
    }
    if let Some(extras) = extras {
        let found = crate::handler::snippet_value(target, insights::SNIPPET).await?;
        let mut annotated = insights::annotate(found);
        insights::add_unused_code(&mut annotated, extras.unused);
        insights::add_forced_reflows(&mut annotated, Ok(extras.reflows));
        result_json["insights"] = annotated;
    }
    let text = serde_json::to_string_pretty(&result_json).unwrap_or_default();
    Ok(CallToolResult::text_content(vec![text.into()]))
}

/// The load timings a trace gives (from a load during it).
const LOAD_TIMINGS: [&str; 4] = [
    "first_contentful_paint_ms",
    "largest_contentful_paint_ms",
    "dom_content_loaded_ms",
    "load_event_ms",
];

/// Why a profile's load timings are the page's own (ISSUE-92).
const PAGE_VITALS_NOTE: &str = "nothing loaded while tracing: first_contentful_paint_ms, largest_contentful_paint_ms, dom_content_loaded_ms and load_event_ms are the current page's, from the Web Performance APIs (ms since its navigation started), while total_blocking_time_ms and top_long_tasks cover only the traced window; use action 'reload' to trace a load";

/// Why a profile without a load has no timings at all (ISSUE-92).
const NO_PAGE_VITALS_NOTE: &str = "nothing loaded while tracing, and the page reported no load timings of its own (none yet, or it could not be read): use action 'reload' to trace a load";

/// Whether a profile `summary` has none of the load timings: no load
/// happened while tracing (all were `null` without a word, ISSUE-92).
fn nothing_loaded(summary: &serde_json::Value) -> bool {
    LOAD_TIMINGS.iter().all(|key| summary[*key].is_null())
}

/// Fills a profile `summary`'s load timings from the page's own
/// (`util::js::PAGE_VITALS`, as on Firefox; `None` when they could not be
/// read), with a note saying where they come from, or that there are none.
fn with_page_vitals(summary: &mut serde_json::Value, vitals: Option<&serde_json::Value>) {
    let round = |ms: f64| (ms * 10.0).round() / 10.0;
    let mut found = false;
    for key in LOAD_TIMINGS {
        let value = vitals.and_then(|v| v[key].as_f64()).map(round);
        found |= value.is_some();
        summary[key] = json!(value);
    }
    let note = match found {
        true => PAGE_VITALS_NOTE,
        false => NO_PAGE_VITALS_NOTE,
    };
    match summary["notes"].as_array_mut() {
        Some(notes) => notes.push(json!(note)),
        None => summary["notes"] = json!([note]),
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
        assert_eq!(tool.action, Some(ProfileAction::Reload));
        assert!(tool.reloads());
        assert_eq!(tool.disable_cache, Some(true));
    }

    /// ISSUE-82: 'action' lists its values in the schema, and a value it
    /// does not list is refused instead of silently profiling without a
    /// reload.
    #[test]
    fn action_is_an_enum() {
        // Given the tool's schema as clients see it
        let tool = crate::handler::schema_compat::normalize_tool_with_options(
            ProfilePagePerformanceTool::tool(),
            true,
        );
        let properties = tool.input_schema.properties.unwrap_or_default();
        // Then 'action' lists 'none' and 'reload'
        assert_eq!(properties["action"]["enum"], json!(["none", "reload"]));
        // And an unlisted value fails to parse
        let parsed =
            serde_json::from_value::<ProfilePagePerformanceTool>(json!({"action": "Reload"}));
        assert!(parsed.is_err());
        // And no action does not reload
        let none: ProfilePagePerformanceTool =
            serde_json::from_value(json!({"action": "none"})).unwrap();
        assert!(!none.reloads());
    }

    /// ISSUE-92 (Chrome): a trace in which nothing loaded (no reload) gives
    /// the current page's FCP, LCP, DCL and Load from the Web Performance
    /// APIs, as Firefox does, saying so; a trace with a load keeps its own.
    #[test]
    fn a_trace_without_a_load_reads_the_page() {
        let vitals = json!({"first_contentful_paint_ms": 28.5, "largest_contentful_paint_ms": 30.0,
            "dom_content_loaded_ms": 11.0, "load_event_ms": 11.999999, "lcp_supported": true});
        let mut idle = json!({"profile_duration_ms": 2000, "first_contentful_paint_ms": null,
            "largest_contentful_paint_ms": null, "dom_content_loaded_ms": null, "load_event_ms": null});
        assert!(nothing_loaded(&idle));
        let mut unread = idle.clone();
        with_page_vitals(&mut idle, Some(&vitals));
        assert_eq!(idle["first_contentful_paint_ms"], 28.5);
        assert_eq!(idle["load_event_ms"], 12.0);
        assert_eq!(idle["notes"][0], PAGE_VITALS_NOTE);
        // A page that could not be read, or reports nothing, says so
        with_page_vitals(&mut unread, None);
        assert!(unread["load_event_ms"].is_null());
        assert_eq!(unread["notes"][0], NO_PAGE_VITALS_NOTE);
        let loaded = json!({"first_contentful_paint_ms": 40.0, "largest_contentful_paint_ms": null,
            "dom_content_loaded_ms": null, "load_event_ms": null});
        assert!(!nothing_loaded(&loaded));
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

    /// ISSUE-56: layouts and style recalculations run with a script on the
    /// stack (Chrome records it only for those) are forced reflows, grouped
    /// by the frame that forced them; the others are the page's own.
    #[test]
    fn forced_reflows_group_by_call_site() {
        assert!(CATEGORIES.contains("disabled-by-default-devtools.timeline.stack"));
        let frame = json!([{"functionName": "thrash", "url": "http://a/t.js", "lineNumber": 6, "columnNumber": 18},
            {"functionName": "", "url": "http://a/t.js", "lineNumber": 11}]);
        let forced = |name: &str, dur: u64| json!({"name": name, "ph": "X", "dur": dur, "args": {"beginData": {"stackTrace": frame}}});
        let events = vec![
            forced("Layout", 2000),
            forced("Layout", 2000),
            forced("UpdateLayoutTree", 1000),
            json!({"name": "Layout", "ph": "X", "dur": 9000, "args": {"beginData": {}}}),
            json!({"name": "RunTask", "ph": "X", "dur": 9000}),
        ];
        let reflows = forced_reflows(&events);
        assert_eq!(
            reflows,
            json!({"count": 3, "total_ms": 5.0, "top_call_sites": [
                {"function": "thrash", "url": "http://a/t.js", "line": 6, "count": 3, "total_ms": 5.0}]})
        );
        let empty = forced_reflows(&[]);
        assert_eq!(empty["count"], 0);
        // ISSUE-58: no `-0.0` in the reply
        assert_eq!(
            empty["total_ms"].as_f64().unwrap().to_bits(),
            0.0f64.to_bits()
        );
    }

    /// ISSUE-56: `save_path` must name an absolute .json or .json.gz file
    /// under the upload roots, checked before anything is recorded.
    #[test]
    fn trace_file_is_checked_first() {
        let root = tempfile::tempdir().unwrap();
        let roots = vec![root.path().to_path_buf()];
        let tool = |path: &str| -> ProfilePagePerformanceTool {
            serde_json::from_value(json!({"save_path": path})).unwrap()
        };
        let fails = |path: &str| tool(path).trace_file(&roots).unwrap_err().to_string();
        let inside = |name: &str| root.path().join(name).display().to_string();
        assert!(
            fails("trace.json").contains("absolute"),
            "{}",
            fails("trace.json")
        );
        assert!(fails(&inside("trace.txt")).contains("must name a .json or .json.gz file"));
        assert!(
            tool(&inside("trace.json"))
                .trace_file(&roots)
                .unwrap()
                .is_some()
        );
        assert!(
            tool(&inside("Trace.JSON.GZ"))
                .trace_file(&roots)
                .unwrap()
                .is_some()
        );
        let none: ProfilePagePerformanceTool = serde_json::from_value(json!({})).unwrap();
        assert!(none.trace_file(&roots).unwrap().is_none());
    }

    /// ISSUE-56: the raw trace is written as it came, or gzipped for a
    /// .gz name; the reply gives the path and size.
    #[tokio::test]
    async fn trace_is_written_plain_or_gzipped() {
        use std::io::Read;
        let root = tempfile::tempdir().unwrap();
        let raw = r#"{"traceEvents":[{"name":"Layout"}]}"#;
        let plain = root.path().join("t.json");
        let saved = write_trace(&plain, raw).await.unwrap();
        assert_eq!(std::fs::read_to_string(&plain).unwrap(), raw);
        assert_eq!(saved["bytes"], raw.len());
        let packed = root.path().join("t.json.gz");
        let saved = write_trace(&packed, raw).await.unwrap();
        let mut unpacked = String::new();
        let file = std::fs::File::open(&packed).unwrap();
        flate2::read::GzDecoder::new(file)
            .read_to_string(&mut unpacked)
            .unwrap();
        assert_eq!(unpacked, raw);
        assert_eq!(saved["path"], packed.display().to_string());
        assert!(saved["bytes"].as_u64().unwrap() > 0);
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
