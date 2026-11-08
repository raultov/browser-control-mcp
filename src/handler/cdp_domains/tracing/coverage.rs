//! `profile_page_performance`'s `unused_code` on Chrome: how much of each
//! script never ran, and of each stylesheet never matched, during the
//! profiled reload (CDP's `Profiler` precise coverage and `CSS` rule usage,
//! the data behind Lighthouse's "Reduce unused JavaScript / CSS"). Only a
//! reload is measured: code that ran before the recording started would
//! count as unused.

use crate::handler::cdp_domains::cdp_target::CdpTarget;
use crate::handler::cdp_domains::event_pump::{ListenerHandles, spawn_domain_listener};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Why there is no `unused_code` without a reload.
pub(crate) const NEEDS_RELOAD: &str =
    "it needs action: 'reload' (coverage counts the code run from the start of a load)";

/// Most resources `top_unused` lists.
const TOP: usize = 5;

/// Largest script whose coverage is computed (bigger ones are skipped).
const MAX_SCRIPT_BYTES: u64 = 20_000_000;

/// A stylesheet as `CSS.styleSheetAdded` announced it.
#[derive(Clone, Debug)]
struct Sheet {
    url: String,
    length: u64,
    inline: bool,
}

/// Stylesheets by id.
type Sheets = Arc<Mutex<HashMap<String, Sheet>>>;

/// Coverage being recorded on a tab: the stylesheets announced so far, and
/// the listener collecting them (aborted when this is dropped).
pub(crate) struct Recording {
    sheets: Sheets,
    _listener: ListenerHandles,
}

/// The commands that start the recording, in order.
const START: [(&str, &str); 5] = [
    ("Profiler.enable", "{}"),
    (
        "Profiler.startPreciseCoverage",
        r#"{"callCount": false, "detailed": true}"#,
    ),
    ("DOM.enable", "{}"),
    ("CSS.enable", "{}"),
    ("CSS.startRuleUsageTracking", "{}"),
];

/// The commands that leave the tab as it was, sent on every path.
const STOP: [&str; 4] = [
    "Profiler.stopPreciseCoverage",
    "Profiler.disable",
    "CSS.disable",
    "DOM.disable",
];

/// Starts recording coverage on `target` (before the reload); on a failure
/// the domains are left as they were and the error says why.
pub(crate) async fn start(target: &CdpTarget) -> Result<Recording, String> {
    let sheets: Sheets = Arc::default();
    let collected = sheets.clone();
    let mut listener = ListenerHandles::default();
    // Subscribed before CSS.enable, which announces the sheets already loaded.
    listener.push(spawn_domain_listener(target, "CSS", move |event| {
        let sheets = collected.clone();
        async move {
            if event.method.as_deref() != Some("CSS.styleSheetAdded") {
                return;
            }
            let header = event.params.as_ref().map_or(&Value::Null, |p| &p["header"]);
            let Some(id) = header["styleSheetId"].as_str() else {
                return;
            };
            let sheet = Sheet {
                url: header["sourceURL"].as_str().unwrap_or_default().to_string(),
                length: header["length"].as_f64().unwrap_or(0.0) as u64,
                inline: header["isInline"].as_bool().unwrap_or(false),
            };
            sheets.lock().await.insert(id.to_string(), sheet);
        }
    }));
    for (method, params) in START {
        let params: Value = serde_json::from_str(params).unwrap_or_default();
        if let Err(error) = target.send_raw_command(method, params).await {
            stop(target).await;
            return Err(format!("Chrome refused {method}: {error}"));
        }
    }
    Ok(Recording {
        sheets,
        _listener: listener,
    })
}

/// Sends [`STOP`], ignoring failures (each undoes one start command).
async fn stop(target: &CdpTarget) {
    for method in STOP {
        let _ = target.send_raw_command(method, json!({})).await;
    }
}

impl Recording {
    /// Ends the recording without reading it, on a page a dialog or a
    /// pause blocks: the stop commands are queued, and Chrome runs them once
    /// the page answers again (sent and awaited, each would wait for the
    /// command timeout).
    pub(crate) fn abandon(self, target: &CdpTarget) {
        target.queue_raw_command("CSS.stopRuleUsageTracking", json!({}));
        for method in STOP {
            target.queue_raw_command(method, json!({}));
        }
    }

    /// Ends the recording and returns `unused_code`: `{unused_js_bytes,
    /// unused_css_bytes, top_unused: [{url, kind, total_bytes,
    /// unused_bytes, unused_percent}]}`. The domains are disabled on every
    /// path.
    pub(crate) async fn finish(self, target: &CdpTarget) -> Result<Value, String> {
        let scripts = target
            .send_raw_command("Profiler.takePreciseCoverage", json!({}))
            .await;
        let rules = target
            .send_raw_command("CSS.stopRuleUsageTracking", json!({}))
            .await;
        stop(target).await;
        let page = target
            .send_raw_command(
                "Runtime.evaluate",
                json!({"expression": "location.href", "returnByValue": true}),
            )
            .await
            .ok()
            .and_then(|r| r.result)
            .and_then(|r| r["result"]["value"].as_str().map(str::to_string))
            .unwrap_or_default();
        let scripts = scripts.map_err(|e| format!("Profiler.takePreciseCoverage failed: {e}"))?;
        let rules = rules.map_err(|e| format!("CSS.stopRuleUsageTracking failed: {e}"))?;
        let sheets = self.sheets.lock().await.clone();
        let mut rows = unused_js(&scripts.result.unwrap_or_default()["result"], &page);
        rows.extend(unused_css(
            &rules.result.unwrap_or_default()["ruleUsage"],
            &sheets,
        ));
        Ok(summary(rows))
    }
}

/// One resource's coverage.
#[derive(Debug, PartialEq)]
struct Unused {
    url: String,
    kind: &'static str,
    total: u64,
    unused: u64,
}

/// The total and unused bytes of a script from V8's block coverage of its
/// functions: a byte is unused when the innermost range holding it never
/// ran (ranges nest, so outer ones are applied first).
fn script_bytes(functions: &[Value]) -> Option<(u64, u64)> {
    let mut ranges: Vec<(u64, u64, bool)> = functions
        .iter()
        .flat_map(|f| f["ranges"].as_array().cloned().unwrap_or_default())
        .filter_map(|r| {
            let start = r["startOffset"].as_u64()?;
            let end = r["endOffset"].as_u64()?;
            Some((start, end, r["count"].as_u64().unwrap_or(0) > 0))
        })
        .collect();
    let total = ranges.iter().map(|r| r.1).max()?;
    if total == 0 || total > MAX_SCRIPT_BYTES {
        return None;
    }
    ranges.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
    let mut ran = vec![false; usize::try_from(total).ok()?];
    for (start, end, count) in ranges {
        let size = ran.len();
        let (start, end) = (usize::try_from(start).ok()?, usize::try_from(end).ok()?);
        if let Some(bytes) = ran.get_mut(start..end.min(size)) {
            bytes.fill(count);
        }
    }
    let unused = ran.iter().filter(|r| !**r).count() as u64;
    Some((total, unused))
}

/// The unused JavaScript of each external script (`http(s)`, not the
/// page's inline scripts, which carry the page's URL), by URL.
fn unused_js(scripts: &Value, page: &str) -> Vec<Unused> {
    let mut by_url: Vec<Unused> = Vec::new();
    for script in scripts.as_array().into_iter().flatten() {
        let url = script["url"].as_str().unwrap_or_default();
        if !url.starts_with("http") || url == page {
            continue;
        }
        let functions = script["functions"].as_array().cloned().unwrap_or_default();
        let Some((total, unused)) = script_bytes(&functions) else {
            continue;
        };
        match by_url.iter_mut().find(|u| u.url == url) {
            Some(known) => {
                known.total += total;
                known.unused += unused;
            }
            None => by_url.push(Unused {
                url: url.to_string(),
                kind: "js",
                total,
                unused,
            }),
        }
    }
    by_url
}

/// The unused CSS of each external stylesheet the usage covers: its length
/// minus the bytes of its rules that matched (Lighthouse's measure). Sheets
/// with no rule in the usage belonged to the document the reload replaced
/// (`CSS.enable` announced them).
fn unused_css(usage: &Value, sheets: &HashMap<String, Sheet>) -> Vec<Unused> {
    let mut used: HashMap<&str, u64> = HashMap::new();
    for rule in usage.as_array().into_iter().flatten() {
        let id = rule["styleSheetId"].as_str().unwrap_or_default();
        let bytes = used.entry(id).or_default();
        if rule["used"].as_bool() == Some(true) {
            let start = rule["startOffset"].as_f64().unwrap_or(0.0);
            let end = rule["endOffset"].as_f64().unwrap_or(0.0);
            *bytes += (end - start).max(0.0) as u64;
        }
    }
    let mut rows: Vec<Unused> = sheets
        .iter()
        .filter(|(_, s)| !s.inline && s.url.starts_with("http") && s.length > 0)
        .filter_map(|(id, s)| {
            let matched = *used.get(id.as_str())?;
            Some(Unused {
                url: s.url.clone(),
                kind: "css",
                total: s.length,
                unused: s.length.saturating_sub(matched),
            })
        })
        .collect();
    rows.sort_by(|a, b| a.url.cmp(&b.url));
    rows
}

/// `url` cut to 150 characters with an ellipsis, as the insights' URLs.
fn short_url(url: &str) -> String {
    match url.char_indices().nth(149) {
        Some((at, _)) if url.chars().count() > 150 => format!("{}…", &url[..at]),
        _ => url.to_string(),
    }
}

/// `unused_code` from every resource's coverage.
fn summary(mut rows: Vec<Unused>) -> Value {
    let sum = |kind: &str, rows: &[Unused]| -> u64 {
        rows.iter()
            .filter(|r| r.kind == kind)
            .map(|r| r.unused)
            .sum()
    };
    let (js, css) = (sum("js", &rows), sum("css", &rows));
    rows.retain(|r| r.unused > 0);
    rows.sort_by_key(|r| std::cmp::Reverse(r.unused));
    let top: Vec<Value> = rows
        .iter()
        .take(TOP)
        .map(|r| {
            let percent = (r.unused as f64 * 1000.0 / r.total as f64).round() / 10.0;
            json!({"url": short_url(&r.url), "kind": r.kind,
                "total_bytes": r.total, "unused_bytes": r.unused, "unused_percent": percent})
        })
        .collect();
    json!({"unused_js_bytes": js, "unused_css_bytes": css, "top_unused": top})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::cdp_domains::tests::{
        fire_events_after, mock_handler, spawn_recording_mock,
    };

    fn range(start: u64, end: u64, count: u64) -> Value {
        json!({"startOffset": start, "endOffset": end, "count": count})
    }

    /// ISSUE-54: a byte is unused when its innermost range never ran.
    #[test]
    fn innermost_range_decides() {
        // Given a 1000-byte script whose function at 100..400 never ran,
        // except a block at 200..300 that did
        let functions = [
            json!({"functionName": "", "ranges": [range(0, 1000, 1)]}),
            json!({"functionName": "f", "ranges": [range(100, 400, 0), range(200, 300, 1)]}),
        ];
        // Then 200 bytes are unused
        assert_eq!(script_bytes(&functions), Some((1000, 200)));
        assert_eq!(script_bytes(&[]), None);
    }

    /// ISSUE-54: external scripts count, by URL; the page's inline ones
    /// and those without a URL do not.
    #[test]
    fn external_scripts_count() {
        let script =
            |url: &str, ranges: Value| json!({"url": url, "functions": [{"ranges": ranges}]});
        let scripts = json!([
            script(
                "http://a/app.js",
                json!([range(0, 100, 1), range(50, 100, 0)])
            ),
            script("http://a/app.js", json!([range(0, 10, 0)])),
            script("http://a/", json!([range(0, 500, 0)])),
            script("", json!([range(0, 500, 0)])),
        ]);
        let rows = unused_js(&scripts, "http://a/");
        assert_eq!(
            rows,
            [Unused {
                url: "http://a/app.js".into(),
                kind: "js",
                total: 110,
                unused: 60
            }]
        );
    }

    /// ISSUE-54: a stylesheet's unused bytes are its length minus its
    /// matched rules; inline sheets do not count, nor sheets with no rule in
    /// the usage (those of the document the reload replaced).
    #[test]
    fn stylesheets_count_their_matched_rules() {
        let sheet = |url: &str, length: u64, inline: bool| Sheet {
            url: url.into(),
            length,
            inline,
        };
        let sheets = HashMap::from([
            ("S1".to_string(), sheet("http://a/site.css", 500, false)),
            ("S2".to_string(), sheet("http://a/", 300, true)),
            ("S3".to_string(), sheet("http://a/print.css", 200, false)),
            ("S4".to_string(), sheet("http://a/site.css", 500, false)),
        ]);
        let usage = json!([
            {"styleSheetId": "S1", "startOffset": 0, "endOffset": 100, "used": true},
            {"styleSheetId": "S1", "startOffset": 100, "endOffset": 300, "used": false},
            {"styleSheetId": "S2", "startOffset": 0, "endOffset": 300, "used": false},
            {"styleSheetId": "S3", "startOffset": 0, "endOffset": 200, "used": false}]);
        let rows = unused_css(&usage, &sheets);
        let unused: Vec<(&str, u64)> = rows.iter().map(|r| (r.url.as_str(), r.unused)).collect();
        assert_eq!(
            unused,
            [("http://a/print.css", 200), ("http://a/site.css", 400)]
        );
        // And the summary adds them up, the largest first
        let top = summary(rows);
        assert_eq!(top["unused_css_bytes"], 600);
        assert_eq!(top["top_unused"][0]["url"], "http://a/site.css");
        assert_eq!(top["top_unused"][0]["unused_percent"], 80.0);
    }

    /// ISSUE-54: long URLs are cut as the insights' are.
    #[test]
    fn long_urls_are_cut() {
        let long = format!("http://a/{}", "x".repeat(300));
        let cut = short_url(&long);
        assert_eq!(cut.chars().count(), 150);
        assert!(cut.ends_with("x…"), "{cut}");
        assert_eq!(short_url("http://a/b.js"), "http://a/b.js");
    }

    /// ISSUE-54 (Chrome): coverage starts before the reload and stops on
    /// every path, the stylesheets coming from CSS.styleSheetAdded.
    #[tokio::test]
    async fn recording_starts_and_stops_the_domains() {
        // Given a page with one stylesheet and one script, a third unused
        let mock = spawn_recording_mock().await;
        let header = json!({"header": {"styleSheetId": "S1", "sourceURL": "http://a/site.css", "length": 90, "isInline": false}});
        fire_events_after(mock, "CSS.enable", vec![("CSS.styleSheetAdded", header)]);
        mock.respond_with("Profiler.takePreciseCoverage", |_| {
            let scripts = json!([{"url": "http://a/app.js", "functions": [{"ranges": [range(0, 300, 1), range(0, 100, 0)]}]}]);
            json!({"result": {"result": scripts}})
        });
        mock.respond_with("CSS.stopRuleUsageTracking", |_| {
            json!({"ruleUsage": [{"styleSheetId": "S1", "startOffset": 0, "endOffset": 30, "used": true}]})
        });
        let handler = mock_handler(mock.http_port);
        let target = handler
            .session(None)
            .await
            .unwrap()
            .target(None)
            .await
            .unwrap();
        // When coverage is recorded
        let recording = start(&target).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let unused = recording.finish(&target).await.unwrap();
        // Then both counts are reported
        assert_eq!(
            (
                unused["unused_js_bytes"].clone(),
                unused["unused_css_bytes"].clone()
            ),
            (json!(100), json!(60))
        );
        // And every domain it enabled was disabled again
        let methods: Vec<String> = mock.commands().into_iter().map(|c| c.method).collect();
        let at = |m: &str| methods.iter().position(|x| x == m).unwrap();
        assert!(at("Profiler.startPreciseCoverage") < at("Profiler.takePreciseCoverage"));
        for method in STOP {
            assert!(at("Profiler.takePreciseCoverage") < at(method), "{method}");
        }
    }
}
