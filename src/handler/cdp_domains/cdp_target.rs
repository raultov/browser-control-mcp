use crate::handler::DebuggerState;
use crate::handler::cdp_domains::debugger::with_script_url;
use crate::handler::debugging::{PauseLocation, Paused};
use crate::handler::dialogs::{OpenDialog, unless_blocked};
use crate::handler::interruption::Interruption;
use cdp_browser_lite::{CdpClient, CdpResult, EventFilter, Tab, WsResponse};
use serde::Serialize;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio_stream::{Stream, StreamExt};

/// Where a call's CDP commands go: a tab's session, or the browser
/// connection when no tab is registered.
#[derive(Clone)]
pub(crate) struct CdpTarget {
    connection: Connection,
    /// The debugger state of the tab (its parsed scripts), to name the
    /// script a pause stopped in: Chrome leaves `callFrames[].url` empty.
    debugger: Option<Arc<Mutex<DebuggerState>>>,
}

#[derive(Clone)]
enum Connection {
    Client(CdpClient),
    Tab(Tab),
}

impl std::fmt::Debug for CdpTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.connection {
            Connection::Client(_) => f.write_str("CdpTarget::Client"),
            Connection::Tab(t) => write!(f, "CdpTarget::Tab(target_id: {})", t.target_id()),
        }
    }
}

impl CdpTarget {
    /// The browser connection.
    pub(crate) fn client(client: CdpClient) -> Self {
        Self {
            connection: Connection::Client(client),
            debugger: None,
        }
    }

    /// A tab's session.
    pub(crate) fn tab(tab: Tab) -> Self {
        Self {
            connection: Connection::Tab(tab),
            debugger: None,
        }
    }

    /// The target with its tab's debugger state, so pauses that interrupt a
    /// call name their script.
    pub(crate) fn with_debugger(mut self, state: Arc<Mutex<DebuggerState>>) -> Self {
        self.debugger = Some(state);
        self
    }

    /// Whether the tab is paused in the debugger (a pause it keeps; the
    /// ones the debugger listener skips are never recorded).
    pub(crate) async fn is_paused(&self) -> bool {
        self.paused().await.is_some()
    }

    /// Where the tab is stopped in the debugger, if it is (its script
    /// named from the tab's parsed scripts).
    pub(crate) async fn paused(&self) -> Option<Paused> {
        let state = self.debugger.as_ref()?.lock().await;
        state.paused_call_frame_id.as_ref()?;
        let location = state.paused_at.clone();
        Some(Paused(location.map(|at| with_script_url(at, &state))))
    }

    /// The tab's CDP target id (`None` for the browser connection).
    #[cfg(test)]
    pub(crate) fn tab_target_id(&self) -> Option<&str> {
        match &self.connection {
            Connection::Tab(t) => Some(t.target_id()),
            Connection::Client(_) => None,
        }
    }

    /// Sends a command. A JavaScript dialog the page opens meanwhile, or a
    /// debugger pause its code hits, blocks the page, so the command would
    /// wait for a timeout: it fails at once instead, with a `ProtocolError`
    /// under `dialogs::INTERRUPTED_ERROR_CODE` whose message says why.
    pub(crate) async fn send_raw_command<P: Serialize>(
        &self,
        method: &str,
        params: P,
    ) -> CdpResult<WsResponse> {
        self.send_interruptible(method, params)
            .await
            .unwrap_or_else(|interruption| Err(interrupted_error(&interruption, method)))
    }

    /// Sends a command, or says what interrupted it (see `send_raw_command`),
    /// for input actions whose effect happened anyway.
    pub(crate) async fn send_interruptible<P: Serialize>(
        &self,
        method: &str,
        params: P,
    ) -> Result<CdpResult<WsResponse>, Interruption> {
        let send = async {
            match &self.connection {
                Connection::Client(c) => c.send_raw_command(method, params).await,
                Connection::Tab(t) => t.send_raw_command(method, params).await,
            }
        };
        if method == "Page.handleJavaScriptDialog" {
            return Ok(send.await);
        }
        self.race_interruptions(method, send).await
    }

    /// Runs `work` (one command, or a `call` made of several such as a
    /// wait) unless the page opens a dialog or pauses in the debugger
    /// meanwhile, also between its commands: then what interrupted it.
    pub(crate) async fn race_interruptions<T>(
        &self,
        call: &str,
        work: impl Future<Output = T>,
    ) -> Result<T, Interruption> {
        let events = self.on_domain("Page").merge(self.on_domain("Debugger"));
        // The outcome is not held across the naming's await: a tool's error
        // (a possible `T`) is not `Send`.
        let location = match unless_interrupted(work, events, call).await {
            Err(Interruption::Paused(Some(location))) => location,
            other => return other,
        };
        Err(Interruption::Paused(Some(self.named(location).await)))
    }

    /// `location` with its script's URL from the tab's parsed scripts.
    async fn named(&self, location: PauseLocation) -> PauseLocation {
        match &self.debugger {
            Some(state) => with_script_url(location, &*state.lock().await),
            None => location,
        }
    }

    pub(crate) fn on_domain(&self, domain: &'static str) -> EventFilter {
        match &self.connection {
            Connection::Client(c) => c.on_domain(domain),
            Connection::Tab(t) => t.on_domain(domain),
        }
    }
}

/// How long a pause must last to interrupt a command: the debugger listener
/// resumes the pauses a tab does not keep (a page's own `debugger;` while it
/// has no breakpoint, see `debugger::start_debugger_listener`) within a
/// round trip or two, and the command then answers.
const PAUSE_SETTLE: Duration = Duration::from_millis(200);

/// What an event of the target says about a pending command.
enum Blocker {
    /// A JavaScript dialog opened.
    Dialog(OpenDialog),
    /// The page paused in the debugger (where, when the event says).
    Paused(Option<PauseLocation>),
    /// The page runs again.
    Resumed,
}

/// The blocker `event` announces; pauses and resumes only when `pauses`.
fn blocker(event: &CdpResult<WsResponse>, pauses: bool) -> Option<Blocker> {
    let event = event.as_ref().ok()?;
    let params = event.params.as_ref();
    match event.method.as_deref()? {
        "Page.javascriptDialogOpening" => {
            params.map(|p| Blocker::Dialog(OpenDialog::from_chrome(p)))
        }
        "Debugger.paused" if pauses => Some(Blocker::Paused(
            params.and_then(super::debugger::pause_location),
        )),
        "Debugger.resumed" if pauses => Some(Blocker::Resumed),
        _ => None,
    }
}

/// Runs `command` (CDP `method`, or a tool's whole `call`) unless `events`
/// (the `Page` and `Debugger` events of its target) announce a dialog, or a
/// debugger pause that is still there after [`PAUSE_SETTLE`], first: then
/// that is returned.
async fn unless_interrupted<T>(
    command: impl Future<Output = T>,
    mut events: impl Stream<Item = CdpResult<WsResponse>> + Unpin,
    method: &str,
) -> Result<T, Interruption> {
    // Debugger commands answer while paused, and a step pauses on purpose.
    let pauses = !method.starts_with("Debugger.");
    tokio::pin!(command);
    loop {
        let paused = match unless_blocked(&mut command, &mut events, |e| blocker(e, pauses)).await {
            Ok(reply) => return Ok(reply),
            Err(Blocker::Dialog(opened)) => return Err(Interruption::Dialog(opened)),
            Err(Blocker::Resumed) => continue,
            Err(Blocker::Paused(location)) => location,
        };
        let settle = tokio::time::timeout(PAUSE_SETTLE, &mut command);
        return match unless_blocked(settle, &mut events, |e| blocker(e, true)).await {
            Ok(Ok(reply)) => Ok(reply),
            Err(Blocker::Dialog(opened)) => Err(Interruption::Dialog(opened)),
            Err(Blocker::Resumed) => continue,
            Err(Blocker::Paused(next)) => Err(Interruption::Paused(next)),
            Ok(Err(_elapsed)) => Err(Interruption::Paused(paused)),
        };
    }
}

/// The error of CDP `method`, which `interruption` left unfinished.
fn interrupted_error(interruption: &Interruption, method: &str) -> cdp_browser_lite::CdpError {
    cdp_browser_lite::CdpError::ProtocolError {
        code: crate::handler::dialogs::INTERRUPTED_ERROR_CODE,
        message: interruption.during(method),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::cdp_domains::tests::make_event;
    use serde_json::json;

    /// `Debugger.paused` at 0-based line 3, column 2 of script 35, without
    /// a URL in the call frame (Chrome 156 leaves it empty).
    fn paused() -> CdpResult<WsResponse> {
        let frame = json!({"callFrameId": "f1", "url": "", "location": {"scriptId": "35", "lineNumber": 3, "columnNumber": 2}});
        Ok(make_event(
            "Debugger.paused",
            json!({"reason": "other", "hitBreakpoints": ["bp"], "callFrames": [frame]}),
        ))
    }

    fn resumed() -> CdpResult<WsResponse> {
        Ok(make_event("Debugger.resumed", json!({})))
    }

    /// The error text `send_raw_command` gives an interrupted command.
    fn message(outcome: Result<CdpResult<u32>, Interruption>) -> String {
        let Err(interruption) = outcome else {
            panic!("expected an interruption, got {outcome:?}");
        };
        let error = interrupted_error(&interruption, "Input.dispatchMouseEvent");
        assert!(crate::handler::dialogs::interrupted(&error));
        let cdp_browser_lite::CdpError::ProtocolError { message, .. } = error else {
            unreachable!("an interruption is a protocol error");
        };
        message
    }

    /// The reply of a command nothing interrupted.
    fn answered(outcome: Result<CdpResult<u32>, Interruption>) -> Option<u32> {
        outcome.ok().and_then(Result::ok)
    }

    /// ISSUE-1: a pause the command's own code hits (a click on a button
    /// whose handler has a breakpoint) fails the command at once, saying
    /// where the page stopped, instead of after the 10 s command timeout.
    #[tokio::test]
    async fn pause_interrupts_a_blocked_command() {
        // Given a command Chrome will not answer and a pause that stays
        let blocked = std::future::pending::<CdpResult<u32>>();
        let events = tokio_stream::iter([paused()]);
        // When it runs
        let run = unless_interrupted(blocked, events, "Input.dispatchMouseEvent");
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(2), run).await;
        // Then it fails within the settle window, naming the location
        let message = message(outcome.expect("the command waited for the pause"));
        assert!(
            message.starts_with("The page paused in the debugger at script 35 line 3, column 2 (0-based) while running Input.dispatchMouseEvent:"),
            "{message}"
        );
    }

    /// ISSUE-1: a pause the debugger listener resumes at once (a page's own
    /// `debugger;` while the tab has no breakpoint) does not fail the
    /// command, which answers once the page runs again.
    #[tokio::test]
    async fn resumed_pause_keeps_the_command() {
        // Given a pause followed by its resume, and a command answering later
        let events = tokio_stream::iter([paused(), resumed()]);
        let later = async {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            Ok(7)
        };
        // When it runs
        let outcome = unless_interrupted(later, events, "Input.dispatchMouseEvent").await;
        // Then its result is kept
        assert_eq!(answered(outcome), Some(7));
    }

    /// ISSUE-1: debugger commands are not interrupted by a pause (a step
    /// pauses on purpose; `step_over` waits for it itself).
    #[tokio::test]
    async fn debugger_commands_ignore_pauses() {
        let events = tokio_stream::iter([paused()]);
        let later = async {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            Ok(7)
        };
        let outcome = unless_interrupted(later, events, "Debugger.stepOver").await;
        assert_eq!(answered(outcome), Some(7));
    }

    /// ISSUE-10: a pause that interrupts a call names its script from the
    /// tab's parsed scripts (Chrome leaves the frame's URL empty).
    #[tokio::test]
    async fn interrupting_pause_names_its_script() {
        // Given a tab that parsed script 35 from http://a/cart.js
        let mock = crate::handler::cdp_domains::tests::spawn_recording_mock().await;
        let handler = crate::handler::cdp_domains::tests::mock_handler(mock.http_port);
        let session = handler.session(None).await.unwrap();
        let target = session.target(None).await.unwrap();
        let parsed = json!({"scriptId": "35", "hash": "h", "url": "http://a/cart.js"});
        let state = session.debugger_state(None).unwrap();
        crate::handler::cdp_domains::debugger::process_debugger_event(
            &make_event("Debugger.scriptParsed", parsed),
            &state,
        )
        .await;
        // When a call the page will not answer is interrupted by a pause in it
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            let paused = paused().unwrap().params.unwrap();
            mock.emit("Debugger.paused", paused, None);
        });
        let blocked = std::future::pending::<()>();
        let run = target.race_interruptions("click_element", blocked);
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(2), run).await;
        // Then the pause has the script's URL and id
        let Ok(Err(Interruption::Paused(Some(at)))) = outcome else {
            panic!("expected a pause, got {outcome:?}");
        };
        assert_eq!(at.url.as_deref(), Some("http://a/cart.js"));
        assert_eq!(at.script_id.as_deref(), Some("35"));
    }

    /// AUD-EV-05: a dialog still interrupts a command, quoting it.
    #[tokio::test]
    async fn dialog_still_interrupts_a_command() {
        let blocked = std::future::pending::<CdpResult<u32>>();
        let dialog = make_event(
            "Page.javascriptDialogOpening",
            json!({"type": "confirm", "message": "Delete?"}),
        );
        let events = tokio_stream::iter([Ok(dialog)]);
        let outcome = unless_interrupted(blocked, events, "Input.dispatchMouseEvent").await;
        assert!(message(outcome).contains("confirm dialog (\"Delete?\")"));
    }
}
