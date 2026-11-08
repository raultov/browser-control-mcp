//! Firefox's `lighthouse_report`: the same load and report as on Chrome
//! with what WebDriver BiDi gives. The viewport and the device pixel ratio
//! are emulated and the cache bypassed, but there is no CPU or network
//! throttling, no touch and no trace: FCP and LCP come from the Web
//! Performance APIs, TBT, CLS and Speed Index cannot be measured, so the
//! performance category has no score; the other three are scored.

use super::page;
use super::tools::{ids, target};
use crate::backend::BackendError;
use crate::handler::lighthouse::scoring::Metric;
use crate::handler::lighthouse::trace::LoadMetrics;
use crate::handler::lighthouse::{Findings, LighthouseReportTool};
use crate::handler::{McpHandler, insights, parse_args};
use bidi_browser_lite::Tab;
use rust_mcp_sdk::schema::{CallToolError, CallToolRequestParams, CallToolResult};
use serde_json::{Value, json};

/// What Firefox's report cannot measure.
pub(crate) const NO_TRACE_NOTE: &str = "Firefox: no CPU or network throttling, touch or trace over WebDriver BiDi: the load ran at full speed, FCP and LCP come from the Web Performance APIs, and total_blocking_time_ms, cumulative_layout_shift and speed_index_ms cannot be measured";

/// The page's viewport and device pixel ratio.
const VIEWPORT: &str = "({ width: innerWidth, height: innerHeight, ratio: devicePixelRatio })";

/// The document's HTTP status, from its navigation timing (0 when the
/// browser does not tell).
const STATUS: &str = "(performance.getEntriesByType('navigation')[0] || {}).responseStatus || 0";

/// How long the page runs after `load`.
#[cfg(not(test))]
const SETTLE: std::time::Duration = std::time::Duration::from_secs(2);
#[cfg(test)]
const SETTLE: std::time::Duration = std::time::Duration::from_millis(10);

/// `browsingContext.setViewport` of `tab` (`None`: the window's own).
async fn set_viewport(tab: &Tab, size: Option<(u64, u64, f64)>) -> Result<(), BackendError> {
    let params = match size {
        Some((width, height, ratio)) => json!({"context": tab.context_id(),
            "viewport": {"width": width, "height": height}, "devicePixelRatio": ratio}),
        None => json!({"context": tab.context_id(), "viewport": null, "devicePixelRatio": null}),
    };
    page::command(tab, "browsingContext.setViewport", params).await?;
    Ok(())
}

/// A measured viewport as `(width, height, ratio)`.
fn viewport_of(measured: &Value) -> Option<(u64, u64, f64)> {
    Some((
        measured["width"].as_u64()?,
        measured["height"].as_u64()?,
        measured["ratio"].as_f64()?,
    ))
}

/// Puts the viewport the page had back: the window's own if that is what
/// it was, else the size it had (an emulation BiDi cannot read back).
async fn restore_viewport(tab: &Tab, before: Option<(u64, u64, f64)>) {
    if set_viewport(tab, None).await.is_err() {
        return;
    }
    let now = page::run_snippet(tab, VIEWPORT).await.ok();
    if now.as_ref().and_then(viewport_of) != before && before.is_some() {
        let _ = set_viewport(tab, before).await;
    }
}

/// The load metrics Firefox gives: FCP and LCP.
fn metrics_of(vitals: &Value) -> LoadMetrics {
    let mut metrics = LoadMetrics::default();
    let read = |key: &str| vitals[key].as_f64();
    metrics.set(
        Metric::FirstContentfulPaint,
        read("first_contentful_paint_ms"),
    );
    metrics.set(
        Metric::LargestContentfulPaint,
        read("largest_contentful_paint_ms"),
    );
    metrics
}

/// Loads `url` from `about:blank` in the device's viewport, the cache
/// bypassed, and reads what the report needs.
async fn examine(
    args: &LighthouseReportTool,
    tab: &super::session::FirefoxTab,
    url: &str,
) -> Result<Findings, BackendError> {
    page::navigate(&tab.tab, "about:blank").await?;
    let since = crate::handler::lighthouse::now_ms();
    let mut outcome = page::navigate(&tab.tab, url).await?;
    // BiDi's navigation gives no status: the page's own timing does.
    if outcome.status.is_none() {
        let status = page::run_snippet(&tab.tab, STATUS).await.ok();
        outcome.status = status.and_then(|s| s.as_i64()).filter(|s| *s > 0);
    }
    tokio::time::sleep(SETTLE).await;
    let page = match args.audit() {
        Some(audit) => Some(page::run_snippet(&tab.tab, &audit.snippet()).await?),
        None => None,
    };
    let mut findings = args.findings(outcome, page, (&tab.log, since)).await;
    if args.wants_performance() {
        let vitals = page::run_snippet(&tab.tab, crate::util::js::PAGE_VITALS).await?;
        findings.metrics = Some(metrics_of(&vitals));
        let mut annotated =
            insights::annotate(page::run_snippet(&tab.tab, insights::SNIPPET).await?);
        insights::add_unused_code(&mut annotated, Err(super::performance::NO_COVERAGE.into()));
        insights::add_forced_reflows(&mut annotated, Err(super::performance::NO_TRACE.into()));
        findings.insights = Some(annotated);
        findings.notes.push(NO_TRACE_NOTE.into());
    }
    Ok(findings)
}

/// `lighthouse_report` on Firefox.
pub(crate) async fn report(
    params: CallToolRequestParams,
    handler: &McpHandler,
) -> Result<CallToolResult, CallToolError> {
    let args: LighthouseReportTool = parse_args(params)?;
    let given = args.checked_url(handler.flags.local)?.map(str::to_string);
    let (session, id, tab) = target(handler, &ids(&args.instance_id, &args.tab_id)).await?;
    super::debugger::watch_session_tab(&session, &id, &tab).await;
    let url = match given {
        Some(url) => url,
        None if tab.url.starts_with("http") || tab.url.starts_with("file:") => tab.url.clone(),
        None => {
            return Err(CallToolError::from_message(
                "The tab shows no web page to report on: give 'url'.",
            ));
        }
    };
    let before = viewport_of(&page::run_snippet(&tab.tab, VIEWPORT).await?);
    let c = args.device().conditions();
    set_viewport(&tab.tab, Some((c.width.into(), c.height.into(), c.scale))).await?;
    super::performance::set_cache_behavior(&tab.tab, "bypass").await?;
    let found = examine(&args, &tab, &url).await;
    let _ = super::performance::set_cache_behavior(&tab.tab, "default").await;
    restore_viewport(&tab.tab, before).await;
    Ok(args.report("Firefox", found?))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// LH-FF-01: Firefox's report gives FCP and LCP only.
    #[test]
    fn firefox_gives_the_paints() {
        let vitals =
            json!({"first_contentful_paint_ms": 120.5, "largest_contentful_paint_ms": null});
        let metrics = metrics_of(&vitals);
        assert_eq!(metrics.get(Metric::FirstContentfulPaint), Some(120.5));
        assert_eq!(metrics.get(Metric::LargestContentfulPaint), None);
        assert_eq!(metrics.get(Metric::TotalBlockingTime), None);
        assert_eq!(
            viewport_of(&json!({"width": 412, "height": 823, "ratio": 1.75})),
            Some((412, 823, 1.75))
        );
    }
}
