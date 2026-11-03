use crate::handler::McpHandler;
use rust_mcp_sdk::{
    macros,
    schema::{CallToolError, CallToolRequestParams, CallToolResult},
};
use serde_json::{Value, json};

#[macros::mcp_tool(
    name = "scroll",
    description = "Scrolls the window by x/y pixels or by viewport heights ('pages'), or brings 'selector' into view. Returns the window positions {before, after, element_in_view?} and says when nothing moved (on a page that scrolls an inner element, pass a 'selector' inside it). wheel: true dispatches a real mouse wheel event instead (x/y pixels, default 100 down) at 'selector' or the viewport's centre, for a canvas or map that listens to wheel events."
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct ScrollTool {
    /// Instance id; omit for the default instance.
    pub instance_id: Option<String>,
    /// Tab id; omit for the active tab.
    pub tab_id: Option<String>,
    /// Horizontal pixels (negative = left).
    pub x: Option<i32>,
    /// Vertical pixels (negative = up); ignored with 'pages'.
    pub y: Option<i32>,
    /// Viewport heights to scroll (e.g. 1.5; negative = up).
    pub pages: Option<f64>,
    /// Element to bring into view (CSS, 'host >>> inner' or a snapshot ref); overrides x, y and pages.
    pub selector: Option<String>,
    /// Dispatch a real mouse wheel event instead (deltaX = x, deltaY = y pixels; default 100 down) at 'selector', or at the viewport's centre, for a canvas or map that listens to `wheel`. Default: false.
    pub wheel: Option<bool>,
}

/// The viewport's centre, where a wheel without a `selector` turns.
pub(crate) const VIEWPORT_CENTRE: &str =
    "({ x: Math.round(innerWidth / 2), y: Math.round(innerHeight / 2) })";

impl ScrollTool {
    /// Whether a real wheel event is asked for (ISSUE-81).
    pub(crate) fn wheel(&self) -> bool {
        self.wheel == Some(true)
    }

    /// The wheel's `(deltaX, deltaY)`: `x` and `y`, 100 px down when
    /// neither is given; `pages` does not apply (a wheel turns by pixels).
    pub(crate) fn wheel_deltas(&self) -> Result<(i32, i32), CallToolError> {
        if self.pages.is_some() {
            return Err(CallToolError::from_message(
                "'pages' does not apply with wheel: give the wheel's turn in pixels with 'y' (and 'x').",
            ));
        }
        let down = if self.x.is_none() { 100 } else { 0 };
        Ok((self.x.unwrap_or(0), self.y.unwrap_or(down)))
    }

    /// The reply once a wheel event was dispatched at `point`.
    pub(crate) fn wheel_reply(&self, (x, y): (f64, f64), (dx, dy): (i32, i32)) -> CallToolResult {
        let at = self
            .selector
            .as_deref()
            .map_or_else(|| "the viewport's centre".to_string(), |s| format!("'{s}'"));
        CallToolResult::text_content(vec![
            format!("Dispatched a wheel event (deltaX {dx}, deltaY {dy}) at {at} ({x}, {y}).")
                .into(),
        ])
    }

    /// `scroll` with `wheel` on Chrome: `Input.dispatchMouseEvent`
    /// `mouseWheel` at the element's checked point, or the viewport's centre.
    async fn wheel_on_chrome(
        params: CallToolRequestParams,
        handler: &McpHandler,
    ) -> Result<CallToolResult, CallToolError> {
        let ids = |a: &Self| (a.instance_id.clone(), a.tab_id.clone());
        let (args, target) =
            crate::handler::args_and_target(params, handler, ids, |a: &Self| a.selector.clone())
                .await?;
        let deltas = args.wheel_deltas()?;
        let point = match &args.selector {
            Some(selector) => {
                let checked = crate::handler::cdp_domains::input::click_element::checked_point(
                    &target,
                    selector,
                    "scrolled with the wheel",
                );
                checked.await?
            }
            None => centre(&crate::handler::snippet_value(&target, VIEWPORT_CENTRE).await?),
        };
        let wheel = json!({"type": "mouseWheel", "x": point.0, "y": point.1, "deltaX": deltas.0, "deltaY": deltas.1});
        crate::handler::send_or(&target, "Input.dispatchMouseEvent", wheel, "CDP Error").await?;
        Ok(args.wheel_reply(point, deltas))
    }

    /// The JavaScript that performs the scroll (shared by every browser).
    /// Precedence: selector, then pages, then x/y.
    pub(crate) fn expression(&self) -> String {
        let dx = self.x.unwrap_or(0).to_string();
        if let Some(selector) = &self.selector {
            crate::util::js::scroll_into_view(selector)
        } else if let Some(pages) = self.pages {
            crate::util::js::scroll_by(&dx, &format!("window.innerHeight * {pages}"))
        } else {
            crate::util::js::scroll_by(&dx, &self.y.unwrap_or(0).to_string())
        }
    }

    /// The reply from the scroll snippet's outcome (shared by every browser):
    /// the window positions, and why nothing moved when it did not.
    pub(crate) fn reply(&self, outcome: &Value) -> Result<CallToolResult, CallToolError> {
        let selector = self.selector.as_deref().unwrap_or_default();
        match outcome["status"].as_str() {
            Some("done") => {}
            Some("hidden") => {
                return Err(CallToolError::from_message(format!(
                    "Element '{selector}' is not visible (no layout box); there is nothing to scroll to."
                )));
            }
            _ if self.selector.is_some() => {
                return Err(CallToolError::from_message(format!(
                    "Element not found for scrolling: {selector}{}",
                    crate::util::js::ref_hint(selector)
                )));
            }
            _ => {
                return Err(CallToolError::from_message(
                    "The page returned no scroll positions.",
                ));
            }
        }
        let mut body = json!({"before": outcome["before"], "after": outcome["after"]});
        let first = if let Some(in_view) = outcome["in_view"].as_bool() {
            body["element_in_view"] = json!(in_view);
            if in_view {
                "Successfully scrolled.".to_string()
            } else {
                format!(
                    "'{selector}' is still outside the viewport (a fixed or clipped container hides it)."
                )
            }
        } else if outcome["before"] == outcome["after"] {
            "The window did not move: it is already at that edge, or the page scrolls an inner element (pass a 'selector' inside it to bring that element into view).".to_string()
        } else {
            "Successfully scrolled.".to_string()
        };
        let body = serde_json::to_string_pretty(&body).unwrap_or_default();
        Ok(CallToolResult::text_content(vec![
            format!("{first}\n{body}").into(),
        ]))
    }

    pub async fn handle(
        params: CallToolRequestParams,
        handler: &McpHandler,
    ) -> Result<CallToolResult, CallToolError> {
        let args: Self = crate::handler::parse_args(params.clone())?;
        if args.wheel() {
            return Self::wheel_on_chrome(params, handler).await;
        }
        crate::handler::handle_snippet_tool::<Self>(params, handler).await
    }
}

/// The `(x, y)` of [`VIEWPORT_CENTRE`]'s answer.
pub(crate) fn centre(value: &Value) -> (f64, f64) {
    let at = |key: &str| value[key].as_f64().unwrap_or(0.0);
    (at("x"), at("y"))
}

impl crate::handler::SnippetTool for ScrollTool {
    fn at(&self) -> Option<&str> {
        self.selector.as_deref()
    }

    fn target_args(&self) -> crate::handler::TargetArgs {
        crate::handler::TargetArgs {
            instance_id: self.instance_id.clone(),
            tab_id: self.tab_id.clone(),
        }
    }

    fn snippet(&self) -> String {
        self.expression()
    }

    fn answer(&self, outcome: &Value) -> Result<CallToolResult, CallToolError> {
        self.reply(outcome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_mcp_sdk::schema::CallToolRequestParams;
    use serde_json::json;

    /// ISSUE-81 (Chrome): `wheel: true` dispatches a real wheel event at
    /// the element (a canvas or map listening to `wheel` got nothing from
    /// a scroll of the window); without deltas it scrolls 100 px down, and
    /// `pages` does not apply.
    #[tokio::test]
    async fn wheel_dispatches_a_wheel_event() {
        use crate::handler::cdp_domains::tests::{mock_handler, reply_text, spawn_recording_mock};
        let mock = spawn_recording_mock().await;
        mock.respond_with("Runtime.evaluate", |_| {
            let value = json!({"status": "ok", "x": 30, "y": 40});
            json!({"result": {"result": {"type": "object", "value": value}}})
        });
        let handler = mock_handler(mock.http_port);
        let wheel = |args: Value| {
            ScrollTool::handle(
                crate::handler::cdp_domains::tests::call("scroll", args),
                &handler,
            )
        };
        let reply = wheel(json!({"selector": "#map", "wheel": true, "y": 120}))
            .await
            .unwrap();
        let sent = mock.commands_named("Input.dispatchMouseEvent");
        assert_eq!(
            sent[0].params,
            json!({"type": "mouseWheel", "x": 30.0, "y": 40.0, "deltaX": 0, "deltaY": 120})
        );
        assert_eq!(
            reply_text(&reply),
            "Dispatched a wheel event (deltaX 0, deltaY 120) at '#map' (30, 40)."
        );
        wheel(json!({"wheel": true})).await.unwrap();
        let sent = mock.commands_named("Input.dispatchMouseEvent");
        assert_eq!(sent[1].params["deltaY"], 100);
        let error = wheel(json!({"wheel": true, "pages": 1}))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("'pages' does not apply"), "{error}");
    }

    #[tokio::test]
    async fn test_scroll_params_deserialization() {
        let params: Result<CallToolRequestParams, _> = serde_json::from_value(json!({
            "name": "scroll",
            "arguments": {
                "y": 500,
                "pages": 1.5,
                "selector": "#bottom"
            }
        }));
        assert!(params.is_ok());
    }

    /// A Chrome mock whose scroll snippet answers `outcome` (by value).
    async fn scroll_with(outcome: Value, args: Value) -> Result<CallToolResult, CallToolError> {
        let mock = crate::handler::cdp_domains::tests::spawn_recording_mock().await;
        mock.respond_with(
            "Runtime.evaluate",
            move |_| json!({"result": {"result": {"type": "object", "value": outcome.clone()}}}),
        );
        let handler = crate::handler::cdp_domains::tests::mock_handler(mock.http_port);
        let params = crate::handler::cdp_domains::tests::call("scroll", args);
        ScrollTool::handle(params, &handler).await
    }

    /// AUD-IN-15: a scroll that moved reports the positions before and after.
    #[tokio::test]
    async fn scroll_reports_positions() {
        let moved =
            json!({"status": "done", "before": {"x": 0, "y": 0}, "after": {"x": 0, "y": 500}});
        let reply = scroll_with(moved, json!({"y": 500})).await.unwrap();
        let text = crate::handler::cdp_domains::tests::reply_text(&reply);
        let (first, body) = text.split_once('\n').unwrap();
        assert_eq!(first, "Successfully scrolled.");
        let body: Value = serde_json::from_str(body).unwrap();
        assert_eq!(
            body,
            json!({"before": {"x": 0, "y": 0}, "after": {"x": 0, "y": 500}})
        );
    }

    /// AUD-IN-15: a window that did not move says so (it used to report
    /// success on pages that scroll an inner element).
    #[tokio::test]
    async fn scroll_that_did_not_move_says_so() {
        let still =
            json!({"status": "done", "before": {"x": 0, "y": 0}, "after": {"x": 0, "y": 0}});
        let reply = scroll_with(still, json!({"y": 500})).await.unwrap();
        let text = crate::handler::cdp_domains::tests::reply_text(&reply);
        assert!(text.starts_with("The window did not move"), "{text}");
    }

    /// AUD-IN-15: scrolling to a hidden element fails.
    #[tokio::test]
    async fn scroll_to_hidden_element_fails() {
        let hidden = json!({"status": "hidden"});
        let error = scroll_with(hidden, json!({"selector": "#h"}))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.starts_with("Element '#h' is not visible"), "{error}");
    }

    /// AUD-IN-16: `x` is kept when scrolling by pages.
    #[test]
    fn pages_keep_the_horizontal_offset() {
        let tool: ScrollTool = serde_json::from_value(json!({"x": 10, "pages": 1.5})).unwrap();
        let expression = tool.expression();
        assert!(expression.contains("left: 10,"), "{expression}");
        assert!(
            expression.contains("window.innerHeight * 1.5"),
            "{expression}"
        );
    }
}
