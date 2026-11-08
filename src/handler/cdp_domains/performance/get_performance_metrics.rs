use crate::handler::McpHandler;
use crate::handler::cdp_domains::cdp_target::CdpTarget;
use rust_mcp_sdk::{
    macros,
    schema::{CallToolError, CallToolRequestParams, CallToolResult},
};
use std::collections::BTreeMap;

#[macros::mcp_tool(
    name = "get_performance_metrics",
    description = "Returns runtime counters (JSHeapUsedSize, Nodes, Documents, JSEventListeners, LayoutCount...); with collect_garbage, reads repeated after an action show what it leaked. For load timings use profile_page_performance. Firefox: partial, only Nodes, Frames, Resources and TransferSize, no garbage collection (the reply says what is missing)."
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct GetPerformanceMetricsTool {
    /// Default: the default instance.
    pub instance_id: Option<String>,
    /// Default: the active tab.
    pub tab_id: Option<String>,
    /// Collect garbage before reading (Chrome), so the heap and node counts hold only what is still reachable. Default: false.
    pub collect_garbage: Option<bool>,
}

/// The note of a reply whose metrics were read after a garbage collection.
const COLLECTED_NOTE: &str = "Garbage was collected first (HeapProfiler.collectGarbage): JSHeapUsedSize, Nodes and JSEventListeners count what is still reachable";

/// What Firefox answers to `collect_garbage`.
pub(crate) const NOT_COLLECTED_NOTE: &str = "Garbage was not collected: WebDriver BiDi cannot force a garbage collection, so the counts include objects not yet collected";

impl GetPerformanceMetricsTool {
    /// Whether garbage is to be collected first.
    pub(crate) fn collects_garbage(&self) -> bool {
        self.collect_garbage == Some(true)
    }
}

/// `Memory.getDOMCounters` (live documents, nodes and event listeners)
/// under the names `Performance.getMetrics` uses; nothing if Chrome does
/// not answer it.
async fn dom_counters(target: &CdpTarget) -> Vec<(String, f64)> {
    let Ok(reply) = target
        .send_raw_command("Memory.getDOMCounters", serde_json::json!({}))
        .await
    else {
        return Vec::new();
    };
    let counters = reply.result.unwrap_or_default();
    [
        ("documents", "Documents"),
        ("nodes", "Nodes"),
        ("jsEventListeners", "JSEventListeners"),
    ]
    .into_iter()
    .filter_map(|(key, name)| Some((name.to_string(), counters.get(key)?.as_f64()?)))
    .collect()
}

/// `Performance.getMetrics` as name → value.
async fn read_metrics(target: &CdpTarget) -> Result<BTreeMap<String, f64>, CallToolError> {
    let reply = target
        .send_raw_command("Performance.getMetrics", serde_json::json!({}))
        .await
        .map_err(|e| CallToolError::from_message(crate::handler::cdp_failure("CDP Error", &e)))?;
    let metrics = reply.result.unwrap_or_default();
    Ok(metrics
        .get("metrics")
        .and_then(|m| m.as_array())
        .into_iter()
        .flatten()
        .filter_map(|metric| {
            let name = metric.get("name").and_then(|n| n.as_str())?;
            let value = metric.get("value").and_then(|v| v.as_f64())?;
            Some((name.to_string(), value))
        })
        .collect())
}

impl GetPerformanceMetricsTool {
    pub async fn handle(
        params: CallToolRequestParams,
        handler: &McpHandler,
    ) -> Result<CallToolResult, CallToolError> {
        let args: GetPerformanceMetricsTool = crate::handler::parse_args(params)?;
        let session = handler.session(args.instance_id.clone()).await?;
        let target = session.target(args.tab_id.clone()).await?;
        if args.collects_garbage() {
            let method = "HeapProfiler.collectGarbage";
            crate::handler::send_or(&target, method, serde_json::json!({}), "CDP Error").await?;
        }
        let mut metrics = read_metrics(&target).await?;
        // Chrome answers an empty list until the domain is enabled on this
        // session; tabs enable it when registered, so this only runs for a
        // session that missed it.
        if metrics.is_empty() {
            let _ = target
                .send_raw_command("Performance.enable", serde_json::json!({}))
                .await;
            metrics = read_metrics(&target).await?;
        }
        if metrics.is_empty() {
            return Err(CallToolError::from_message(
                "Chrome returned no metrics for this tab (Performance.getMetrics answered an empty list even after Performance.enable)",
            ));
        }
        metrics.extend(dom_counters(&target).await);
        let body = serde_json::to_string_pretty(&metrics).unwrap_or_default();
        let note = args.collects_garbage().then_some(COLLECTED_NOTE);
        let blocks = std::iter::once(body).chain(note.map(str::to_string));
        Ok(CallToolResult::text_content(
            blocks.map(Into::into).collect(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::cdp_domains::tests::{
        call, mock_handler, reply_text, spawn_recording_mock,
    };
    use cdp_browser_lite::test_support::mock_devtools::MockDevTools;
    use serde_json::{Value, json};

    /// Chrome's behaviour: no metrics on a session until it enabled the domain.
    fn metrics_after_enable(mock: &'static MockDevTools) {
        mock.respond_with("Performance.getMetrics", move |_| {
            let enabled = !mock.commands_named("Performance.enable").is_empty();
            let metrics = if enabled {
                json!([{"name": "JSHeapUsedSize", "value": 1024.0}, {"name": "Nodes", "value": 12.0}])
            } else {
                json!([])
            };
            json!({"metrics": metrics})
        });
    }

    /// Registers the mock's page target as a tab; with `listen`, the way the
    /// tab lifecycle does it (listeners, then domains).
    async fn tab(handler: &McpHandler, mock: &MockDevTools, listen: bool) -> String {
        let session = handler.session(None).await.unwrap();
        let address = format!("127.0.0.1:{}", mock.http_port);
        let browser =
            cdp_browser_lite::BrowserClient::connect(&address, std::time::Duration::from_secs(5))
                .await
                .unwrap();
        let tab = browser.attach("T-page-1").await.unwrap();
        let info = crate::handler::chrome_instance::tab_registry::NewTab::new(None, "about:blank");
        if listen {
            crate::handler::chrome_instance::tab_lifecycle::register_and_listen(
                &session.tabs,
                &tab,
                info,
                false,
            )
            .await
            .unwrap()
        } else {
            crate::util::sync::write(&session.tabs)
                .register_tab(tab, info, false)
                .unwrap()
        }
    }

    fn enables_on(mock: &MockDevTools, session: Option<&str>) -> usize {
        mock.commands_named("Performance.enable")
            .iter()
            .filter(|c| c.session_id.as_deref() == session)
            .count()
    }

    /// PRF-01: metrics are present after the tool alone, even on a tab
    /// whose session never enabled the Performance domain.
    #[tokio::test]
    async fn metrics_present_after_the_tool_alone() {
        // Given a tab registered without its domains enabled
        let mock = spawn_recording_mock().await;
        metrics_after_enable(mock);
        let handler = mock_handler(mock.http_port);
        let tab_id = tab(&handler, mock, false).await;
        // When get_performance_metrics runs on it
        let args = json!({"tab_id": tab_id});
        let reply =
            GetPerformanceMetricsTool::handle(call("get_performance_metrics", args), &handler)
                .await
                .unwrap();
        // Then the metrics are reported
        let metrics: Value = serde_json::from_str(&reply_text(&reply)).unwrap();
        assert_eq!(metrics["JSHeapUsedSize"], 1024.0);
    }

    /// PRF-02: Performance.enable is sent once per tab session.
    #[tokio::test]
    async fn performance_enabled_once_per_tab() {
        // Given a tab registered by the tab lifecycle
        let mock = spawn_recording_mock().await;
        metrics_after_enable(mock);
        let handler = mock_handler(mock.http_port);
        let tab_id = tab(&handler, mock, true).await;
        // When get_performance_metrics runs twice
        for _ in 0..2 {
            let args = json!({"tab_id": tab_id});
            GetPerformanceMetricsTool::handle(call("get_performance_metrics", args), &handler)
                .await
                .unwrap();
        }
        // Then the tab's session enabled the domain exactly once
        let session = mock.commands_named("Performance.getMetrics")[0]
            .session_id
            .clone();
        assert!(session.is_some());
        assert_eq!(enables_on(mock, session.as_deref()), 1);
        // And it was enabled when the tab registered, before the first read
        let on_tab: Vec<String> = mock
            .commands()
            .into_iter()
            .filter(|c| c.session_id == session && c.method.starts_with("Performance."))
            .map(|c| c.method)
            .collect();
        assert_eq!(on_tab[0], "Performance.enable");
    }

    /// ISSUE-57: `collect_garbage` collects before the metrics are read,
    /// and the DOM counters (documents, nodes, event listeners) join them.
    #[tokio::test]
    async fn garbage_is_collected_before_reading() {
        // Given a tab whose DOM counters Chrome reports
        let mock = spawn_recording_mock().await;
        metrics_after_enable(mock);
        mock.respond_with(
            "Memory.getDOMCounters",
            |_| json!({"documents": 2, "nodes": 50, "jsEventListeners": 7}),
        );
        let handler = mock_handler(mock.http_port);
        let tab_id = tab(&handler, mock, true).await;
        // When get_performance_metrics collects garbage first
        let args = json!({"tab_id": tab_id, "collect_garbage": true});
        let reply =
            GetPerformanceMetricsTool::handle(call("get_performance_metrics", args), &handler)
                .await
                .unwrap();
        // Then the collection came before the reads, and the counters are there
        let order: Vec<String> = mock
            .commands()
            .into_iter()
            .map(|c| c.method)
            .filter(|m| {
                m == "HeapProfiler.collectGarbage"
                    || m.ends_with("getMetrics")
                    || m.ends_with("getDOMCounters")
            })
            .collect();
        assert_eq!(
            order,
            [
                "HeapProfiler.collectGarbage",
                "Performance.getMetrics",
                "Memory.getDOMCounters"
            ]
        );
        let metrics: Value = serde_json::from_str(&reply_text(&reply)).unwrap();
        assert_eq!(
            (
                metrics["Documents"].clone(),
                metrics["Nodes"].clone(),
                metrics["JSEventListeners"].clone()
            ),
            (json!(2.0), json!(50.0), json!(7.0))
        );
        assert_eq!(metrics["JSHeapUsedSize"], 1024.0);
        let note = reply.content[1].as_text_content().unwrap().text.clone();
        assert!(note.starts_with("Garbage was collected first"), "{note}");
        // And without it nothing is collected
        let args = json!({"tab_id": tab_id});
        let reply =
            GetPerformanceMetricsTool::handle(call("get_performance_metrics", args), &handler)
                .await
                .unwrap();
        assert_eq!(mock.commands_named("HeapProfiler.collectGarbage").len(), 1);
        assert_eq!(reply.content.len(), 1);
    }

    /// PRF-03: an empty metric list is an error, never `{}`.
    #[tokio::test]
    async fn empty_metric_list_is_an_error() {
        // Given a Chrome that never reports metrics
        let mock = spawn_recording_mock().await;
        mock.respond_with("Performance.getMetrics", |_| json!({"metrics": []}));
        let handler = mock_handler(mock.http_port);
        let tab_id = tab(&handler, mock, false).await;
        // When get_performance_metrics runs
        let args = json!({"tab_id": tab_id});
        let error =
            GetPerformanceMetricsTool::handle(call("get_performance_metrics", args), &handler)
                .await
                .unwrap_err();
        // Then it fails saying so
        assert!(error.to_string().contains("no metrics"), "{error}");
    }
}
