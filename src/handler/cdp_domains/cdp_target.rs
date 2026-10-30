use crate::handler::debugging::PauseLocation;
use crate::handler::dialogs::{OpenDialog, unless_blocked};
use crate::handler::interruption::Interruption;
use cdp_browser_lite::{CdpClient, CdpResult, EventFilter, Tab, WsResponse};
use serde::Serialize;
use std::future::Future;
use std::time::Duration;
use tokio_stream::{Stream, StreamExt};

#[derive(Clone)]
pub(crate) enum CdpTarget {
    Client(CdpClient),
    Tab(Tab),
}

impl std::fmt::Debug for CdpTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Client(_) => f.write_str("CdpTarget::Client"),
            Self::Tab(t) => write!(f, "CdpTarget::Tab(target_id: {})", t.target_id()),
        }
    }
}

impl CdpTarget {
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
            match self {
                Self::Client(c) => c.send_raw_command(method, params).await,
                Self::Tab(t) => t.send_raw_command(method, params).await,
            }
        };
        if method == "Page.handleJavaScriptDialog" {
            return Ok(send.await);
        }
        let events = self.on_domain("Page").merge(self.on_domain("Debugger"));
        unless_interrupted(send, events, method).await
    }

    pub(crate) fn on_domain(&self, domain: &'static str) -> EventFilter {
        match self {
            Self::Client(c) => c.on_domain(domain),
            Self::Tab(t) => t.on_domain(domain),
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

/// Runs `command` (CDP `method`) unless `events` (the `Page` and `Debugger`
/// events of its target) announce a dialog, or a debugger pause that is
/// still there after [`PAUSE_SETTLE`], first: then that is returned.
async fn unless_interrupted<T>(
    command: impl Future<Output = CdpResult<T>>,
    mut events: impl Stream<Item = CdpResult<WsResponse>> + Unpin,
    method: &str,
) -> Result<CdpResult<T>, Interruption> {
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
            message.starts_with("The page paused in the debugger at line 3, column 2 (0-based) while running Input.dispatchMouseEvent:"),
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
