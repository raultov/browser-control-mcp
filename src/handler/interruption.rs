//! What stops the page from answering a call: a JavaScript dialog it
//! opened, or a debugger pause its code hit (both browsers); on Chrome also
//! its renderer crashing, or its tab closing (ISSUE-102). A call
//! whose answer is missing fails with it ([`Interruption::during`]); an
//! input action whose effect happened (a click) succeeds and says so
//! ([`Interruption::note`], [`Interruption::to_json`]).

use crate::handler::debugging::{self, PauseLocation};
use crate::handler::dialogs::OpenDialog;
use serde_json::{Value, json};

/// Why the page stopped answering.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Interruption {
    /// It opened a JavaScript dialog.
    Dialog(OpenDialog),
    /// It paused in the debugger (where, when the browser says).
    Paused(Option<PauseLocation>),
    /// Its renderer process died (Chrome's `Inspector.targetCrashed`).
    Crashed,
    /// Its tab closed (Chrome's `Inspector.detached`): a page that closed
    /// itself, for example with `window.close()`.
    Closed,
}

/// Why a tool does not act on a tab whose page crashed (ISSUE-102: it
/// timed out after 10-30 s with unrelated errors).
pub(crate) const PAGE_CRASHED: &str = "The tab's page crashed (its renderer process is gone): call reload, or navigate, to load it again.";

impl Interruption {
    /// The error of `call`, which the interruption left unfinished.
    pub(crate) fn during(&self, call: &str) -> String {
        match self {
            Self::Dialog(dialog) => dialog.opened_during(call),
            Self::Paused(location) => debugging::paused_during(location.as_ref(), call),
            Self::Crashed => format!(
                "The tab's page crashed (its renderer process is gone) while running {call}: call reload, or navigate, to load it again."
            ),
            Self::Closed => format!("The tab closed while running {call}."),
        }
    }

    /// The sentence a reply adds after an action it interrupted.
    pub(crate) fn note(&self) -> String {
        match self {
            Self::Dialog(dialog) => dialog.opened_note(),
            Self::Paused(location) => debugging::paused_note(location.as_ref()),
            Self::Crashed => "The tab's page crashed during the action (its renderer process is gone): call reload, or navigate, to load it again.".to_string(),
            Self::Closed => "The tab closed during the action.".to_string(),
        }
    }

    /// The reply text of an action (`done`, its first words) it interrupted:
    /// `<done>. <note>` and the JSON on the next line.
    pub(crate) fn after(&self, done: &str) -> String {
        let body = serde_json::to_string_pretty(&self.to_json()).unwrap_or_default();
        format!("{done}. {}\n{body}", self.note())
    }

    /// `reply` (an action's reply text) with the note and JSON of this
    /// interruption, which came after the action answered: worded as
    /// [`Self::after`] for a one-line reply, appended to a longer one.
    pub(crate) fn appended_to(&self, reply: &str) -> String {
        if !reply.contains('\n') {
            return self.after(reply);
        }
        let body = serde_json::to_string_pretty(&self.to_json()).unwrap_or_default();
        format!("{reply}\n{}\n{body}", self.note())
    }

    /// `{dialog}` or `{paused, location}` (the `step_over` shape) for replies.
    pub(crate) fn to_json(&self) -> Value {
        match self {
            Self::Dialog(dialog) => json!({ "dialog": dialog.to_json() }),
            Self::Paused(location) => {
                json!({"paused": true, "location": location.as_ref().map(PauseLocation::to_json)})
            }
            Self::Crashed => json!({"crashed": true}),
            Self::Closed => json!({"closed": true}),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn confirm() -> Interruption {
        Interruption::Dialog(OpenDialog::from_chrome(
            &json!({"type": "confirm", "message": "Delete account?"}),
        ))
    }

    fn breakpoint() -> Interruption {
        Interruption::Paused(Some(PauseLocation {
            url: Some("http://a/cart.js".into()),
            script_id: None,
            line: 3,
            column: 2,
        }))
    }

    /// ISSUE-11: an action a dialog interrupted notes the dialog and how to
    /// close it, and carries it as JSON; a call missing its answer fails
    /// with the same text as before.
    #[test]
    fn dialog_note_and_json() {
        // Given a confirm the action opened
        let dialog = confirm();
        // Then the note, the JSON and the error name it
        assert_eq!(
            dialog.note(),
            "The page opened a JavaScript confirm dialog (\"Delete account?\"), which blocks it: call handle_dialog with accept true or false to close it."
        );
        assert_eq!(
            dialog.to_json(),
            json!({"dialog": {"type": "confirm", "message": "Delete account?"}})
        );
        assert!(
            dialog
                .during("Input.dispatchMouseEvent")
                .starts_with("The page opened a JavaScript confirm dialog (\"Delete account?\") while running Input.dispatchMouseEvent")
        );
    }

    /// ISSUE-11: an action a pause interrupted notes where, 0-based, and
    /// carries it in the `step_over` shape.
    #[test]
    fn pause_note_and_json() {
        let paused = breakpoint();
        assert_eq!(
            paused.note(),
            "The page paused in the debugger at http://a/cart.js line 3, column 2 (0-based): inspect the paused frame with evaluate_on_call_frame, then continue with resume or step_over."
        );
        assert_eq!(
            paused.to_json(),
            json!({"paused": true, "location": {"url": "http://a/cart.js", "line_number": 3, "column_number": 2}})
        );
        assert!(
            paused
                .during("click_element")
                .starts_with("The page paused in the debugger at http://a/cart.js line 3, column 2 (0-based) while running click_element:")
        );
        assert_eq!(
            Interruption::Paused(None).to_json(),
            json!({"paused": true, "location": null})
        );
    }
}
