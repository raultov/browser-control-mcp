//! The load metrics of a Chrome trace, as Lighthouse reads them: times are
//! from the `navigationStart` of the tab's main frame (its last one with a
//! document URL), First and Largest Contentful Paint from Chrome's marks,
//! Total Blocking Time from the main thread's tasks after FCP, Cumulative
//! Layout Shift from session windows of layout shifts, and Speed Index
//! from the screenshots (the browser's: Chrome does not say which tab they
//! show, so another tab painting during the load counts, as in Lighthouse).

use super::scoring::Metric;
use super::speed_index;
use serde_json::Value;

/// A task blocks the main thread for whatever it lasts beyond this (ms).
const BLOCKING_THRESHOLD_MS: f64 = 50.0;

/// A session window of layout shifts closes after a second without one,
/// or five seconds after it opened (µs).
const SHIFT_GAP_US: f64 = 1_000_000.0;
const SHIFT_WINDOW_US: f64 = 5_000_000.0;

/// The metrics a trace gives, by [`Metric`]; `None` for one it lacks.
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct LoadMetrics {
    values: [Option<f64>; 5],
    /// How many screenshots the Speed Index was computed from.
    pub(crate) screenshots: usize,
}

impl LoadMetrics {
    /// The value of `metric` (ms, or unitless for CLS).
    pub(crate) fn get(&self, metric: Metric) -> Option<f64> {
        self.values[Self::slot(metric)]
    }

    /// Sets the value of `metric`.
    pub(crate) fn set(&mut self, metric: Metric, value: Option<f64>) {
        self.values[Self::slot(metric)] = value;
    }

    fn slot(metric: Metric) -> usize {
        Metric::ALL.iter().position(|m| *m == metric).unwrap_or(0)
    }
}

/// The navigation the metrics count from: its time and the main thread
/// (process, thread) of its renderer.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Navigation {
    ts: f64,
    pid: u64,
    tid: u64,
}

fn name(event: &Value) -> &str {
    event["name"].as_str().unwrap_or_default()
}

fn ts(event: &Value) -> f64 {
    event["ts"].as_f64().unwrap_or_default()
}

/// The last `navigationStart` of `frame` with a document URL (Chrome
/// writes one without, for the initial empty document, around it).
fn navigation(events: &[Value], frame: &str) -> Option<Navigation> {
    let starts = events
        .iter()
        .filter(|e| name(e) == "navigationStart" && e["args"]["frame"] == frame);
    let with_url = starts.filter(|e| {
        let url = e["args"]["data"]["documentLoaderURL"].as_str();
        url.is_some_and(|u| !u.is_empty())
    });
    let start = with_url.max_by(|a, b| ts(a).total_cmp(&ts(b)))?;
    Some(Navigation {
        ts: ts(start),
        pid: start["pid"].as_u64()?,
        tid: start["tid"].as_u64()?,
    })
}

/// The metrics of the load of `frame` (the tab's target id, which is its
/// main frame's id) in `events`; `None` when the trace has no navigation
/// of it.
pub(crate) fn load_metrics(events: &[Value], frame: &str) -> Option<LoadMetrics> {
    let navigation = navigation(events, frame)?;
    let after = |e: &&Value| ts(e) >= navigation.ts;
    let of_frame = |e: &&Value| e["args"]["frame"] == frame;
    let since = |ts: f64| ((ts - navigation.ts) / 1000.0 * 10.0).round() / 10.0;
    let fcp = events
        .iter()
        .filter(|e| name(e) == "firstContentfulPaint")
        .filter(after)
        .filter(of_frame)
        .map(ts)
        .min_by(f64::total_cmp);
    let mut metrics = LoadMetrics::default();
    metrics.set(Metric::FirstContentfulPaint, fcp.map(since));
    let lcp = largest_contentful_paint(events, frame, navigation.ts);
    metrics.set(Metric::LargestContentfulPaint, lcp.map(since));
    let tbt = fcp.map(|fcp| total_blocking_time(events, navigation, fcp));
    metrics.set(Metric::TotalBlockingTime, tbt);
    let cls = cumulative_layout_shift(events, navigation);
    metrics.set(Metric::CumulativeLayoutShift, Some(cls));
    let (index, screenshots) = speed_index(events, navigation.ts);
    metrics.screenshots = screenshots;
    let index = index.map(|ms| (ms * 10.0).round() / 10.0);
    metrics.set(Metric::SpeedIndex, index);
    Some(metrics)
}

/// The last LCP candidate of `frame` since `start`, unless Chrome
/// invalidated it after (then there is none).
fn largest_contentful_paint(events: &[Value], frame: &str, start: f64) -> Option<f64> {
    let last = events
        .iter()
        .filter(|e| name(e).starts_with("largestContentfulPaint::"))
        .filter(|e| ts(e) >= start && e["args"]["frame"] == frame)
        .max_by(|a, b| ts(a).total_cmp(&ts(b)))?;
    (name(last) == "largestContentfulPaint::Candidate").then(|| ts(last))
}

/// The main thread's top-level tasks after `fcp`, each blocking for what
/// it lasts beyond 50 ms once clipped to start at FCP (Lighthouse's
/// `calculateTbtImpactForEvent`; it also stops at Time to Interactive,
/// which this does not compute: the trace's end bounds it).
fn total_blocking_time(events: &[Value], navigation: Navigation, fcp: f64) -> f64 {
    let mut tasks: Vec<(f64, f64)> = events
        .iter()
        .filter(|e| name(e) == "RunTask" && e["ph"] == "X")
        .filter(|e| e["pid"].as_u64() == Some(navigation.pid))
        .filter(|e| e["tid"].as_u64() == Some(navigation.tid))
        .map(|e| (ts(e), ts(e) + e["dur"].as_f64().unwrap_or_default()))
        .collect();
    tasks.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut blocking = 0.0;
    let mut previous_end = f64::MIN;
    for (start, end) in tasks {
        // A task nested in the previous one is part of it.
        if start < previous_end {
            continue;
        }
        previous_end = end;
        let duration = (end - start.max(fcp)) / 1000.0;
        if end > fcp && duration >= BLOCKING_THRESHOLD_MS {
            blocking += duration - BLOCKING_THRESHOLD_MS;
        }
    }
    (blocking * 10.0).round() / 10.0
}

/// How long after the viewport changed a shift flagged as caused by input
/// still counts (ms): Chrome flags the shifts of a resize, and the device's
/// viewport is set right before the load (Lighthouse's `RECENT_INPUT_WINDOW`).
const RECENT_INPUT_WINDOW_MS: f64 = 500.0;

/// The renderer's layout shifts Lighthouse counts, `(ts, score)` in time
/// order: those not caused by recent input, and those flagged so within
/// 500 ms of the start until a first unflagged one (its
/// `getLayoutShiftEvents`, the viewport change taken as the navigation's
/// start: the trace begins after it).
fn counted_shifts(events: &[Value], navigation: Navigation) -> Vec<(f64, f64)> {
    let mut shifts: Vec<&Value> = events
        .iter()
        .filter(|e| name(e) == "LayoutShift" && ts(e) >= navigation.ts)
        .filter(|e| e["pid"].as_u64() == Some(navigation.pid))
        .collect();
    shifts.sort_by(|a, b| ts(a).total_cmp(&ts(b)));
    let mut respect_input = false;
    let mut counted = Vec::new();
    for shift in shifts {
        let data = &shift["args"]["data"];
        if data["had_recent_input"] == true {
            let since = (ts(shift) - navigation.ts) / 1000.0;
            if since > RECENT_INPUT_WINDOW_MS || respect_input {
                continue;
            }
        } else {
            respect_input = true;
        }
        let score = data["weighted_score_delta"]
            .as_f64()
            .or(data["score"].as_f64());
        counted.push((ts(shift), score.unwrap_or_default()));
    }
    counted
}

/// The largest session window of the shifts Lighthouse counts, rounded to
/// 4 decimals.
fn cumulative_layout_shift(events: &[Value], navigation: Navigation) -> f64 {
    let (mut largest, mut current, mut opened, mut last) = (0.0f64, 0.0, 0.0, f64::MIN);
    for (at, score) in counted_shifts(events, navigation) {
        if at - last > SHIFT_GAP_US || at - opened > SHIFT_WINDOW_US {
            current = 0.0;
            opened = at;
        }
        current += score;
        last = at;
        largest = largest.max(current);
    }
    (largest * 10_000.0).round() / 10_000.0
}

/// The Speed Index (ms) of the screenshots since `start`, and how many
/// there were.
fn speed_index(events: &[Value], start: f64) -> (Option<f64>, usize) {
    use base64::Engine;
    let mut shots: Vec<(f64, &str)> = events
        .iter()
        .filter(|e| name(e) == "Screenshot" && ts(e) >= start)
        .filter_map(|e| Some((ts(e), e["args"]["snapshot"].as_str()?)))
        .collect();
    shots.sort_by(|a, b| a.0.total_cmp(&b.0));
    let frames: Vec<(f64, Box<speed_index::Histogram>)> = shots
        .iter()
        .filter_map(|(at, data)| {
            let jpeg = base64::engine::general_purpose::STANDARD
                .decode(data)
                .ok()?;
            Some(((at - start) / 1000.0, speed_index::histogram(&jpeg)?))
        })
        .collect();
    (speed_index::speed_index(&frames), frames.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The trimmed trace of a real mobile load (see SOURCES.md).
    fn fixture() -> Vec<Value> {
        let trace: Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/cdp/lighthouse-trace-mobile.json"
        ))
        .unwrap();
        trace["traceEvents"].as_array().unwrap().clone()
    }

    const FRAME: &str = "679A293C1D20A0F06F14BC9E88EF763D";

    /// LH-TRACE-01: a real Chrome 156 mobile load gives Lighthouse's five
    /// metrics: paints from the marks, the 252 ms task after FCP blocking
    /// for 202 ms, the one layout shift, and a Speed Index at or after FCP.
    #[test]
    fn a_real_trace_gives_the_five_metrics() {
        // Given the trace of a load with a late banner and a long task
        let events = fixture();
        // When its metrics are read
        let metrics = load_metrics(&events, FRAME).unwrap();
        // Then each one is there, counted from the navigation with a URL
        let fcp = metrics.get(Metric::FirstContentfulPaint).unwrap();
        assert!((600.0..620.0).contains(&fcp), "{fcp}");
        assert_eq!(metrics.get(Metric::LargestContentfulPaint), Some(fcp));
        let tbt = metrics.get(Metric::TotalBlockingTime).unwrap();
        assert!((195.0..210.0).contains(&tbt), "{tbt}");
        assert_eq!(metrics.get(Metric::CumulativeLayoutShift), Some(0.0465));
        let si = metrics.get(Metric::SpeedIndex).unwrap();
        assert!(si > fcp && si < 1500.0, "{si}");
        assert_eq!(metrics.screenshots, 20);
        // Another frame's trace has no navigation
        assert_eq!(load_metrics(&events, "OTHER"), None);
    }

    /// LH-TRACE-02: layout shifts group in session windows (1 s gap, 5 s
    /// at most), the largest one counts, and input-driven ones do not,
    /// unless within 500 ms of the start before any other (the resize).
    #[test]
    fn layout_shifts_count_by_session_window() {
        // Given shifts at 0, 0.5 s and 2 s (a new window), and one after input
        let shift = |ms: f64, score: f64, input: bool| {
            json!({"name": "LayoutShift", "pid": 1, "ts": ms * 1000.0,
                   "args": {"data": {"weighted_score_delta": score, "had_recent_input": input}}})
        };
        let events = vec![
            shift(0.0, 0.1, false),
            shift(500.0, 0.1, false),
            shift(2000.0, 0.15, false),
            shift(2100.0, 0.5, true),
        ];
        let navigation = Navigation {
            ts: 0.0,
            pid: 1,
            tid: 1,
        };
        // When CLS is computed
        // Then the first window (0.2) beats the second (0.15)
        assert_eq!(cumulative_layout_shift(&events, navigation), 0.2);
        // A flagged shift right after the start (the resize) counts
        let resized = vec![shift(100.0, 0.3, true), shift(600.0, 0.1, false)];
        assert_eq!(cumulative_layout_shift(&resized, navigation), 0.4);
    }

    /// LH-TRACE-03: only the main thread's top-level tasks after FCP block,
    /// one straddling FCP for its part after it; an invalidated LCP is none.
    #[test]
    fn blocking_time_and_lcp_follow_lighthouse() {
        let task = |pid: u64, start_ms: f64, ms: f64| json!({"name": "RunTask", "ph": "X", "pid": pid, "tid": 1, "ts": start_ms * 1000.0, "dur": ms * 1000.0});
        let events = vec![
            task(1, 0.0, 200.0),   // before FCP (at 300 ms): nothing
            task(1, 250.0, 150.0), // 100 ms after FCP: 50
            task(1, 260.0, 120.0), // nested in the previous one
            task(1, 500.0, 40.0),  // short
            task(2, 600.0, 400.0), // another process
            task(1, 1000.0, 80.0), // 30
        ];
        let navigation = Navigation {
            ts: 0.0,
            pid: 1,
            tid: 1,
        };
        assert_eq!(total_blocking_time(&events, navigation, 300_000.0), 80.0);
        let mark = |kind: &str, ms: f64| json!({"name": format!("largestContentfulPaint::{kind}"), "ts": ms * 1000.0, "args": {"frame": "F"}});
        let invalidated = vec![mark("Candidate", 100.0), mark("Invalidate", 200.0)];
        assert_eq!(largest_contentful_paint(&invalidated, "F", 0.0), None);
        let later = vec![mark("Candidate", 100.0), mark("Candidate", 300.0)];
        assert_eq!(largest_contentful_paint(&later, "F", 0.0), Some(300_000.0));
    }
}
