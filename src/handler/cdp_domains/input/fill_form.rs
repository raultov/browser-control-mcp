use crate::handler::McpHandler;
use crate::handler::cdp_domains::cdp_target::CdpTarget;
use rust_mcp_sdk::{
    macros,
    schema::{CallToolError, CallToolRequestParams, CallToolResult},
};
use serde_json::{Value, json};

#[macros::mcp_tool(
    name = "fill_form",
    description = "Fills several fields in one call, in order: text fields and <select>s as fill_input does; a checkbox or radio takes 'true' or 'false' and is clicked only when its state differs. Returns each field's outcome; stops at the first field that fails, naming it."
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct FillFormTool {
    /// Default: 'default'.
    pub instance_id: Option<String>,
    /// Default: the active tab.
    pub tab_id: Option<String>,
    /// 1 to 50 fields, filled in order.
    pub fields: Vec<FormField>,
}

/// One field of `fill_form`.
#[derive(Debug, Clone, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct FormField {
    /// A snapshot ref (e12) or a CSS selector.
    pub selector: String,
    /// Text, option value or label, or 'true' / 'false' for a checkbox or radio.
    pub value: String,
}

/// Most fields one call fills.
const MAX_FIELDS: usize = 50;

/// What `fill_form` needs from a browser for each field.
pub(crate) trait FormOps {
    /// Evaluates a `util::js` snippet, by value.
    async fn snippet(&self, expression: &str) -> Result<Value, CallToolError>;
    /// What `fill_input` does to `selector`.
    async fn fill(&self, selector: &str, text: &str) -> Result<CallToolResult, CallToolError>;
    /// What `click_element` does to `selector`.
    async fn click(&self, selector: &str) -> Result<CallToolResult, CallToolError>;
}

impl FormOps for CdpTarget {
    async fn snippet(&self, expression: &str) -> Result<Value, CallToolError> {
        crate::handler::snippet_value(self, expression).await
    }

    async fn fill(&self, selector: &str, text: &str) -> Result<CallToolResult, CallToolError> {
        let mode = crate::util::js::FillMode::Replace;
        super::fill_input::fill(self, selector, text, mode).await
    }

    async fn click(&self, selector: &str) -> Result<CallToolResult, CallToolError> {
        let once = super::click_element::ClickOptions::default();
        super::click_element::click(self, selector, &once).await
    }
}

impl FillFormTool {
    /// Checks the arguments (before any browser work).
    pub(crate) fn check_arguments(&self) -> Result<(), CallToolError> {
        if (1..=MAX_FIELDS).contains(&self.fields.len()) {
            return Ok(());
        }
        Err(CallToolError::from_message(format!(
            "fill_form takes 1 to {MAX_FIELDS} fields, got {}",
            self.fields.len()
        )))
    }

    /// Fills every field in order through `ops`, stopping at the first
    /// failure (shared by every browser).
    pub(crate) async fn fill_all(
        &self,
        ops: &impl FormOps,
    ) -> Result<CallToolResult, CallToolError> {
        let mut done = Vec::new();
        for (index, field) in self.fields.iter().enumerate() {
            match fill_one(ops, field).await {
                Ok(outcome) => done.push(json!({"selector": field.selector, "outcome": outcome})),
                Err(error) => return Err(self.failed(index, &error, &done)),
            }
        }
        let body = serde_json::to_string_pretty(&json!({ "fields": done })).unwrap_or_default();
        Ok(CallToolResult::text_content(vec![
            format!("Filled {} fields.\n{body}", done.len()).into(),
        ]))
    }

    pub async fn handle(
        params: CallToolRequestParams,
        handler: &McpHandler,
    ) -> Result<CallToolResult, CallToolError> {
        let args: FillFormTool = crate::handler::parse_args(params)?;
        args.check_arguments()?;
        let at =
            crate::handler::frames::one_frame(args.fields.iter().map(|f| f.selector.as_str()))?;
        let target = crate::handler::resolve_target_at(
            handler,
            args.instance_id.clone(),
            args.tab_id.clone(),
            at,
        )
        .await?;
        args.fill_all(&target).await
    }
}

/// Fills one field; what happened to it.
async fn fill_one(ops: &impl FormOps, field: &FormField) -> Result<String, CallToolError> {
    let selector = &field.selector;
    let kind = ops.snippet(&crate::util::js::form_field(selector)).await?;
    match kind["status"].as_str() {
        Some("box") => set_box(ops, field, &kind).await,
        Some("field") => {
            let reply = text_of(&ops.fill(selector, &field.value).await?);
            let outcome = if reply.starts_with("Successfully selected") {
                "selected"
            } else if reply.contains("\nNote:") {
                "filled, reformatted by the page"
            } else {
                "filled"
            };
            Ok(outcome.into())
        }
        _ => Err(CallToolError::from_message(format!(
            "Element not found for selector: {selector}{}",
            crate::util::js::ref_hint(selector)
        ))),
    }
}

/// A checkbox's wanted state from its value.
fn checked(value: &str, selector: &str) -> Result<bool, CallToolError> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "on" | "yes" | "checked" => Ok(true),
        "false" | "off" | "no" | "unchecked" => Ok(false),
        other => Err(CallToolError::from_message(format!(
            "'{selector}' is a checkbox or radio: its value must be 'true' or 'false', got '{other}'"
        ))),
    }
}

fn text_of(reply: &CallToolResult) -> String {
    reply
        .content
        .first()
        .and_then(|c| c.as_text_content().ok())
        .map(|t| t.text.clone())
        .unwrap_or_default()
}

/// What the click's reply noted (a dialog it opened), if anything.
fn note_of(reply: &CallToolResult) -> Option<String> {
    let text = text_of(reply);
    let first = text.lines().next().unwrap_or_default();
    // The click reply is "... at coordinates (x, y). <note>".
    first.split_once("). ").map(|(_, note)| note.to_string())
}

/// Brings a checkbox or radio (`found`: what `util::js::form_field` read)
/// to the state `field` asks for, with one click when it differs. The
/// outcome is the state the box has once clicked, read again, never the
/// wanted one: a click leaves a radio checked, and a click handler may
/// cancel the change (#215).
async fn set_box(
    ops: &impl FormOps,
    field: &FormField,
    found: &Value,
) -> Result<String, CallToolError> {
    let selector = &field.selector;
    let wanted = checked(&field.value, selector)?;
    if found["checked"].as_bool() == Some(wanted) {
        return Ok("unchanged".into());
    }
    if found["radio"] == true && !wanted {
        return Err(CallToolError::from_message(format!(
            "'{selector}' is a checked radio, which a click does not uncheck (check another radio of its group instead); it was not clicked and is still checked."
        )));
    }
    let reply = ops.click(selector).await?;
    if let Some(note) = note_of(&reply) {
        // What the click opened (a dialog, a pause) blocks the page.
        return Ok(format!("clicked, state not read; {note}"));
    }
    let after = ops.snippet(&crate::util::js::form_field(selector)).await?;
    clicked_outcome(&after, wanted, selector)
}

/// The outcome of a clicked box from its state `after` the click: an error
/// unless it is the `wanted` one.
fn clicked_outcome(after: &Value, wanted: bool, selector: &str) -> Result<String, CallToolError> {
    let state = |checked| if checked { "checked" } else { "unchecked" };
    match after["checked"].as_bool() {
        Some(now) if now == wanted => Ok(state(now).into()),
        Some(now) => Err(CallToolError::from_message(format!(
            "'{selector}' was clicked, but the page kept it {} (its click handler may cancel the change).",
            state(now)
        ))),
        None => Err(CallToolError::from_message(format!(
            "'{selector}' disappeared after the click (the page replaced it), so its state could not be read."
        ))),
    }
}

impl FillFormTool {
    /// The error of field `index` (0-based), saying what was done before it.
    fn failed(&self, index: usize, error: &CallToolError, done: &[Value]) -> CallToolError {
        let before = if done.is_empty() {
            "No field was filled before it.".to_string()
        } else {
            format!(
                "Done before it: {}.",
                serde_json::to_string(done).unwrap_or_default()
            )
        };
        let selector = self.fields.get(index).map_or("", |f| f.selector.as_str());
        CallToolError::from_message(format!(
            "Field {} of {} ('{selector}') failed: {error} {before}",
            index + 1,
            self.fields.len()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// A page whose fields are text boxes, except those named in `boxes`
    /// (checkboxes with their state, which a click changes); it records the
    /// fills and clicks.
    #[derive(Default)]
    struct Page {
        boxes: Vec<(&'static str, bool)>,
        /// The boxes that are radios: a click checks one, never unchecks it.
        radios: Vec<&'static str>,
        /// The boxes whose click handler cancels the change.
        kept: Vec<&'static str>,
        /// The boxes whose click opens a dialog.
        alerts: Vec<&'static str>,
        missing: Vec<&'static str>,
        calls: RefCell<Vec<String>>,
    }

    impl Page {
        /// The state of the box `selector`, which started as `checked`.
        fn state(&self, selector: &str, checked: bool) -> Value {
            let click = format!("click {selector}");
            let clicked = self.calls.borrow().contains(&click) && !self.kept.contains(&selector);
            let radio = self.radios.contains(&selector);
            let now = if radio {
                checked || clicked
            } else {
                checked != clicked
            };
            json!({"status": "box", "checked": now, "radio": radio})
        }
    }

    impl FormOps for Page {
        async fn snippet(&self, expression: &str) -> Result<Value, CallToolError> {
            let named = |s: &&str| expression.contains(&format!("resolve(\"{s}\")"));
            if self.missing.iter().any(named) {
                return Ok(json!({"status": "missing"}));
            }
            Ok(match self.boxes.iter().find(|(s, _)| named(s)) {
                Some((selector, checked)) => self.state(selector, *checked),
                None => json!({"status": "field"}),
            })
        }

        async fn fill(&self, selector: &str, _: &str) -> Result<CallToolResult, CallToolError> {
            self.calls.borrow_mut().push(format!("fill {selector}"));
            Ok(CallToolResult::text_content(vec![
                "Successfully filled".into(),
            ]))
        }

        async fn click(&self, selector: &str) -> Result<CallToolResult, CallToolError> {
            self.calls.borrow_mut().push(format!("click {selector}"));
            let done = "Successfully clicked on element at coordinates (1, 2)";
            let reply = match self.alerts.contains(&selector) {
                true => format!("{done}. The page opened a JavaScript alert dialog.\n{{}}"),
                false => done.to_string(),
            };
            Ok(CallToolResult::text_content(vec![reply.into()]))
        }
    }

    fn form(fields: &[(&str, &str)]) -> FillFormTool {
        let fields: Vec<Value> = fields
            .iter()
            .map(|(s, v)| json!({"selector": s, "value": v}))
            .collect();
        serde_json::from_value(json!({ "fields": fields })).unwrap()
    }

    /// ISSUE-5: text fields are filled and a checkbox is clicked only when
    /// its state differs; the reply lists outcomes, never values.
    #[tokio::test]
    async fn fields_are_filled_in_order() {
        // Given a login form with an unchecked and a checked box
        let page = Page {
            boxes: vec![("#terms", false), ("#news", true)],
            ..Page::default()
        };
        let tool = form(&[
            ("#user", "alice"),
            ("#pass", "s3cret"),
            ("#terms", "true"),
            ("#news", "true"),
        ]);
        // When it is filled
        let reply = tool.fill_all(&page).await.unwrap();
        // Then each field got its tool, in order, and the reply has no value
        assert_eq!(
            *page.calls.borrow(),
            ["fill #user", "fill #pass", "click #terms"]
        );
        let text = text_of(&reply);
        assert!(text.starts_with("Filled 4 fields.\n"), "{text}");
        assert!(
            !text.contains("s3cret") && !text.contains("alice"),
            "{text}"
        );
        let body: Value = serde_json::from_str(&text[text.find('{').unwrap()..]).unwrap();
        let outcomes: Vec<&str> = body["fields"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["outcome"].as_str().unwrap())
            .collect();
        assert_eq!(outcomes, ["filled", "filled", "checked", "unchanged"]);
    }

    /// ISSUE-5: the first failing field stops the fill, naming it and what
    /// was done before; a checkbox needs true or false.
    #[tokio::test]
    async fn first_failure_stops_and_says_what_was_done() {
        let page = Page {
            missing: vec!["#country"],
            ..Page::default()
        };
        let tool = form(&[("#user", "alice"), ("#country", "Malta"), ("#zip", "1")]);
        let error = tool.fill_all(&page).await.unwrap_err().to_string();
        assert!(
            error.starts_with("Field 2 of 3 ('#country') failed: Element not found"),
            "{error}"
        );
        assert!(error.contains(r##""selector":"#user""##), "{error}");
        assert_eq!(*page.calls.borrow(), ["fill #user"]);
        let boxed = Page {
            boxes: vec![("#terms", false)],
            ..Page::default()
        };
        let error = form(&[("#terms", "maybe")])
            .fill_all(&boxed)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("must be 'true' or 'false', got 'maybe'"),
            "{error}"
        );
    }

    /// ISSUE-5: 1 to 50 fields.
    #[test]
    fn field_count_is_bounded() {
        assert!(form(&[]).check_arguments().is_err());
        let many: Vec<(&str, &str)> = vec![("#a", "x"); 51];
        assert!(
            form(&many)
                .check_arguments()
                .unwrap_err()
                .to_string()
                .contains("got 51")
        );
        assert!(form(&[("#a", "x")]).check_arguments().is_ok());
    }

    /// #215: a checked radio asked for `false` fails without a click (a
    /// click leaves a radio checked; it was reported "unchecked").
    #[tokio::test]
    async fn a_checked_radio_is_not_clicked_to_uncheck_it() {
        // Given a checked radio
        let page = Page {
            boxes: vec![("#r1", true)],
            radios: vec!["#r1"],
            ..Page::default()
        };
        // When it is asked to be unchecked
        let error = form(&[("#r1", "false")])
            .fill_all(&page)
            .await
            .unwrap_err()
            .to_string();
        // Then the field fails saying why, and the radio was not clicked
        assert!(
            error.starts_with("Field 1 of 1 ('#r1') failed: '#r1' is a checked radio"),
            "{error}"
        );
        assert!(
            error.contains("check another radio of its group"),
            "{error}"
        );
        assert!(error.contains("still checked"), "{error}");
        assert!(page.calls.borrow().is_empty());
    }

    /// #215: the outcome of a clicked box is its state in the page, read
    /// again after the click: a box whose click handler cancels the change
    /// fails saying the state it kept (it was reported "checked").
    #[tokio::test]
    async fn a_box_is_read_again_after_the_click() {
        // Given a box that takes the click, a radio, and a box that cancels it
        let page = Page {
            boxes: vec![("#news", true), ("#r2", false), ("#terms", false)],
            radios: vec!["#r2"],
            kept: vec!["#terms"],
            ..Page::default()
        };
        let tool = form(&[("#news", "false"), ("#r2", "true"), ("#terms", "true")]);
        // When they are filled
        let error = tool.fill_all(&page).await.unwrap_err().to_string();
        // Then the first two report the state the page holds, and the third
        // fails with the state it kept
        assert_eq!(
            *page.calls.borrow(),
            ["click #news", "click #r2", "click #terms"]
        );
        assert!(
            error.starts_with("Field 3 of 3 ('#terms') failed: '#terms' was clicked, but the page kept it unchecked"),
            "{error}"
        );
        assert!(
            error.contains(r##"{"outcome":"unchecked","selector":"#news"}"##),
            "{error}"
        );
        assert!(
            error.contains(r##"{"outcome":"checked","selector":"#r2"}"##),
            "{error}"
        );
    }

    /// #215: a box whose click opened a dialog cannot be read while the
    /// dialog blocks the page: the outcome says clicked, with the note, not
    /// a state.
    #[tokio::test]
    async fn a_box_behind_a_dialog_is_not_given_a_state() {
        // Given a box whose click handler opens an alert
        let page = Page {
            boxes: vec![("#terms", false)],
            alerts: vec!["#terms"],
            ..Page::default()
        };
        // When it is checked
        let reply = form(&[("#terms", "true")]).fill_all(&page).await.unwrap();
        // Then the outcome is the click and its note
        let text = text_of(&reply);
        assert!(
            text.contains("clicked, state not read; The page opened a JavaScript alert dialog."),
            "{text}"
        );
    }

    /// #215: a box the click removed from the page fails, as its state
    /// cannot be read; one the page kept checked says so.
    #[test]
    fn a_clicked_box_reports_the_state_it_has() {
        let outcome =
            |after: Value, wanted| clicked_outcome(&after, wanted, "#t").map_err(|e| e.to_string());
        let checked = json!({"status": "box", "checked": true, "radio": false});
        assert_eq!(outcome(checked.clone(), true).unwrap(), "checked");
        let kept = outcome(checked, false).unwrap_err();
        assert!(kept.contains("the page kept it checked"), "{kept}");
        let gone = outcome(json!({"status": "missing"}), true).unwrap_err();
        assert!(gone.contains("'#t' disappeared after the click"), "{gone}");
    }

    /// ISSUE-5: a click's note (a dialog it opened) reaches the outcome;
    /// #215: without a state, which a blocked page does not let read.
    #[test]
    fn click_notes_reach_the_outcome() {
        let reply = CallToolResult::text_content(vec![
            "Successfully clicked on element '#t' at coordinates (1, 2). The page opened a JavaScript alert dialog (\"Hi\"), which blocks it: call handle_dialog with accept true or false to close it.\n{}".into(),
        ]);
        let note = note_of(&reply).unwrap();
        assert!(
            note.starts_with("The page opened a JavaScript alert dialog"),
            "{note}"
        );
        let plain = CallToolResult::text_content(vec![
            "Successfully clicked on element '#t' at coordinates (1, 2)".into(),
        ]);
        assert_eq!(note_of(&plain), None);
    }
}
