//! Partial `get_performance_metrics` and `profile_page_performance` on
//! Firefox, from the standard Web Performance APIs (Firefox has no
//! equivalent of CDP's Performance and Tracing domains). Each reply says
//! what Firefox cannot give, instead of answering an empty or made-up value.

use super::page;
use crate::handler::cdp_domains::performance::get_performance_metrics::GetPerformanceMetricsTool;
use crate::handler::cdp_domains::tracing::profile_page_performance::ProfilePagePerformanceTool;
use crate::handler::{McpHandler, parse_args};
use rust_mcp_sdk::schema::{CallToolError, CallToolRequestParams, CallToolResult};
use serde_json::{Value, json};

type ToolResult = Result<CallToolResult, CallToolError>;

/// What Firefox's `get_performance_metrics` cannot report.
pub(crate) const METRICS_NOTE: &str = "Firefox gives only Nodes, Frames, Resources and TransferSize (bytes), read from the page with the Web Performance APIs; JSHeapUsedSize, JSHeapTotalSize, LayoutCount, RecalcStyleCount, ScriptDuration, TaskDuration and the other Chrome metrics need CDP's Performance domain";

/// What Firefox's `profile_page_performance` cannot report.
pub(crate) const LONG_TASKS_NOTE: &str =
    "Firefox has no Long Tasks API: total_blocking_time_ms and top_long_tasks are null";

/// `get_performance_metrics`: the metrics the page can read, plus a note.
pub(crate) async fn metrics(params: CallToolRequestParams, handler: &McpHandler) -> ToolResult {
    let args: GetPerformanceMetricsTool = parse_args(params)?;
    let session = handler.firefox(args.instance_id.as_deref()).await?;
    let (_, tab) = session.tab(args.tab_id.as_deref()).await?;
    let metrics = page::run_snippet(&tab.tab, crate::util::js::PAGE_METRICS).await?;
    Ok(CallToolResult::text_content(vec![
        serde_json::to_string_pretty(&metrics)
            .unwrap_or_default()
            .into(),
        METRICS_NOTE.into(),
    ]))
}

/// `profile_page_performance`: reloads if asked (bypassing the cache when
/// `disable_cache`), lets the page run `duration_ms`, then reads the
/// page-load vitals. Same keys as on Chrome.
pub(crate) async fn profile(params: CallToolRequestParams, handler: &McpHandler) -> ToolResult {
    let args: ProfilePagePerformanceTool = parse_args(params)?;
    let session = handler.firefox(args.instance_id.as_deref()).await?;
    let (_, tab) = session.tab(args.tab_id.as_deref()).await?;
    let duration = args.duration_ms.unwrap_or(3000).clamp(500, 15000);
    if args.action.as_deref() == Some("reload") {
        if args.disable_cache.unwrap_or(false) {
            page::command(
                &tab.tab,
                "browsingContext.reload",
                json!({"context": tab.tab.context_id(), "ignoreCache": true, "wait": "complete"}),
            )
            .await?;
        } else {
            page::reload(&tab.tab).await?;
        }
    }
    tokio::time::sleep(std::time::Duration::from_millis(duration)).await;
    let vitals = page::run_snippet(&tab.tab, crate::util::js::PAGE_VITALS).await?;
    let summary = summary(&vitals, duration);
    Ok(CallToolResult::text_content(vec![
        serde_json::to_string_pretty(&summary)
            .unwrap_or_default()
            .into(),
    ]))
}

/// The profile reply from the page's vitals: Chrome's keys, `null` where
/// Firefox has nothing, and notes saying why.
pub(crate) fn summary(vitals: &Value, duration: u64) -> Value {
    let field = |key: &str| vitals.get(key).cloned().unwrap_or(Value::Null);
    let mut notes = vec![LONG_TASKS_NOTE.to_string()];
    if vitals.get("lcp_supported") != Some(&Value::Bool(true)) {
        notes.push("this Firefox does not report largest-contentful-paint: largest_contentful_paint_ms is null".into());
    }
    if vitals.get("long_tasks_supported") == Some(&Value::Bool(true)) {
        notes[0] = "Firefox reports long tasks, but they are not read yet: total_blocking_time_ms and top_long_tasks are null".into();
    }
    json!({
        "profile_duration_ms": duration,
        "first_contentful_paint_ms": field("first_contentful_paint_ms"),
        "largest_contentful_paint_ms": field("largest_contentful_paint_ms"),
        "dom_content_loaded_ms": field("dom_content_loaded_ms"),
        "load_event_ms": field("load_event_ms"),
        "total_blocking_time_ms": Value::Null,
        "top_long_tasks": Value::Null,
        "notes": notes,
    })
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{self as ts, evaluate_success};
    use super::*;
    use bidi_browser_lite::test_support::MockFirefox;

    /// FPF-01: Firefox metrics come from the page, with the note.
    #[tokio::test]
    async fn firefox_metrics_shape_and_notes() {
        // Given a page with 12 nodes
        let mock = MockFirefox::start(false).await.unwrap();
        mock.bidi().respond_with("script.evaluate", |_| {
            evaluate_success(json!({"type": "object", "value": [
                ["Nodes", {"type": "number", "value": 12}],
                ["Frames", {"type": "number", "value": 1}],
                ["Resources", {"type": "number", "value": 3}],
                ["TransferSize", {"type": "number", "value": 2048}],
            ]}))
        });
        let handler = ts::handler(&mock);
        // When get_performance_metrics runs
        let reply = metrics(ts::call("get_performance_metrics", json!({})), &handler)
            .await
            .unwrap();
        // Then the metrics are JSON and the note says what is missing
        let body: Value = serde_json::from_str(&ts::text(&reply)).unwrap();
        assert_eq!(body["Nodes"], 12);
        let note = reply.content[1].as_text_content().unwrap().text.clone();
        assert!(note.contains("JSHeapUsedSize"), "{note}");
        let sent = &mock.bidi().commands_named("script.evaluate")[0].params;
        assert_eq!(sent["expression"], crate::util::js::PAGE_METRICS);
        ts::stop(&handler).await;
    }

    /// FPF-02 / FPF-03: profile with reload answers Chrome's keys, `null`
    /// (never a silent `{}`) where Firefox has nothing, with notes.
    #[tokio::test]
    async fn firefox_profile_with_reload() {
        // Given a page whose vitals Firefox reports, without LCP
        let mock = MockFirefox::start(false).await.unwrap();
        mock.bidi().respond_with("script.evaluate", |_| {
            evaluate_success(json!({"type": "object", "value": [
                ["dom_content_loaded_ms", {"type": "number", "value": 120.5}],
                ["load_event_ms", {"type": "number", "value": 300}],
                ["first_contentful_paint_ms", {"type": "number", "value": 90}],
                ["largest_contentful_paint_ms", {"type": "null"}],
                ["lcp_supported", {"type": "boolean", "value": false}],
                ["long_tasks_supported", {"type": "boolean", "value": false}],
            ]}))
        });
        mock.bidi().respond_with(
            "browsingContext.reload",
            |_| json!({"navigation": "n", "url": "about:blank"}),
        );
        let handler = ts::handler(&mock);
        // When it profiles a cold reload
        let args = json!({"action": "reload", "disable_cache": true, "duration_ms": 500});
        let reply = profile(ts::call("profile_page_performance", args), &handler)
            .await
            .unwrap();
        // Then it reloaded bypassing the cache and reports the vitals
        let reload = &mock.bidi().commands_named("browsingContext.reload")[0].params;
        assert_eq!(
            (reload["ignoreCache"].as_bool(), reload["wait"].as_str()),
            (Some(true), Some("complete"))
        );
        let body: Value = serde_json::from_str(&ts::text(&reply)).unwrap();
        assert_eq!(body["dom_content_loaded_ms"], 120.5);
        assert_eq!(body["first_contentful_paint_ms"], 90);
        assert!(body["largest_contentful_paint_ms"].is_null());
        assert!(body["total_blocking_time_ms"].is_null() && body["top_long_tasks"].is_null());
        let notes = body["notes"].as_array().unwrap();
        assert_eq!(notes.len(), 2, "{body}");
        ts::stop(&handler).await;
    }
}
