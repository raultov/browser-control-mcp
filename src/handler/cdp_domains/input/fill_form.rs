use crate::handler::McpHandler;
use crate::handler::cdp_domains::cdp_target::CdpTarget;
use rust_mcp_sdk::{
    macros,
    schema::{CallToolError, CallToolRequestParams, CallToolResult},
};
use serde_json::{Value, json};

#[macros::mcp_tool(
    name = "fill_form",
    description = "Fills several fields in one call, in order: text fields and <select>s as 'fill_input' does; a checkbox or radio takes 'true' or 'false' and is clicked only when its state differs. Returns each field's outcome (filled, selected, checked, unchecked, unchanged), never the values. Stops at the first failing field, naming it and what was done."
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct FillFormTool {
    /// Instance id; omit for the default instance.
    pub instance_id: Option<String>,
    /// Tab id; omit for the active tab.
    pub tab_id: Option<String>,
    /// 1 to 50 fields, filled in order.
    pub fields: Vec<FormField>,
}

/// One field of `fill_form`.
#[derive(Debug, Clone, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct FormField {
    /// Element: CSS selector, 'host >>> inner' (inside an open shadow root or same-origin iframe), or a snapshot ref (e12).
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
        let target =
            crate::handler::resolve_target(handler, args.instance_id.clone(), args.tab_id.clone())
                .await?;
        args.fill_all(&target).await
    }
}

/// Fills one field; what happened to it.
async fn fill_one(ops: &impl FormOps, field: &FormField) -> Result<String, CallToolError> {
    let selector = &field.selector;
    let kind = ops.snippet(&crate::util::js::form_field(selector)).await?;
    match kind["status"].as_str() {
        Some("box") => {
            let wanted = checked(&field.value, selector)?;
            if kind["checked"].as_bool() == Some(wanted) {
                return Ok("unchanged".into());
            }
            let reply = ops.click(selector).await?;
            let outcome = if wanted { "checked" } else { "unchecked" };
            Ok(with_note(outcome, &reply))
        }
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

/// `outcome`, plus what the click's reply noted (a dialog it opened).
fn with_note(outcome: &str, reply: &CallToolResult) -> String {
    let text = text_of(reply);
    let first = text.lines().next().unwrap_or_default();
    // The click reply is "... at coordinates (x, y). <note>".
    match first.split_once("). ") {
        Some((_, note)) => format!("{outcome}; {note}"),
        None => outcome.to_string(),
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
    /// (checkboxes with their state); it records the fills and clicks.
    #[derive(Default)]
    struct Page {
        boxes: Vec<(&'static str, bool)>,
        missing: Vec<&'static str>,
        calls: RefCell<Vec<String>>,
    }

    impl FormOps for Page {
        async fn snippet(&self, expression: &str) -> Result<Value, CallToolError> {
            let named = |s: &&str| expression.contains(&format!("resolve(\"{s}\")"));
            if self.missing.iter().any(named) {
                return Ok(json!({"status": "missing"}));
            }
            Ok(match self.boxes.iter().find(|(s, _)| named(s)) {
                Some((_, checked)) => json!({"status": "box", "checked": checked}),
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
            Ok(CallToolResult::text_content(vec![
                "Successfully clicked".into(),
            ]))
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

    /// ISSUE-5: a click's note (a dialog it opened) reaches the outcome.
    #[test]
    fn click_notes_reach_the_outcome() {
        let reply = CallToolResult::text_content(vec![
            "Successfully clicked on element '#t' at coordinates (1, 2). The page opened a JavaScript alert dialog (\"Hi\"), which blocks it: call handle_dialog with accept true or false to close it.\n{}".into(),
        ]);
        let outcome = with_note("checked", &reply);
        assert!(
            outcome.starts_with("checked; The page opened a JavaScript alert dialog"),
            "{outcome}"
        );
        let plain = CallToolResult::text_content(vec![
            "Successfully clicked on element '#t' at coordinates (1, 2)".into(),
        ]);
        assert_eq!(with_note("checked", &plain), "checked");
    }
}
