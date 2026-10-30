//! JavaScript dialogs (`alert`, `confirm`, `prompt`, `beforeunload`), the
//! same on both browsers. An open dialog blocks its page: a command that
//! runs page code (evaluate, input, navigation) waits until it is closed.
//! The tools say so at once, quoting the dialog, instead of waiting for a
//! timeout, and `handle_dialog` closes it.

use serde_json::{Value, json};
use std::future::Future;
use tokio_stream::{Stream, StreamExt};

/// Longest part of a dialog's message quoted in an error.
const QUOTED_CHARS: usize = 200;

/// The error of `handle_dialog` when no dialog is open.
pub(crate) const NO_DIALOG: &str = "No JavaScript dialog is open on this tab.";

/// The CDP error code under which a command interrupted by a dialog or a
/// debugger pause is reported (outside the codes Chrome uses), so its
/// message is shown as is.
pub(crate) const INTERRUPTED_ERROR_CODE: i64 = -32_098;

/// Whether a CDP command failed because the page opened a dialog or paused
/// in the debugger.
pub(crate) fn interrupted(error: &cdp_browser_lite::CdpError) -> bool {
    matches!(error, cdp_browser_lite::CdpError::ProtocolError { code, .. } if *code == INTERRUPTED_ERROR_CODE)
}

/// An open JavaScript dialog.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct OpenDialog {
    /// `alert`, `confirm`, `prompt` or `beforeunload`.
    pub(crate) kind: String,
    /// The text the page shows.
    pub(crate) message: String,
    /// A `prompt`'s default answer.
    pub(crate) default_prompt: Option<String>,
}

/// The dialog of one tab, kept from the browser's events.
#[derive(Debug, Default)]
pub(crate) struct DialogState {
    /// The dialog open now, if any.
    pub(crate) open: Option<OpenDialog>,
}

impl OpenDialog {
    /// From Chrome's `Page.javascriptDialogOpening`.
    pub(crate) fn from_chrome(params: &Value) -> Self {
        Self::from_params(params, "defaultPrompt")
    }

    /// From Firefox's `browsingContext.userPromptOpened`.
    pub(crate) fn from_firefox(params: &Value) -> Self {
        Self::from_params(params, "defaultValue")
    }

    fn from_params(params: &Value, default_key: &str) -> Self {
        let text = |key: &str| params.get(key).and_then(Value::as_str).map(str::to_string);
        Self {
            kind: text("type").unwrap_or_else(|| "alert".into()),
            message: text("message").unwrap_or_default(),
            default_prompt: text(default_key),
        }
    }

    /// The message, quoted and cut to a readable length.
    fn quoted(&self) -> String {
        let mut text: String = self.message.chars().take(QUOTED_CHARS).collect();
        if self.message.chars().count() > QUOTED_CHARS {
            text.push_str("...");
        }
        serde_json::Value::String(text).to_string()
    }

    /// The error of a call made while this dialog is open.
    pub(crate) fn blocking(&self) -> String {
        format!(
            "A JavaScript {} dialog is open on this tab ({}) and blocks the page. Call 'handle_dialog' with accept true or false to close it first.",
            self.kind,
            self.quoted()
        )
    }

    /// The error of a command the page interrupted by opening this dialog.
    pub(crate) fn opened_during(&self, method: &str) -> String {
        format!(
            "The page opened a JavaScript {} dialog ({}) while running {method}, and the dialog blocks the page: the call did not finish, though its effect (a click, a navigation) may have happened. Call 'handle_dialog' with accept true or false to close it.",
            self.kind,
            self.quoted()
        )
    }

    /// `{type, message, default_prompt?}` for replies.
    pub(crate) fn to_json(&self) -> Value {
        let mut out = json!({"type": self.kind, "message": self.message});
        if let Some(default) = &self.default_prompt {
            out["default_prompt"] = json!(default);
        }
        out
    }
}

/// Runs `command` unless `events` announces what blocks the page first
/// (`blocks` reads it from an event: a dialog, a debugger pause): then that
/// is returned and the command is abandoned, as the page will not answer it
/// until the dialog is closed or the pause resumed.
pub(crate) async fn unless_blocked<T, I, B>(
    command: impl Future<Output = T>,
    mut events: impl Stream<Item = I> + Unpin,
    blocks: impl Fn(&I) -> Option<B>,
) -> Result<T, B> {
    tokio::pin!(command);
    loop {
        tokio::select! {
            biased;
            out = &mut command => return Ok(out),
            event = events.next() => match event {
                Some(event) => {
                    if let Some(blocker) = blocks(&event) {
                        return Err(blocker);
                    }
                }
                None => return Ok(command.await),
            },
        }
    }
}

/// The reply of `handle_dialog`.
pub(crate) fn handled(
    accepted: bool,
    dialog: Option<&OpenDialog>,
) -> rust_mcp_sdk::schema::CallToolResult {
    let first = if accepted {
        "Dialog accepted."
    } else {
        "Dialog dismissed."
    };
    let body = json!({
        "accepted": accepted,
        "dialog": dialog.map(OpenDialog::to_json),
    });
    let body = serde_json::to_string_pretty(&body).unwrap_or_default();
    rust_mcp_sdk::schema::CallToolResult::text_content(vec![format!("{first}\n{body}").into()])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alert(event: &&str) -> Option<OpenDialog> {
        (*event == "dialog")
            .then(|| OpenDialog::from_chrome(&json!({"type": "alert", "message": "hi"})))
    }

    /// AUD-EV-05: a dialog announced before the command answers wins.
    #[tokio::test]
    async fn dialog_interrupts_a_blocked_command() {
        let blocked = std::future::pending::<u32>();
        let events = tokio_stream::iter(["other", "dialog"]);
        let dialog = unless_blocked(blocked, events, alert).await.unwrap_err();
        assert_eq!(dialog.kind, "alert");
        assert!(
            dialog
                .opened_during("Runtime.evaluate")
                .contains("(\"hi\") while running Runtime.evaluate"),
            "{}",
            dialog.opened_during("Runtime.evaluate")
        );
    }

    /// AUD-EV-05: a command that answers first keeps its result, also when
    /// the event stream ends.
    #[tokio::test]
    async fn answered_command_keeps_its_result() {
        let events = tokio_stream::iter(["dialog"]);
        assert_eq!(unless_blocked(async { 7 }, events, alert).await, Ok(7));
        let ended = tokio_stream::iter(Vec::<&str>::new());
        let slow = async {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            8
        };
        assert_eq!(unless_blocked(slow, ended, alert).await, Ok(8));
    }

    /// Long messages are cut; a prompt's default answer is kept.
    #[test]
    fn dialog_text_is_bounded() {
        let long = "x".repeat(500);
        let dialog = OpenDialog::from_firefox(
            &json!({"type": "prompt", "message": long, "defaultValue": "d"}),
        );
        assert!(dialog.blocking().len() < 400);
        assert_eq!(
            dialog.to_json(),
            json!({"type": "prompt", "message": "x".repeat(500), "default_prompt": "d"})
        );
    }
}
