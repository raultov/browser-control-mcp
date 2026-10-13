use crate::handler::McpHandler;
use crate::handler::cdp_domains::cdp_target::CdpTarget;
use rust_mcp_sdk::{
    macros,
    schema::{CallToolError, CallToolRequestParams, CallToolResult},
};
use std::collections::HashMap;

#[macros::mcp_tool(
    name = "get_performance_metrics",
    description = "Captures runtime performance metrics including JS heap size, DOM node count, and layout timing. Side effects: none (read-only snapshot). Prerequisites: requires an active Chrome tab. Returns: JSON object mapping metric names to numeric values (e.g., JSHeapUsedSize, LayoutCount); an error, never an empty object, when Chrome reports no metrics. Use this to monitor memory usage, detect memory leaks, or profile performance. Alternatives: 'profile_page_performance' for detailed tracing, browser DevTools Performance tab. On Firefox: partial, only Nodes, Frames, Resources and TransferSize read from the page with the Web Performance APIs (a second block says what is missing)."
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct GetPerformanceMetricsTool {
    /// Chrome instance id from open_instance/list_instances. Omit for the default instance.
    pub instance_id: Option<String>,
    /// The Tab ID of the target tab. Omit to use the active tab.
    pub tab_id: Option<String>,
}

/// `Performance.getMetrics` as name → value.
async fn read_metrics(target: &CdpTarget) -> Result<HashMap<String, f64>, CallToolError> {
    let reply = target
        .send_raw_command("Performance.getMetrics", serde_json::json!({}))
        .await
        .map_err(|e| CallToolError::from_message(format!("CDP Error: {e:?}")))?;
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
        Ok(CallToolResult::text_content(vec![
            serde_json::to_string_pretty(&metrics)
                .unwrap_or_default()
                .into(),
        ]))
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
