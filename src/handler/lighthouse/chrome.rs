//! The Chrome side of `lighthouse_report`: the tab goes to `about:blank`,
//! takes the device's viewport, throttling and a bypassed cache, and loads
//! the page while Chrome traces it (with screenshots, for the Speed Index)
//! and records code coverage. The throttling is put back before the page's
//! checks run, the viewport after them: the tab ends as it emulated before.

use super::{Conditions, Findings, LighthouseReportTool, trace};
use crate::handler::McpHandler;
use crate::handler::cdp_domains::cdp_target::CdpTarget;
use crate::handler::cdp_domains::log::LogState;
use crate::handler::cdp_domains::page::emulate::Emulation;
use crate::handler::cdp_domains::page::navigate::navigate_and_wait;
use crate::handler::cdp_domains::tracing::profile_page_performance as profile;
use crate::handler::cdp_domains::tracing::{TracingState, coverage};
use crate::handler::dialogs::DialogState;
use crate::handler::navigation::NavigationOutcome;
use crate::handler::{NetworkState, insights};
use rust_mcp_sdk::schema::{CallToolError, CallToolRequestParams, CallToolResult};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, mpsc};

/// The trace categories: the paint and navigation marks, layout shifts,
/// the main thread's tasks, the stacks of forced layouts and the
/// screenshots (Chrome's `toplevel`, most of a trace, is left out).
const CATEGORIES: &str = "loading,blink.user_timing,devtools.timeline,disabled-by-default-devtools.timeline,disabled-by-default-devtools.timeline.stack,disabled-by-default-devtools.screenshot,v8.execute";

/// How long the page runs after `load` while traced: late paints, layout
/// shifts and long tasks still count (Lighthouse waits for the network
/// and the CPU to be quiet; this waits a fixed time).
#[cfg(not(test))]
const SETTLE: Duration = Duration::from_secs(3);
#[cfg(test)]
const SETTLE: Duration = Duration::from_millis(10);

/// How long the page runs after `load` when it is not traced.
#[cfg(not(test))]
const QUIET: Duration = Duration::from_secs(1);
#[cfg(test)]
const QUIET: Duration = Duration::from_millis(10);

/// The tab's states the report uses.
struct States {
    emulation: Arc<Mutex<Emulation>>,
    dialog: Arc<Mutex<DialogState>>,
    network: Arc<Mutex<NetworkState>>,
    tracing: Arc<Mutex<TracingState>>,
    log: Arc<Mutex<LogState>>,
}

/// What the load recorded.
struct Recorded {
    outcome: NavigationOutcome,
    /// The raw trace (with performance).
    trace: Option<String>,
    /// The JavaScript and CSS the load left unused, or why there is none.
    unused: Result<Value, String>,
    /// When the load started (ms since the epoch): console errors count
    /// from then.
    since: f64,
}

/// Sends `method`; its error as text.
async fn send(target: &CdpTarget, method: &str, params: Value) -> Result<Value, String> {
    let reply = target.send_raw_command(method, params).await;
    let reply = reply.map_err(|e| format!("{method} failed: {e}"))?;
    Ok(reply.result.unwrap_or_default())
}

/// Sends each command in order, stopping at the first that fails.
async fn send_all(target: &CdpTarget, commands: &[(&str, Value)]) -> Result<(), String> {
    for (method, params) in commands {
        send(target, method, params.clone()).await?;
    }
    Ok(())
}

/// Sends each command, whatever the others do (putting the tab back): the
/// methods that failed. On a page that cannot answer (`blocked`) they are
/// queued instead, and Chrome runs them once it answers again.
async fn send_each(target: &CdpTarget, commands: &[(&str, Value)], blocked: bool) -> Vec<String> {
    let mut failed = Vec::new();
    for (method, params) in commands {
        if blocked {
            target.queue_raw_command(method, params.clone());
        } else if send(target, method, params.clone()).await.is_err() {
            failed.push(method.to_string());
        }
    }
    failed
}

/// What blocks the page now, if anything: an open dialog, or a pause in
/// the debugger. Commands to the page wait for it, so the report stops.
async fn blocker(target: &CdpTarget, dialog: &Mutex<DialogState>) -> Option<String> {
    if let Some(open) = dialog.lock().await.open.clone() {
        return Some(open.blocking());
    }
    target.is_paused().await.then(|| PAUSED.to_string())
}

/// Why the report stopped on a page paused in the debugger.
const PAUSED: &str =
    "The page is paused in the debugger: resume it (resume), then call lighthouse_report again.";

/// What the report says when it stopped on a blocked page.
const PUT_BACK_LATER: &str =
    "The tab's own emulation and cache are put back once the page answers again.";

/// The commands that emulate `conditions` (throttled only with
/// `throttle`), with the cache bypassed.
fn emulation(conditions: Conditions, throttle: bool) -> Vec<(&'static str, Value)> {
    let mut commands = vec![
        (
            "Emulation.setDeviceMetricsOverride",
            json!({"width": conditions.width, "height": conditions.height,
                   "deviceScaleFactor": conditions.scale, "mobile": conditions.mobile}),
        ),
        (
            "Emulation.setTouchEmulationEnabled",
            json!({"enabled": conditions.mobile}),
        ),
        ("Network.setCacheDisabled", json!({"cacheDisabled": true})),
    ];
    if throttle {
        let rate = json!({"rate": conditions.cpu_slowdown});
        commands.push(("Emulation.setCPUThrottlingRate", rate));
        let network = conditions.network.cdp_conditions();
        commands.push(("Network.emulateNetworkConditions", network));
    }
    commands
}

/// Loads `url` and waits for `load`, ending at once on a dialog or a
/// debugger pause.
async fn navigation(
    target: &CdpTarget,
    network: &Mutex<NetworkState>,
    url: &str,
) -> Result<NavigationOutcome, String> {
    let work = navigate_and_wait(target, network, url);
    match target.race_interruptions("lighthouse_report", work).await {
        Ok(Ok(outcome)) => Ok(outcome),
        Ok(Err(error)) => Err(error.to_string()),
        Err(stopped) => Err(stopped.during("lighthouse_report")),
    }
}

/// Runs `during` while Chrome traces the page: its output and the raw
/// trace. The completion channel is cleared on every path.
async fn traced<T>(
    target: &CdpTarget,
    tracing: &Mutex<TracingState>,
    during: impl std::future::Future<Output = T>,
) -> Result<(T, String), String> {
    let (tx, mut rx) = mpsc::channel(1);
    tracing.lock().await.completion_channel = Some(tx);
    let start = json!({"transferMode": "ReturnAsStream", "categories": CATEGORIES});
    let recorded = match send(target, "Tracing.start", start).await {
        Ok(_) => {
            let output = during.await;
            let ended = send(target, "Tracing.end", json!({})).await;
            let completed = tokio::time::timeout(profile::COMPLETION_TIMEOUT, rx.recv()).await;
            match (ended, completed) {
                (Ok(_), Ok(Some(handle))) => {
                    Ok((output, profile::read_stream(target, &handle).await))
                }
                (Err(e), _) => Err(e),
                _ => Err("Timeout waiting for Tracing.tracingComplete".to_string()),
            }
        }
        Err(e) => Err(e),
    };
    tracing.lock().await.completion_channel = None;
    recorded
}

/// From `about:blank`, emulates the device and loads `url`: traced, with
/// coverage, when `perf`.
async fn record(
    target: &CdpTarget,
    states: &States,
    url: &str,
    (conditions, perf): (Conditions, bool),
) -> Result<Recorded, String> {
    navigation(target, &states.network, "about:blank").await?;
    send_all(target, &emulation(conditions, perf)).await?;
    let since = super::now_ms();
    if !perf {
        let outcome = navigation(target, &states.network, url).await?;
        tokio::time::sleep(QUIET).await;
        let unused = Err(coverage::NEEDS_RELOAD.to_string());
        return Ok(Recorded {
            outcome,
            trace: None,
            unused,
            since,
        });
    }
    let recording = coverage::start(target).await;
    let load = async {
        let outcome = navigation(target, &states.network, url).await;
        if outcome.is_ok() {
            tokio::time::sleep(SETTLE).await;
        }
        outcome
    };
    let traced = traced(target, &states.tracing, load).await;
    let blocked = blocker(target, &states.dialog).await;
    let unused = match (recording, blocked) {
        (Ok(recording), None) => recording.finish(target).await,
        (Ok(recording), Some(why)) => {
            recording.abandon(target);
            Err(why)
        }
        (Err(why), _) => Err(why),
    };
    let (outcome, raw) = traced?;
    let outcome = outcome?;
    Ok(Recorded {
        outcome,
        trace: Some(raw),
        unused,
        since,
    })
}

/// The id of the tab's main frame (the frame the trace's marks name).
async fn main_frame(target: &CdpTarget) -> Option<String> {
    let tree = send(target, "Page.getFrameTree", json!({})).await.ok()?;
    tree.pointer("/frameTree/frame/id")?
        .as_str()
        .map(str::to_string)
}

/// The performance part of the findings: the trace's metrics and the
/// insights that explain them.
async fn performance(
    target: &CdpTarget,
    raw: &str,
    unused: Result<Value, String>,
    found: &mut Findings,
) {
    let events = profile::trace_events(raw);
    let frame = main_frame(target).await.unwrap_or_default();
    let metrics = trace::load_metrics(&events, &frame);
    if metrics.is_none() {
        found
            .notes
            .push("the trace has no navigation of the tab: no metric could be read".into());
    }
    found.metrics = Some(metrics.unwrap_or_default());
    let reflows = profile::forced_reflows(&events);
    drop(events);
    let page = crate::handler::snippet_value(target, insights::SNIPPET).await;
    if let Ok(page) = page {
        let mut annotated = insights::annotate(page);
        insights::add_unused_code(&mut annotated, unused);
        insights::add_forced_reflows(&mut annotated, Ok(reflows));
        found.insights = Some(annotated);
    }
}

/// What the page and the trace found, once the load is recorded.
async fn examine(
    args: &LighthouseReportTool,
    target: &CdpTarget,
    log: &Arc<Mutex<LogState>>,
    recorded: Recorded,
) -> Result<Findings, String> {
    let page = match args.audit() {
        Some(audit) => Some(
            crate::handler::snippet_value(target, &audit.snippet())
                .await
                .map_err(|e| e.to_string())?,
        ),
        None => None,
    };
    let since = recorded.since;
    let mut found = args.findings(recorded.outcome, page, (log, since)).await;
    if let (true, Some(raw)) = (args.wants_performance(), &recorded.trace) {
        performance(target, raw, recorded.unused, &mut found).await;
    }
    Ok(found)
}

/// Records the load and examines it, putting the tab's emulation back on
/// every path: the throttling before the page's checks, the viewport after.
async fn run(
    args: &LighthouseReportTool,
    target: &CdpTarget,
    states: &States,
    url: &str,
) -> Result<Findings, String> {
    let before = states.emulation.lock().await.clone();
    let mode = (args.device().conditions(), args.wants_performance());
    let recorded = record(target, states, url, mode).await;
    let mut back = before.throttling_commands();
    back.push(("Network.setCacheDisabled", json!({"cacheDisabled": false})));
    if let Some(why) = blocker(target, &states.dialog).await {
        back.extend(before.viewport_commands());
        send_each(target, &back, true).await;
        let why = recorded.err().unwrap_or(why);
        return Err(format!("{why} {PUT_BACK_LATER}"));
    }
    let mut failed = send_each(target, &back, false).await;
    let found = match recorded {
        Ok(recorded) => examine(args, target, &states.log, recorded).await,
        Err(why) => Err(why),
    };
    failed.extend(send_each(target, &before.viewport_commands(), false).await);
    with_restore_note(found, &failed)
}

/// `found` with a note naming the restore commands Chrome refused, if any:
/// on a report, among its notes; on a failure, after its error.
fn with_restore_note(
    found: Result<Findings, String>,
    failed: &[String],
) -> Result<Findings, String> {
    if failed.is_empty() {
        return found;
    }
    let note = format!(
        "the tab's own emulation may not be all back: Chrome refused {}",
        failed.join(", ")
    );
    match found {
        Ok(mut found) => {
            found.notes.push(note);
            Ok(found)
        }
        Err(why) => Err(format!("{why} ({note})")),
    }
}

/// The URL of the tab's page, to load again.
async fn current_page(target: &CdpTarget) -> Result<String, CallToolError> {
    let href = crate::handler::snippet_value(target, "location.href").await?;
    match href.as_str() {
        Some(url) if url.starts_with("http") || url.starts_with("file:") => Ok(url.to_string()),
        _ => Err(CallToolError::from_message(
            "The tab shows no web page to report on: give 'url'.",
        )),
    }
}

impl LighthouseReportTool {
    /// Chrome: loads the page in the device's conditions and reports.
    pub async fn handle(
        params: CallToolRequestParams,
        handler: &McpHandler,
    ) -> Result<CallToolResult, CallToolError> {
        let args: Self = crate::handler::parse_args(params)?;
        let given = args.checked_url(handler.flags.local)?.map(str::to_string);
        let session = handler.session(args.instance_id.clone()).await?;
        let target = session.target(args.tab_id.clone()).await?;
        let url = match given {
            Some(url) => url,
            None => current_page(&target).await?,
        };
        let tab = || args.tab_id.clone();
        let states = States {
            emulation: session.emulation_state(tab())?,
            dialog: session.dialog_state(tab())?,
            network: session.network_state(tab())?,
            tracing: session.tracing_state(tab())?,
            log: session.log_state(tab())?,
        };
        let found = run(&args, &target, &states, &url).await;
        found
            .map(|found| args.report("Chrome", found))
            .map_err(CallToolError::from_message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::cdp_domains::tests::{
        call, fire_after, mock_handler, reply_text, spawn_recording_mock,
    };

    /// The id of the main frame in the trace fixture.
    const FRAME: &str = "679A293C1D20A0F06F14BC9E88EF763D";

    /// The page answers every script with an empty audit.
    fn quiet_page(mock: &cdp_browser_lite::test_support::mock_devtools::MockDevTools) {
        mock.respond_with("Runtime.evaluate", |_| {
            json!({"result": {"type": "object", "value": {"url": "http://a.test/", "audits": [], "notes": []}}})
        });
    }

    /// The methods the mock received, in order, from the first `from`.
    fn methods_from(
        mock: &cdp_browser_lite::test_support::mock_devtools::MockDevTools,
        from: &str,
    ) -> Vec<(String, Value)> {
        let all = mock.commands();
        let start = all.iter().position(|c| c.method == from).unwrap_or(0);
        all[start..]
            .iter()
            .map(|c| (c.method.clone(), c.params.clone()))
            .collect()
    }

    /// LH-CDP-01: without performance, the tab goes to about:blank, takes
    /// the mobile viewport with the cache bypassed and no throttling, loads
    /// the page, and gets its own emulation back (none: cleared).
    #[tokio::test]
    async fn checks_only_emulate_the_viewport_and_put_it_back() {
        // Given a page
        let mock = spawn_recording_mock().await;
        quiet_page(mock);
        let handler = mock_handler(mock.http_port);
        // When the accessibility report runs
        let args = json!({"url": "http://a.test/", "categories": ["accessibility"]});
        let reply = LighthouseReportTool::handle(call("lighthouse_report", args), &handler)
            .await
            .unwrap();
        // Then the wire has the load in the device's viewport, then the reset
        let sent = methods_from(mock, "Page.navigate");
        let names: Vec<&str> = sent.iter().map(|(m, _)| m.as_str()).collect();
        assert_eq!(sent[0].1["url"], "about:blank");
        let metrics = names
            .iter()
            .position(|m| *m == "Emulation.setDeviceMetricsOverride")
            .unwrap();
        assert_eq!(
            sent[metrics].1,
            json!({"width": 412, "height": 823, "deviceScaleFactor": 1.75, "mobile": true})
        );
        assert!(!names.contains(&"Tracing.start"), "{names:?}");
        let rates: Vec<&Value> = sent
            .iter()
            .filter(|(m, _)| m == "Emulation.setCPUThrottlingRate")
            .map(|(_, p)| p)
            .collect();
        assert_eq!(rates, [&json!({"rate": 1.0})]);
        let cleared = names
            .iter()
            .position(|m| *m == "Emulation.clearDeviceMetricsOverride")
            .unwrap();
        assert!(cleared > metrics);
        let caches: Vec<&Value> = sent
            .iter()
            .filter(|(m, _)| m == "Network.setCacheDisabled")
            .map(|(_, p)| &p["cacheDisabled"])
            .collect();
        assert_eq!(caches, [&json!(true), &json!(false)]);
        assert!(
            reply_text(&reply).starts_with("Lighthouse-style report of"),
            "{}",
            reply_text(&reply)
        );
    }

    /// The load was traced with screenshots, under the mobile throttling,
    /// which was put back.
    fn assert_throttled_and_traced(
        mock: &cdp_browser_lite::test_support::mock_devtools::MockDevTools,
    ) {
        let sent = methods_from(mock, "Page.navigate");
        let start = sent.iter().find(|(m, _)| m == "Tracing.start").unwrap();
        assert!(
            start.1["categories"]
                .as_str()
                .unwrap()
                .contains("devtools.screenshot")
        );
        let rates: Vec<&Value> = sent
            .iter()
            .filter(|(m, _)| m == "Emulation.setCPUThrottlingRate")
            .map(|(_, p)| &p["rate"])
            .collect();
        assert_eq!(rates, [&json!(4.0), &json!(1.0)]);
        let network = sent
            .iter()
            .find(|(m, _)| m == "Network.emulateNetworkConditions")
            .unwrap();
        assert_eq!(network.1["latency"], 562.5);
    }

    /// LH-CDP-02: performance is traced under throttling, with screenshots,
    /// and scored from the trace's metrics; the throttling is put back.
    #[tokio::test]
    async fn performance_is_scored_from_the_trace() {
        // Given a page whose trace is the fixture's
        let mock = spawn_recording_mock().await;
        quiet_page(mock);
        let trace = include_str!("../../../tests/fixtures/cdp/lighthouse-trace-mobile.json");
        mock.respond_with("IO.read", move |_| json!({"data": trace, "eof": true}));
        mock.respond_with(
            "Page.getFrameTree",
            |_| json!({"frameTree": {"frame": {"id": FRAME}}}),
        );
        fire_after(
            mock,
            "Tracing.end",
            "Tracing.tracingComplete",
            json!({"stream": "s1"}),
        );
        let handler = mock_handler(mock.http_port);
        // When the performance report runs
        let args = json!({"url": "http://a.test/", "categories": ["performance"]});
        let reply = LighthouseReportTool::handle(call("lighthouse_report", args), &handler)
            .await
            .unwrap();
        // Then the load was throttled and traced with screenshots
        assert_throttled_and_traced(mock);
        // And the five metrics are scored
        let text = reply_text(&reply);
        let body: Value = serde_json::from_str(text.split_once('\n').unwrap().1).unwrap();
        let score = body["scores"]["performance"].as_u64().unwrap();
        assert!((90..=100).contains(&score), "{text}");
        assert_eq!(body["metrics"]["cumulative_layout_shift"]["value"], 0.0465);
        assert!(
            body["insights"]["unused_code"].is_object()
                || body["insights"]["unused_code"].is_null()
        );
        let state = handler.session(None).await.unwrap();
        assert!(
            state
                .tracing_state
                .lock()
                .await
                .completion_channel
                .is_none()
        );
    }

    /// Emits `dialog` on the session of the second `Page.navigate` (the
    /// page's, after about:blank's).
    async fn alert_on_second_navigation(
        mock: &'static cdp_browser_lite::test_support::mock_devtools::MockDevTools,
        dialog: Value,
    ) {
        for _ in 0..400 {
            let navigations = mock.commands_named("Page.navigate");
            if let Some(second) = navigations.get(1) {
                let session = second.session_id.as_deref();
                mock.emit("Page.javascriptDialogOpening", dialog, session);
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// LH-CDP-03: a dialog the load opens stops the report at once, naming
    /// handle_dialog, and the tab's emulation is queued to come back.
    #[tokio::test]
    async fn a_dialog_stops_the_report() {
        // Given a page that opens an alert as it loads
        let mock = spawn_recording_mock().await;
        let handler = mock_handler(mock.http_port);
        let session = handler.session(None).await.unwrap();
        session.target(None).await.unwrap();
        let dialog =
            json!({"type": "alert", "message": "hi", "url": "http://a.test/", "defaultPrompt": ""});
        mock.respond_with("Page.navigate", |params| match params["url"].as_str() {
            Some("about:blank") => json!({"frameId": "F"}),
            _ => json!({"frameId": "F", "loaderId": "L2"}),
        });
        tokio::spawn(alert_on_second_navigation(mock, dialog));
        // When the report runs
        let args = json!({"url": "http://a.test/", "categories": ["accessibility"]});
        let started = std::time::Instant::now();
        let error = LighthouseReportTool::handle(call("lighthouse_report", args), &handler)
            .await
            .unwrap_err()
            .to_string();
        // Then it fails at once, saying so, and the reset was sent
        assert!(
            error.contains("handle_dialog") && error.contains(PUT_BACK_LATER),
            "{error}"
        );
        assert!(started.elapsed() < Duration::from_secs(5));
        // Queued without waiting for a reply: it reaches the socket a moment later
        for _ in 0..100 {
            if !mock
                .commands_named("Emulation.clearDeviceMetricsOverride")
                .is_empty()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let names: Vec<String> = methods_from(mock, "Page.navigate")
            .into_iter()
            .map(|(m, _)| m)
            .collect();
        assert!(
            names.contains(&"Emulation.clearDeviceMetricsOverride".to_string()),
            "{names:?}"
        );
    }
}
