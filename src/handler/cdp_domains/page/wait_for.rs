use crate::handler::McpHandler;
use crate::handler::cdp_domains::cdp_target::CdpTarget;
use rust_mcp_sdk::{
    macros,
    schema::{CallToolError, CallToolRequestParams, CallToolResult},
};
use serde_json::{Value, json};
use std::time::Duration;

#[macros::mcp_tool(
    name = "wait_for",
    description = "Waits until the page shows a text, no longer shows one, has an element in a given state, or is at a URL; every condition given must hold at once. Checks every 100 ms. Side effects: none (read-only). Prerequisites: requires an active tab. Returns: 'Waited N ms for ...' and JSON {elapsed_ms, url} once the conditions hold (at once when they already do); after 'timeout_ms', an error naming what is still missing and the page's URL. Text is the page's visible text (innerText), case-sensitive, including open shadow roots and same-origin iframes; 'selector' can pierce with 'host >>> inner'. A JavaScript dialog or a debugger pause during the wait ends it at once (close the dialog with 'handle_dialog'; inspect the pause with 'evaluate_on_call_frame'). Use this after a click, a submit or a navigation whose content arrives later. Alternatives: 'inspect_dom' with 'query' for a one-off look, 'evaluate_js' for other conditions."
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct WaitForTool {
    /// Chrome instance id from open_instance/list_instances. Omit for the default instance.
    pub instance_id: Option<String>,
    /// The Tab ID of the target tab. Omit to use the active tab.
    pub tab_id: Option<String>,
    /// Wait until the page's visible text contains this (case-sensitive). Defaults to: not checked.
    pub text: Option<String>,
    /// Wait until the page's visible text no longer contains this (case-sensitive), e.g. a 'Loading' message. Defaults to: not checked.
    pub text_gone: Option<String>,
    /// Wait until the first element matching this CSS selector is in 'state'; 'host >>> inner' matches 'inner' inside the open shadow root of 'host' or inside the same-origin iframe 'host' (chainable). Defaults to: not checked.
    pub selector: Option<String>,
    /// The state 'selector' must reach: 'visible' (in the DOM with a visible box), 'hidden' (missing, or without a visible box), 'attached' (in the DOM) or 'detached' (not in it). Only with 'selector'. Defaults to: 'visible'.
    pub state: Option<WaitState>,
    /// Wait until the tab's URL contains this (e.g. a path after a submit). Defaults to: not checked.
    pub url: Option<String>,
    /// How long to wait, in milliseconds. Constraints: 1 to 30000. Defaults to: 5000.
    pub timeout_ms: Option<u64>,
}

/// The `state` argument.
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    ::serde::Deserialize,
    ::serde::Serialize,
    macros::JsonSchema,
)]
pub enum WaitState {
    /// In the DOM with a visible box.
    #[default]
    #[serde(rename = "visible")]
    Visible,
    /// Missing, or without a visible box.
    #[serde(rename = "hidden")]
    Hidden,
    /// In the DOM.
    #[serde(rename = "attached")]
    Attached,
    /// Not in the DOM.
    #[serde(rename = "detached")]
    Detached,
}

impl WaitState {
    fn name(self) -> &'static str {
        match self {
            Self::Visible => "visible",
            Self::Hidden => "hidden",
            Self::Attached => "attached",
            Self::Detached => "detached",
        }
    }
}

/// How long `wait_for` waits without `timeout_ms`.
const DEFAULT_TIMEOUT: Duration = Duration::from_millis(5000);

/// The longest `timeout_ms`: past it an MCP client may give up on the call.
const MAX_TIMEOUT_MS: u64 = 30_000;

/// How often the conditions are checked.
const POLL: Duration = Duration::from_millis(100);

/// Why one check of the conditions gave no outcome.
pub(crate) enum CheckError {
    /// The page did not answer this time (a navigation replaced its
    /// document): checked again on the next round.
    Retry(String),
    /// The check cannot succeed (an invalid selector): the wait ends.
    Stop(CallToolError),
}

/// One condition: its key in the check's outcome and how to name it.
type Condition = (&'static str, String);

impl WaitForTool {
    /// The conditions asked for, or why the arguments make no sense.
    fn conditions(&self) -> Result<Vec<Condition>, CallToolError> {
        let quote = crate::util::js::string_literal;
        let state = self.state.unwrap_or_default().name();
        let wanted: Vec<Condition> = [
            self.text
                .as_ref()
                .map(|t| ("text", format!("the text {} to appear", quote(t)))),
            self.text_gone
                .as_ref()
                .map(|t| ("text_gone", format!("the text {} to disappear", quote(t)))),
            self.selector
                .as_ref()
                .map(|s| ("selector", format!("{} to be {state}", quote(s)))),
            self.url
                .as_ref()
                .map(|u| ("url", format!("the URL to contain {}", quote(u)))),
        ]
        .into_iter()
        .flatten()
        .collect();
        let fail = |message: &str| Err(CallToolError::from_message(message));
        if wanted.is_empty() {
            return fail("wait_for needs at least one of 'text', 'text_gone', 'selector' or 'url'");
        }
        if self.state.is_some() && self.selector.is_none() {
            return fail("'state' applies to 'selector', which is missing");
        }
        Ok(wanted)
    }

    /// How long to wait, or why `timeout_ms` is refused.
    fn timeout(&self) -> Result<Duration, CallToolError> {
        match self.timeout_ms {
            None => Ok(DEFAULT_TIMEOUT),
            Some(ms @ 1..=MAX_TIMEOUT_MS) => Ok(Duration::from_millis(ms)),
            Some(ms) => Err(CallToolError::from_message(format!(
                "timeout_ms must be between 1 and {MAX_TIMEOUT_MS}, got {ms}"
            ))),
        }
    }

    /// Checks the arguments (before any browser work).
    pub(crate) fn check_arguments(&self) -> Result<(), CallToolError> {
        self.conditions()?;
        self.timeout()?;
        Ok(())
    }

    /// The JavaScript of one check (shared by every browser).
    pub(crate) fn expression(&self) -> String {
        crate::util::js::wait_check(&crate::util::js::WaitConditions {
            text: self.text.as_deref(),
            text_gone: self.text_gone.as_deref(),
            selector: self.selector.as_deref(),
            state: self.state.unwrap_or_default().name(),
            url: self.url.as_deref(),
        })
    }

    /// Runs `check` (one evaluation of [`Self::expression`]) every [`POLL`]
    /// until the conditions hold (the reply) or the timeout passes (an
    /// error naming what is still missing).
    pub(crate) async fn poll<F: Future<Output = Result<Value, CheckError>>>(
        &self,
        mut check: impl FnMut() -> F,
    ) -> Result<CallToolResult, CallToolError> {
        let wanted = self.conditions()?;
        let timeout = self.timeout()?;
        let started = tokio::time::Instant::now();
        let mut last = Err("no check finished in time".to_string());
        loop {
            let left = timeout.saturating_sub(started.elapsed());
            match tokio::time::timeout(left, check()).await {
                Ok(Ok(outcome)) if holds(&wanted, &outcome) => {
                    return Ok(waited(&wanted, &outcome, started.elapsed()));
                }
                Ok(Ok(outcome)) => last = Ok(outcome),
                Ok(Err(CheckError::Retry(error))) => last = Err(error),
                Ok(Err(CheckError::Stop(error))) => return Err(error),
                Err(_elapsed) => break,
            }
            let left = timeout.saturating_sub(started.elapsed());
            if left.is_zero() {
                break;
            }
            tokio::time::sleep(POLL.min(left)).await;
        }
        Err(timed_out(&wanted, &last, started.elapsed()))
    }

    pub async fn handle(
        params: CallToolRequestParams,
        handler: &McpHandler,
    ) -> Result<CallToolResult, CallToolError> {
        let args: WaitForTool = crate::handler::parse_args(params)?;
        args.check_arguments()?;
        let target =
            crate::handler::resolve_target(handler, args.instance_id.clone(), args.tab_id.clone())
                .await?;
        let expression = args.expression();
        let waiting = args.poll(|| check(&target, &expression));
        target
            .race_interruptions("wait_for", waiting)
            .await
            .unwrap_or_else(|stopped| Err(CallToolError::from_message(stopped.during("wait_for"))))
    }
}

/// One check on Chrome: a failed command is retried (a navigation
/// destroyed the context), an exception ends the wait.
async fn check(target: &CdpTarget, expression: &str) -> Result<Value, CheckError> {
    let params = json!({"expression": expression, "returnByValue": true});
    let reply = target
        .send_raw_command("Runtime.evaluate", params)
        .await
        .map_err(|e| CheckError::Retry(crate::handler::cdp_failure("CDP Error", &e)))?;
    crate::handler::snippet_outcome(reply.result.unwrap_or_default()).map_err(CheckError::Stop)
}

/// Whether `outcome` says every condition holds.
fn holds(wanted: &[Condition], outcome: &Value) -> bool {
    wanted.iter().all(|(key, _)| outcome[key] == json!(true))
}

fn millis(elapsed: Duration) -> u64 {
    u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
}

fn named<'a>(conditions: impl Iterator<Item = &'a Condition>) -> String {
    conditions
        .map(|(_, name)| name.as_str())
        .collect::<Vec<_>>()
        .join(" and ")
}

/// The reply once the conditions hold.
fn waited(wanted: &[Condition], outcome: &Value, elapsed: Duration) -> CallToolResult {
    let ms = millis(elapsed);
    let body = json!({"elapsed_ms": ms, "url": outcome["href"]});
    let body = serde_json::to_string_pretty(&body).unwrap_or_default();
    let first = format!("Waited {ms} ms for {}.", named(wanted.iter()));
    CallToolResult::text_content(vec![format!("{first}\n{body}").into()])
}

/// The error once the timeout passed: what the last check still missed,
/// or why no check answered.
fn timed_out(
    wanted: &[Condition],
    last: &Result<Value, String>,
    elapsed: Duration,
) -> CallToolError {
    let ms = millis(elapsed);
    let message = match last {
        Ok(outcome) => {
            let missing = named(wanted.iter().filter(|(key, _)| outcome[key] != json!(true)));
            let url = outcome["href"].as_str().unwrap_or("an unknown URL");
            format!("Timed out after {ms} ms waiting for {missing}; the page is at {url}.")
        }
        Err(error) => format!(
            "Timed out after {ms} ms waiting for {}: no check of the page finished ({error}).",
            named(wanted.iter())
        ),
    };
    CallToolError::from_message(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    fn tool(args: Value) -> WaitForTool {
        serde_json::from_value(args).unwrap()
    }

    /// Polls `tool` with `outcomes` in turn, then a page where nothing
    /// holds; the result and how many checks ran.
    async fn run(
        tool: &WaitForTool,
        outcomes: Vec<Result<Value, CheckError>>,
    ) -> (Result<CallToolResult, CallToolError>, usize) {
        let mut queue = VecDeque::from(outcomes);
        let mut checks = 0;
        let nothing = json!({"href": "http://a/login", "text": false, "url": false});
        let result = tool
            .poll(|| {
                checks += 1;
                let next = queue.pop_front().unwrap_or_else(|| Ok(nothing.clone()));
                std::future::ready(next)
            })
            .await;
        (result, checks)
    }

    fn text(result: &CallToolResult) -> String {
        crate::handler::cdp_domains::tests::reply_text(result)
    }

    /// ISSUE-2: arguments that cannot be waited for are refused before any
    /// browser work, naming the problem.
    #[test]
    fn senseless_arguments_are_refused() {
        let error = |args: Value| tool(args).check_arguments().unwrap_err().to_string();
        assert!(error(json!({})).contains("at least one of 'text'"));
        assert!(
            error(json!({"text": "a", "state": "hidden"}))
                .contains("'state' applies to 'selector'")
        );
        assert!(
            error(json!({"text": "a", "timeout_ms": 0})).contains("between 1 and 30000, got 0")
        );
        assert!(error(json!({"text": "a", "timeout_ms": 30_001})).contains("got 30001"));
        assert!(
            tool(json!({"selector": "#a", "state": "detached"}))
                .check_arguments()
                .is_ok()
        );
    }

    /// ISSUE-2: the conditions appear after a few checks: the reply says
    /// what held and how long it took, with the URL.
    #[tokio::test]
    async fn conditions_that_appear_end_the_wait() {
        // Given a text that shows on the third check
        let waiting = tool(json!({"text": "Welcome", "timeout_ms": 2000}));
        let shown = json!({"href": "http://a/home", "text": true});
        let pending = json!({"href": "http://a/login", "text": false});
        let outcomes = vec![Ok(pending.clone()), Ok(pending), Ok(shown)];
        // When the wait runs
        let (result, checks) = run(&waiting, outcomes).await;
        // Then it ends on that check
        let text = text(&result.unwrap());
        assert_eq!(checks, 3);
        assert!(text.starts_with("Waited "), "{text}");
        assert!(
            text.contains(" ms for the text \"Welcome\" to appear.\n"),
            "{text}"
        );
        let body: Value = serde_json::from_str(&text[text.find('{').unwrap()..]).unwrap();
        assert_eq!(body["url"], "http://a/home");
    }

    /// ISSUE-2: conditions that already hold answer on the first check.
    #[tokio::test]
    async fn conditions_already_holding_answer_at_once() {
        let waiting = tool(json!({"selector": "#done", "state": "attached"}));
        let held = json!({"href": "http://a/", "selector": true});
        let (result, checks) = run(&waiting, vec![Ok(held)]).await;
        assert!(result.is_ok());
        assert_eq!(checks, 1);
    }

    /// ISSUE-2: after the timeout the error names only what is still
    /// missing, and where the page is.
    #[tokio::test]
    async fn timeout_names_what_is_missing() {
        // Given a text that never shows on a page already at the URL
        let waiting = tool(json!({"text": "Welcome", "url": "/login", "timeout_ms": 250}));
        let pending = json!({"href": "http://a/login", "text": false, "url": true});
        // When the wait runs out
        let outcomes = std::iter::repeat_with(|| Ok(pending.clone())).take(10);
        let started = std::time::Instant::now();
        let (result, checks) = run(&waiting, outcomes.collect()).await;
        // Then the error names the text only, after about the timeout
        let error = result.unwrap_err().to_string();
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(checks > 1, "checked every {POLL:?}");
        assert!(error.starts_with("Timed out after "), "{error}");
        assert!(
            error.ends_with(
                " ms waiting for the text \"Welcome\" to appear; the page is at http://a/login."
            ),
            "{error}"
        );
    }

    /// ISSUE-2: a check the page did not answer is retried; a check that
    /// cannot succeed (an invalid selector) ends the wait with its error.
    #[tokio::test]
    async fn failed_checks_retry_or_stop() {
        let waiting = tool(json!({"text": "Welcome"}));
        let shown = json!({"href": "http://a/", "text": true});
        let gone = CheckError::Retry("Execution context was destroyed".into());
        let (result, checks) = run(&waiting, vec![Err(gone), Ok(shown)]).await;
        assert!(result.is_ok());
        assert_eq!(checks, 2);
        let invalid = CheckError::Stop(CallToolError::from_message(
            "SyntaxError: '##' is not valid",
        ));
        let (result, checks) = run(&waiting, vec![Err(invalid)]).await;
        assert_eq!(
            result.unwrap_err().to_string(),
            "SyntaxError: '##' is not valid"
        );
        assert_eq!(checks, 1);
    }

    /// ISSUE-2 (Chrome): the tool evaluates the shared check by value and
    /// answers from its outcome.
    #[tokio::test]
    async fn chrome_waits_through_the_shared_check() {
        // Given a page where the text shows
        let mock = crate::handler::cdp_domains::tests::spawn_recording_mock().await;
        let shown = json!({"href": "http://a/", "text": true});
        mock.respond_with(
            "Runtime.evaluate",
            move |_| json!({"result": {"result": {"type": "object", "value": shown.clone()}}}),
        );
        let handler = crate::handler::cdp_domains::tests::mock_handler(mock.http_port);
        // When wait_for runs
        let params =
            crate::handler::cdp_domains::tests::call("wait_for", json!({"text": "Welcome"}));
        let reply = WaitForTool::handle(params, &handler).await.unwrap();
        // Then it answered from the check, sent by value
        assert!(text(&reply).starts_with("Waited "), "{}", text(&reply));
        let sent = &mock.commands_named("Runtime.evaluate")[0].params;
        assert_eq!(sent["returnByValue"], true);
        assert!(
            sent["expression"]
                .as_str()
                .unwrap()
                .contains("const text = \"Welcome\"")
        );
    }
}
