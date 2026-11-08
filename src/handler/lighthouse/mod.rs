//! `lighthouse_report`: a Lighthouse-style report of a cold load, without
//! the Lighthouse bundle. The page is loaded in Lighthouse's conditions (a
//! throttled phone by default, or a desktop), its five performance metrics
//! are read from a Chrome trace and scored with Lighthouse's curves and
//! weights (see [`scoring`]), and the `audit_page` checks are weighted as
//! Lighthouse weighs its matching audits into the accessibility, SEO and
//! best-practices scores. Firefox gets the same report with what it can
//! measure, and says what it cannot.

pub(crate) mod chrome;
pub(crate) mod scoring;
pub(crate) mod speed_index;
pub(crate) mod trace;

use crate::handler::audit::AuditPageTool;
use crate::handler::cdp_domains::log::LogState;
use crate::handler::cdp_domains::page::emulate::NetworkThrottling;
use crate::handler::navigation::NavigationOutcome;
use rust_mcp_sdk::{
    macros,
    schema::{CallToolError, CallToolResult},
};
use scoring::{CHECK_WEIGHTS, Metric};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::sync::Mutex;
use trace::LoadMetrics;

#[macros::mcp_tool(
    name = "lighthouse_report",
    description = "Loads a page cold in the tab (from about:blank) and scores it Lighthouse-style: 0-100 for performance, accessibility, SEO and best practices; FCP, LCP, TBT, CLS and Speed Index with their scores; each failing check with the points it costs and its elements (snapshot refs); insights explaining the metrics. For problems without reloading use audit_page; for raw timings, profile_page_performance. mobile: 412x823 at 1.75x, touch, CPU 4x slower, Slow 4G; desktop: 1350x940, unthrottled. Cache bypassed, user agent kept, the tab's emulation put back. Approximate: throttling is applied (Lighthouse simulates it), TBT runs to the trace's end, and the other categories use audit_page's checks (fewer than Lighthouse's). Firefox: partial, no trace or throttling (FCP and LCP only, no performance score; the other three categories are scored)."
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct LighthouseReportTool {
    /// Default: the default instance.
    pub instance_id: Option<String>,
    /// Default: the active tab.
    pub tab_id: Option<String>,
    /// Absolute URL to load. Default: the tab's current page (an http or file page, else the call fails asking for 'url').
    pub url: Option<String>,
    /// 'mobile' (default) or 'desktop'.
    pub device: Option<Device>,
    /// Categories to report. Default: all four.
    pub categories: Option<Vec<ReportCategory>>,
}

/// The device Lighthouse emulates. Each variant is renamed on its own: the
/// schema derive ignores `rename_all` (ISSUE-82).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema,
)]
pub enum Device {
    /// A mid-range phone on a slow 4G network (Lighthouse's default).
    #[serde(rename = "mobile")]
    Mobile,
    /// A desktop without throttling.
    #[serde(rename = "desktop")]
    Desktop,
}

/// A category of the report.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema,
)]
pub enum ReportCategory {
    /// The five load metrics.
    #[serde(rename = "performance")]
    Performance,
    /// Names, language, contrast, focus, target size.
    #[serde(rename = "accessibility")]
    Accessibility,
    /// Title, description, links, indexing, status code.
    #[serde(rename = "seo")]
    Seo,
    /// HTTPS, doctype, console errors, image aspect ratio.
    #[serde(rename = "best_practices")]
    BestPractices,
}

impl ReportCategory {
    /// Every category, in the order of the reply.
    const ALL: [Self; 4] = [
        Self::Performance,
        Self::Accessibility,
        Self::Seo,
        Self::BestPractices,
    ];

    /// Its key in the reply (and the `audit_page` category of its checks).
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Performance => "performance",
            Self::Accessibility => "accessibility",
            Self::Seo => "seo",
            Self::BestPractices => "best_practices",
        }
    }
}

/// The conditions a device is emulated in.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Conditions {
    /// The viewport (CSS pixels).
    pub(crate) width: u32,
    pub(crate) height: u32,
    /// The device pixel ratio.
    pub(crate) scale: f64,
    /// A phone: mobile viewport and touch.
    pub(crate) mobile: bool,
    /// How many times slower the CPU runs.
    pub(crate) cpu_slowdown: f64,
    /// The network throttling.
    pub(crate) network: NetworkThrottling,
}

impl Device {
    /// Its name in the reply.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Mobile => "mobile",
            Self::Desktop => "desktop",
        }
    }

    /// Lighthouse's conditions for it (its `moto g power` screen with
    /// DevTools' Slow 4G throttling, or its desktop screen).
    pub(crate) fn conditions(self) -> Conditions {
        match self {
            Self::Mobile => Conditions {
                width: 412,
                height: 823,
                scale: 1.75,
                mobile: true,
                cpu_slowdown: 4.0,
                network: NetworkThrottling::Fast3g,
            },
            Self::Desktop => Conditions {
                width: 1350,
                height: 940,
                scale: 1.0,
                mobile: false,
                cpu_slowdown: 1.0,
                network: NetworkThrottling::None,
            },
        }
    }
}

/// What a load found, from either browser, for [`LighthouseReportTool::report`].
#[derive(Debug, Default)]
pub(crate) struct Findings {
    /// The URL the load ended on.
    pub(crate) url: String,
    /// The load metrics (`None` when performance was not asked for).
    pub(crate) metrics: Option<LoadMetrics>,
    /// The `audit_page` checks, the console's included.
    pub(crate) checks: Vec<Value>,
    /// What explains the metrics (`None` when performance was not asked).
    pub(crate) insights: Option<Value>,
    /// What the agent should know about this report.
    pub(crate) notes: Vec<String>,
}

/// The current time, in ms since the epoch (console messages carry it):
/// errors count from the start of the load.
pub(crate) fn now_ms() -> f64 {
    let since = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH);
    since.map(|d| d.as_secs_f64() * 1000.0).unwrap_or_default()
}

/// Whether a check found something.
fn failing(check: &Value) -> bool {
    check["count"].as_u64().unwrap_or(0) > 0
}

/// Whether a check had something to look at.
fn applies(check: &Value) -> bool {
    check["checked"].as_u64().unwrap_or(0) > 0
}

/// The `http-status-code` check (SEO): an error status keeps the page out
/// of search results.
pub(crate) fn status_check(status: Option<i64>) -> Value {
    let failed: Vec<Value> = status
        .filter(|s| *s >= 400)
        .map(|s| json!({"detail": format!("HTTP status {s}")}))
        .into_iter()
        .collect();
    json!({
        "category": "seo",
        "id": "http-status-code",
        "title": "The page answered with an HTTP error status",
        "checked": u64::from(status.is_some()),
        "count": failed.len(),
        "elements": failed,
    })
}

/// A category's weighted checks: `(id, weight, check)` for those that
/// applied.
fn weighted<'a>(category: &str, checks: &'a [Value]) -> Vec<(&'a str, f64, &'a Value)> {
    CHECK_WEIGHTS
        .iter()
        .filter(|(c, _, _)| *c == category)
        .filter_map(|(_, id, weight)| {
            let check = checks.iter().find(|c| c["id"] == *id)?;
            applies(check).then_some((*id, *weight, check))
        })
        .collect()
}

/// The points each failing check costs, by check id, then by category.
type Losses = Vec<(String, serde_json::Map<String, Value>)>;

/// Adds to `lost` what the failing checks of `own` (a category's weighted
/// checks) cost `category`: each one's share of its weight.
fn add_losses(lost: &mut Losses, category: &str, own: &[(&str, f64, &Value)]) {
    let total: f64 = own.iter().map(|(_, weight, _)| weight).sum();
    for (id, weight, _) in own.iter().filter(|(_, _, c)| failing(c)) {
        let cost = json!(points(weight / total));
        match lost.iter_mut().find(|(known, _)| known == id) {
            Some((_, by)) => {
                by.insert(category.into(), cost);
            }
            None => {
                let by = serde_json::Map::from_iter([(category.to_string(), cost)]);
                lost.push((id.to_string(), by));
            }
        }
    }
}

/// The failing checks of `lost`, each with its `points_lost`.
fn failed_checks(checks: &[Value], lost: Losses) -> Vec<Value> {
    lost.into_iter()
        .filter_map(|(id, by)| {
            let mut check = checks.iter().find(|c| c["id"] == id.as_str())?.clone();
            let fields = check.as_object_mut()?;
            fields.remove("checked");
            fields.insert("points_lost".into(), Value::Object(by));
            Some(check)
        })
        .collect()
}

/// The most points a failing check costs in any category.
fn largest_loss(check: &Value) -> f64 {
    let lost = check["points_lost"].as_object();
    lost.into_iter()
        .flat_map(|by| by.values())
        .filter_map(Value::as_f64)
        .fold(0.0, f64::max)
}

/// Points out of 100, to one decimal.
fn points(share: f64) -> f64 {
    (share * 1000.0).round() / 10.0
}

impl LighthouseReportTool {
    /// The categories asked for, in reply order (all by default).
    pub(crate) fn wanted(&self) -> Vec<ReportCategory> {
        match &self.categories {
            Some(asked) if !asked.is_empty() => ReportCategory::ALL
                .into_iter()
                .filter(|c| asked.contains(c))
                .collect(),
            _ => ReportCategory::ALL.to_vec(),
        }
    }

    /// Whether performance is asked for (then the load is traced and
    /// throttled).
    pub(crate) fn wants_performance(&self) -> bool {
        self.wanted().contains(&ReportCategory::Performance)
    }

    /// Whether a category of checks is asked for (then the page runs the
    /// `audit_page` checks).
    pub(crate) fn wants_checks(&self) -> bool {
        self.wanted()
            .iter()
            .any(|c| *c != ReportCategory::Performance)
    }

    /// The `audit_page` checks the page runs, when checks are asked for:
    /// all three categories, since a check can count in two.
    pub(crate) fn audit(&self) -> Option<AuditPageTool> {
        let all = AuditPageTool {
            instance_id: None,
            tab_id: None,
            categories: None,
        };
        self.wants_checks().then_some(all)
    }

    /// The findings of a load that ended with `outcome`, from what the
    /// [`Self::audit`] script found in the page (`None`: not run) and the
    /// console errors logged from `since` (ms since the epoch).
    pub(crate) async fn findings(
        &self,
        outcome: NavigationOutcome,
        page: Option<Value>,
        (log, since): (&Arc<Mutex<LogState>>, f64),
    ) -> Findings {
        let mut found = Findings {
            url: outcome.url,
            notes: outcome.notes,
            ..Default::default()
        };
        if let (Some(audit), Some(page)) = (self.audit(), page) {
            found.checks = audit.checks(&page, log, Some(since)).await;
            found.checks.push(status_check(outcome.status));
            let notes = page["notes"].as_array().into_iter().flatten();
            found
                .notes
                .extend(notes.filter_map(Value::as_str).map(str::to_string));
        }
        found
    }

    /// The device asked for (mobile by default, as Lighthouse).
    pub(crate) fn device(&self) -> Device {
        self.device.unwrap_or(Device::Mobile)
    }

    /// The URL given, checked as `navigate` checks it (and against the
    /// server's `--local` restriction), before anything runs.
    pub(crate) fn checked_url(&self, local_only: bool) -> Result<Option<&str>, CallToolError> {
        let Some(url) = self.url.as_deref() else {
            return Ok(None);
        };
        crate::handler::navigation::checked_url(url).map_err(CallToolError::from_message)?;
        if local_only && !crate::handler::is_local_address(url) {
            return Err(CallToolError::from_message(format!(
                "Loading '{url}' is blocked: the server runs with the 'local' argument, which allows local addresses only."
            )));
        }
        Ok(Some(url))
    }

    /// The conditions of the reply: the device's, with the throttling only
    /// when the load was throttled.
    fn conditions(&self, throttled: bool) -> Value {
        let c = self.device().conditions();
        let (cpu, network) = match throttled {
            true => (c.cpu_slowdown, c.network.as_str()),
            false => (1.0, "none"),
        };
        json!({
            "viewport": format!("{}x{}", c.width, c.height),
            "device_scale_factor": c.scale,
            "mobile": c.mobile,
            "cpu_slowdown": cpu,
            "network": network,
            "cache": "bypassed",
        })
    }

    /// The performance metrics with their scores, and the category's score
    /// (`None` unless all five were measured, with a note naming those
    /// that were not).
    fn performance(&self, metrics: &LoadMetrics, notes: &mut Vec<String>) -> (Value, Option<u32>) {
        let device = self.device();
        let mut body = serde_json::Map::new();
        let mut scored = Vec::new();
        let mut missing = Vec::new();
        for metric in Metric::ALL {
            let Some(value) = metrics.get(metric) else {
                missing.push(metric.key());
                body.insert(
                    metric.key().into(),
                    json!({"value": null, "weight": metric.weight()}),
                );
                continue;
            };
            let score = scoring::metric_score(metric.curve(device), value);
            scored.push((metric.weight(), score));
            let lost = points(metric.weight() / 100.0 * (1.0 - score));
            body.insert(
                metric.key().into(),
                json!({"value": value, "score": score, "weight": metric.weight(), "points_lost": lost}),
            );
        }
        if !missing.is_empty() {
            notes.push(format!(
                "performance has no score: {} could not be measured",
                missing.join(", ")
            ));
            return (Value::Object(body), None);
        }
        (Value::Object(body), scoring::category_score(&scored))
    }

    /// The scores of the check categories asked for, and each failing
    /// check with the points it costs in each of them, costliest first.
    fn check_scores(&self, checks: &[Value]) -> (serde_json::Map<String, Value>, Vec<Value>) {
        let mut scores = serde_json::Map::new();
        let mut lost = Losses::new();
        let categories = self.wanted().into_iter();
        for category in categories.filter(|c| *c != ReportCategory::Performance) {
            let own = weighted(category.name(), checks);
            let pairs: Vec<(f64, f64)> = own
                .iter()
                .map(|(_, w, c)| (*w, if failing(c) { 0.0 } else { 1.0 }))
                .collect();
            scores.insert(
                category.name().into(),
                json!(scoring::category_score(&pairs)),
            );
            add_losses(&mut lost, category.name(), &own);
        }
        let mut failed = failed_checks(checks, lost);
        failed.sort_by(|a, b| largest_loss(b).total_cmp(&largest_loss(a)));
        (scores, failed)
    }

    /// The ids of the checks of the categories asked for that passed, and
    /// of those that did not apply.
    fn other_checks(&self, checks: &[Value]) -> (Vec<Value>, Vec<Value>) {
        let wanted = self.wanted();
        let mut ids: Vec<&str> = Vec::new();
        let asked = CHECK_WEIGHTS
            .iter()
            .filter(|(c, _, _)| wanted.iter().any(|w| w.name() == *c));
        for (_, id, _) in asked {
            if !ids.contains(id) {
                ids.push(id);
            }
        }
        let (mut passed, mut not_applicable) = (Vec::new(), Vec::new());
        for id in ids {
            let Some(check) = checks.iter().find(|c| c["id"] == id) else {
                continue;
            };
            match (applies(check), failing(check)) {
                (true, false) => passed.push(json!(id)),
                (false, _) => not_applicable.push(json!(id)),
                _ => {}
            }
        }
        (passed, not_applicable)
    }

    /// The reply (both browsers): a line with the scores, then the JSON.
    pub(crate) fn report(&self, browser: &str, mut found: Findings) -> CallToolResult {
        let throttled = browser == "Chrome" && self.wants_performance();
        let mut scores = serde_json::Map::new();
        let mut metrics = Value::Null;
        if let Some(measured) = &found.metrics {
            let (body, score) = self.performance(measured, &mut found.notes);
            scores.insert("performance".into(), json!(score));
            metrics = body;
        }
        let (checked, failed) = self.check_scores(&found.checks);
        scores.extend(checked);
        let (passed, not_applicable) = self.other_checks(&found.checks);
        let mut first = first_line(
            &found.url,
            self.device(),
            browser,
            (&self.wanted(), &scores),
        );
        if self.wants_checks() {
            first.push_str(" Failing checks carry snapshot refs.");
        }
        let mut body = json!({
            "url": found.url,
            "device": self.device().name(),
            "conditions": self.conditions(throttled),
            "scores": scores,
        });
        if !metrics.is_null() {
            body["metrics"] = metrics;
        }
        body["failed"] = json!(failed);
        body["passed"] = json!(passed);
        body["not_applicable"] = json!(not_applicable);
        if let Some(insights) = found.insights {
            body["insights"] = insights;
        }
        found.notes.push(APPROXIMATE.to_string());
        body["notes"] = json!(found.notes);
        let text = serde_json::to_string_pretty(&body).unwrap_or_default();
        CallToolResult::text_content(vec![format!("{first}\n{text}").into()])
    }
}

/// What every report says about itself.
const APPROXIMATE: &str = "Lighthouse-style, not Lighthouse: its curves and weights on the metrics measured here and on the audit_page checks (fewer than Lighthouse's audits), so scores differ from a Lighthouse run; 'failed' lists what costs points, the costliest first";

/// The reply's first line: the scores in words.
fn first_line(
    url: &str,
    device: Device,
    browser: &str,
    (wanted, scores): (&[ReportCategory], &serde_json::Map<String, Value>),
) -> String {
    let named: Vec<String> = wanted
        .iter()
        .map(|category| {
            let score = &scores[category.name()];
            let name = category.name().replace('_', " ");
            match score.as_u64() {
                Some(score) => format!("{name} {score}"),
                None => format!("{name} n/a"),
            }
        })
        .collect();
    format!(
        "Lighthouse-style report of {url} ({}, {browser}): {}.",
        device.name(),
        named.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(args: Value) -> LighthouseReportTool {
        serde_json::from_value(args).unwrap()
    }

    fn check(category: &str, id: &str, checked: u64, count: u64) -> Value {
        json!({"category": category, "id": id, "title": id, "checked": checked, "count": count, "elements": []})
    }

    /// The metrics of a fast page, all five measured.
    fn fast() -> LoadMetrics {
        let mut metrics = LoadMetrics::default();
        for (metric, value) in Metric::ALL
            .into_iter()
            .zip([900.0, 1200.0, 0.0, 0.0, 1000.0])
        {
            metrics.set(metric, Some(value));
        }
        metrics
    }

    /// LH-REPORT-01: a report scores each category and lists what costs
    /// points, a check that counts in two categories once.
    #[test]
    fn a_report_scores_each_category() {
        // Given a page with a missing title and an unnamed button
        let args = tool(json!({}));
        let checks = vec![
            check("accessibility", "image-alt", 3, 0),
            check("accessibility", "button-name", 2, 1),
            check("seo", "document-title", 1, 1),
            check("seo", "meta-description", 1, 0),
            check("accessibility", "label", 0, 0),
            check("best_practices", "is-on-https", 1, 0),
            status_check(Some(200)),
        ];
        let found = Findings {
            url: "http://a/".into(),
            metrics: Some(fast()),
            checks,
            ..Default::default()
        };
        // When the report is put together
        let reply = args.report("Chrome", found);
        let text = &reply.content[0].as_text_content().unwrap().text;
        let (first, body) = text.split_once('\n').unwrap();
        let body: Value = serde_json::from_str(body).unwrap();
        // Then a11y is 10/(10+10+7) passed, SEO 3/4 (image-alt counts in
        // it too), best practices 100
        assert_eq!(body["scores"]["performance"], 100);
        assert_eq!(body["scores"]["accessibility"], 37);
        assert_eq!(body["scores"]["seo"], 75);
        assert_eq!(body["scores"]["best_practices"], 100);
        assert!(
            first.contains("performance 100, accessibility 37, seo 75, best practices 100"),
            "{first}"
        );
        let failed = body["failed"].as_array().unwrap();
        assert_eq!(failed.len(), 2, "{failed:?}");
        assert_eq!(failed[0]["id"], "button-name");
        assert_eq!(failed[0]["points_lost"]["accessibility"], 37.0);
        assert_eq!(
            failed[1]["points_lost"],
            json!({"accessibility": 25.9, "seo": 25.0})
        );
        assert!(
            body["not_applicable"]
                .as_array()
                .unwrap()
                .contains(&json!("label"))
        );
        assert_eq!(body["metrics"]["total_blocking_time_ms"]["score"], 1.0);
    }

    /// LH-REPORT-02: without all five metrics there is no performance
    /// score, and the note names the missing ones; categories narrow it.
    #[test]
    fn a_missing_metric_leaves_performance_unscored() {
        // Given a load without TBT (Firefox) and only performance asked
        let args = tool(json!({"categories": ["performance"], "device": "desktop"}));
        let mut metrics = fast();
        metrics.set(Metric::TotalBlockingTime, None);
        let found = Findings {
            url: "http://a/".into(),
            metrics: Some(metrics),
            ..Default::default()
        };
        // When the report is put together
        let reply = args.report("Firefox", found);
        let text = &reply.content[0].as_text_content().unwrap().text;
        let body: Value = serde_json::from_str(text.split_once('\n').unwrap().1).unwrap();
        // Then performance is null and only it is scored
        assert_eq!(body["scores"], json!({"performance": null}));
        assert!(
            body["notes"][0]
                .as_str()
                .unwrap()
                .contains("total_blocking_time_ms")
        );
        assert_eq!(body["conditions"]["cpu_slowdown"], 1.0);
        assert_eq!(body["device"], "desktop");
    }

    /// LH-REPORT-03: an error status fails http-status-code; none given
    /// does not apply.
    #[test]
    fn the_status_code_is_a_check() {
        assert_eq!(status_check(Some(404))["count"], 1);
        assert_eq!(status_check(Some(200))["count"], 0);
        assert_eq!(status_check(None)["checked"], 0);
    }

    /// LH-REPORT-04: a URL is checked as navigate checks it, and against
    /// --local.
    #[test]
    fn urls_are_checked_first() {
        let error = |args: Value, local| tool(args).checked_url(local).unwrap_err().to_string();
        assert!(error(json!({"url": "javascript:1"}), false).contains("evaluate_js"));
        assert!(error(json!({"url": "https://example.com/"}), true).contains("local"));
        assert_eq!(tool(json!({})).checked_url(true).unwrap(), None);
    }
}
