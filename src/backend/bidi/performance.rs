//! Firefox, from the standard Web Performance APIs (Firefox has no
//! equivalent of CDP's Performance and Tracing domains). Each reply says
//! what Firefox cannot give, instead of answering an empty or made-up value.

use super::page;
use crate::backend::BackendError;
use crate::handler::cdp_domains::performance::get_performance_metrics::{
    GetPerformanceMetricsTool, NOT_COLLECTED_NOTE,
};
use crate::handler::cdp_domains::tracing::profile_page_performance::ProfilePagePerformanceTool;
use crate::handler::{McpHandler, insights, parse_args};
use rust_mcp_sdk::schema::{CallToolError, CallToolRequestParams, CallToolResult};
use serde_json::{Value, json};

type ToolResult = Result<CallToolResult, CallToolError>;

/// What Firefox's `get_performance_metrics` cannot report.
pub(crate) const METRICS_NOTE: &str = "Firefox gives only Nodes, Frames, Resources and TransferSize (bytes), read from the page with the Web Performance APIs; JSHeapUsedSize, JSHeapTotalSize, LayoutCount, RecalcStyleCount, ScriptDuration, TaskDuration and the other Chrome metrics need CDP's Performance domain";

/// What Firefox's `profile_page_performance` cannot report.
pub(crate) const LONG_TASKS_NOTE: &str =
    "Firefox has no Long Tasks API: total_blocking_time_ms and top_long_tasks are null";

/// `cacheBehavior` for the tab (`bypass` or `default`); Firefox has no
/// `ignoreCache` argument on `browsingContext.reload`.
pub(crate) async fn set_cache_behavior(
    tab: &bidi_browser_lite::Tab,
    behavior: &str,
) -> Result<(), BackendError> {
    page::command(
        tab,
        "network.setCacheBehavior",
        json!({"cacheBehavior": behavior, "contexts": [tab.context_id()]}),
    )
    .await?;
    Ok(())
}

/// Why Firefox refuses `save_path`.
const SAVE_UNSUPPORTED: &str = "save_path is only available on Chrome: Firefox has no tracing over WebDriver BiDi, so there is no trace to save; nothing was profiled";

/// Why Firefox has no `forced_reflows`.
pub(crate) const NO_TRACE: &str = "Firefox has no trace over WebDriver BiDi, which is where Chrome records the layouts a script forced";

/// Why Firefox has no `unused_code`.
pub(crate) const NO_COVERAGE: &str =
    "Firefox has no code coverage over WebDriver BiDi (Chrome's Profiler and CSS domains give it)";

/// `get_performance_metrics`: the metrics the page can read, plus a note.
pub(crate) async fn metrics(params: CallToolRequestParams, handler: &McpHandler) -> ToolResult {
    let args: GetPerformanceMetricsTool = parse_args(params)?;
    let session = handler.firefox(args.instance_id.as_deref()).await?;
    let (_, tab) = session.tab(args.tab_id.as_deref()).await?;
    let metrics = page::run_snippet(&tab.tab, crate::util::js::PAGE_METRICS).await?;
    let body = serde_json::to_string_pretty(&metrics).unwrap_or_default();
    let collected = args.collects_garbage().then_some(NOT_COLLECTED_NOTE);
    let blocks = [body.as_str(), METRICS_NOTE].into_iter().chain(collected);
    Ok(CallToolResult::text_content(
        blocks.map(|text| text.to_string().into()).collect(),
    ))
}

/// `profile_page_performance`: reloads if asked (bypassing the cache when
/// `disable_cache`), lets the page run `duration_ms`, then reads the
/// page-load vitals. Same keys as on Chrome.
pub(crate) async fn profile(params: CallToolRequestParams, handler: &McpHandler) -> ToolResult {
    let args: ProfilePagePerformanceTool = parse_args(params)?;
    if args.save_path.is_some() {
        return Err(CallToolError::from_message(SAVE_UNSUPPORTED));
    }
    let session = handler.firefox(args.instance_id.as_deref()).await?;
    let (_, tab) = session.tab(args.tab_id.as_deref()).await?;
    let duration = args.duration_ms.unwrap_or(3000).clamp(500, 15000);
    // Firefox rejects `ignoreCache` on browsingContext.reload: bypass the
    // cache with network.setCacheBehavior, restored on every exit path.
    let bypass = args.disable_cache.unwrap_or(false);
    if bypass {
        set_cache_behavior(&tab.tab, "bypass").await?;
    }
    let profiled = async {
        if args.reloads() {
            page::reload(&tab.tab).await?;
        }
        tokio::time::sleep(std::time::Duration::from_millis(duration)).await;
        Ok::<(), BackendError>(())
    }
    .await;
    if bypass {
        set_cache_behavior(&tab.tab, "default").await?;
    }
    profiled?;
    let vitals = page::run_snippet(&tab.tab, crate::util::js::PAGE_VITALS).await?;
    let mut summary = summary(&vitals, duration);
    if args.insights.unwrap_or(false) {
        let found = page::run_snippet(&tab.tab, insights::SNIPPET).await?;
        let mut annotated = insights::annotate(found);
        insights::add_unused_code(&mut annotated, Err(NO_COVERAGE.into()));
        insights::add_forced_reflows(&mut annotated, Err(NO_TRACE.into()));
        summary["insights"] = annotated;
    }
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

    /// ISSUE-57 (Firefox): `collect_garbage` cannot be done over BiDi:
    /// the reply says so instead of pretending.
    #[tokio::test]
    async fn firefox_cannot_collect_garbage() {
        let mock = MockFirefox::start(false).await.unwrap();
        mock.bidi().respond_with("script.evaluate", |_| {
            evaluate_success(
                json!({"type": "object", "value": [["Nodes", {"type": "number", "value": 12}]]}),
            )
        });
        let handler = ts::handler(&mock);
        let args = json!({"collect_garbage": true});
        let reply = metrics(ts::call("get_performance_metrics", args), &handler)
            .await
            .unwrap();
        let notes: Vec<String> = reply.content[1..]
            .iter()
            .map(|c| c.as_text_content().unwrap().text.clone())
            .collect();
        assert_eq!(notes.len(), 2, "{notes:?}");
        assert!(
            notes[1].starts_with("Garbage was not collected"),
            "{notes:?}"
        );
        ts::stop(&handler).await;
    }

    /// ISSUE-56 (Firefox): `save_path` fails before anything runs: there
    /// is no trace to save.
    #[tokio::test]
    async fn firefox_refuses_to_save_a_trace() {
        let mock = MockFirefox::start(false).await.unwrap();
        let handler = ts::handler(&mock);
        let args = json!({"save_path": "/tmp/trace.json"});
        let error = profile(ts::call("profile_page_performance", args), &handler)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.starts_with("save_path is only available on Chrome"),
            "{error}"
        );
        assert!(mock.bidi().commands_named("script.evaluate").is_empty());
        ts::stop(&handler).await;
    }

    /// A Firefox tab whose reload answers `reload_reply` and whose vitals
    /// snippet answers; profiles a reload with the cache disabled and
    /// returns the mock with the `network.setCacheBehavior` params sent.
    async fn uncached_profile(reload_reply: Value) -> (MockFirefox, Vec<Value>) {
        let mock = MockFirefox::start(false).await.unwrap();
        mock.bidi()
            .respond_with("browsingContext.reload", move |_| reload_reply.clone());
        mock.bidi().respond_with(
            "browsingContext.getTree",
            |_| json!({"contexts": [{"context": "ctx-0", "url": "http://a/", "children": null}]}),
        );
        mock.bidi().respond_with("script.evaluate", |_| {
            evaluate_success(ts::remote(&json!({"dom_content_loaded_ms": 120.5})))
        });
        let handler = ts::handler(&mock);
        let args = json!({"duration_ms": 500, "action": "reload", "disable_cache": true});
        let _ = profile(ts::call("profile_page_performance", args), &handler).await;
        ts::stop(&handler).await;
        let modes = mock
            .bidi()
            .commands_named("network.setCacheBehavior")
            .iter()
            .map(|c| c.params.clone())
            .collect();
        (mock, modes)
    }

    /// ISSUE-58 (Firefox): `disable_cache: true` bypasses the cache with
    /// `network.setCacheBehavior` (Firefox rejects `ignoreCache` on reload)
    /// and restores it afterwards.
    #[tokio::test]
    async fn uncached_profile_bypasses_and_restores_the_cache() {
        // Given a Firefox tab whose reload answers
        let reply = json!({"navigation": "n2", "url": "http://a/"});
        // When it profiles a reload with the cache disabled
        let (mock, modes) = uncached_profile(reply).await;
        // Then the cache was bypassed and restored, never ignoreCache
        assert_eq!(
            modes,
            [
                json!({"cacheBehavior": "bypass", "contexts": ["ctx-0"]}),
                json!({"cacheBehavior": "default", "contexts": ["ctx-0"]})
            ]
        );
        let reload = &mock.bidi().commands_named("browsingContext.reload")[0];
        assert!(reload.params.get("ignoreCache").is_none());
    }

    /// ISSUE-58 (Firefox): the cache is restored even when the reload fails.
    #[tokio::test]
    async fn failed_uncached_reload_still_restores_the_cache() {
        // Given a Firefox tab whose reload always fails
        let reply = json!({"type": "error", "error": "unknown error", "message": "Error: boom"});
        // When it profiles a reload with the cache disabled
        let (_mock, modes) = uncached_profile(reply).await;
        // Then the cache was still restored to its default
        let modes: Vec<&Value> = modes.iter().map(|m| &m["cacheBehavior"]).collect();
        assert_eq!(modes, [&json!("bypass"), &json!("default")]);
    }

    /// ISSUE-22: `insights: true` adds the page's insights, with notes
    /// for what Firefox does not report; without it they are not read.
    #[tokio::test]
    async fn firefox_profile_with_insights() {
        // Given a page whose snippets answer vitals and insights
        let mock = MockFirefox::start(false).await.unwrap();
        mock.bidi().respond_with("script.evaluate", |params| {
            let expression = params["expression"].as_str().unwrap_or_default();
            let value: Value = if expression.contains("long-animation-frame") {
                serde_json::from_str(include_str!(
                    "../../../tests/fixtures/insights/firefox-wikipedia.json"
                ))
                .unwrap()
            } else {
                json!({"dom_content_loaded_ms": 120.5, "lcp_supported": true})
            };
            evaluate_success(ts::remote(&value))
        });
        let handler = ts::handler(&mock);
        // When it profiles with insights
        let args = json!({"duration_ms": 500, "insights": true});
        let reply = profile(ts::call("profile_page_performance", args), &handler)
            .await
            .unwrap();
        // Then the insights come with what Firefox does not report
        let body: Value = serde_json::from_str(&ts::text(&reply)).unwrap();
        assert_eq!(body["insights"]["lcp"]["element"], "img#mwDQ");
        let notes = body["insights"]["notes"].as_array().unwrap();
        assert!(
            notes[0].as_str().unwrap().starts_with("cls is null"),
            "{body}"
        );
        // ISSUE-54 / ISSUE-56: and no coverage or forced reflows, saying why
        assert_eq!(body["insights"]["unused_code"], Value::Null);
        assert_eq!(body["insights"]["forced_reflows"], Value::Null);
        assert!(notes.contains(&json!(format!("unused_code is null: {NO_COVERAGE}"))));
        assert!(notes.contains(&json!(format!("forced_reflows is null: {NO_TRACE}"))));
        // And without insights the snippet is not run
        let args = json!({"duration_ms": 500});
        let reply = profile(ts::call("profile_page_performance", args), &handler)
            .await
            .unwrap();
        let body: Value = serde_json::from_str(&ts::text(&reply)).unwrap();
        assert!(body.get("insights").is_none(), "{body}");
        assert_eq!(mock.bidi().commands_named("script.evaluate").len(), 3);
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
        assert_eq!(reload["wait"].as_str(), Some("complete"));
        let bypass = mock.bidi().commands_named("network.setCacheBehavior");
        assert!(bypass[0].params["cacheBehavior"] == json!("bypass"));
        assert!(bypass[1].params["cacheBehavior"] == json!("default"));
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
