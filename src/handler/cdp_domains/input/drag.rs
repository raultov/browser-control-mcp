use crate::handler::McpHandler;
use crate::handler::cdp_domains::cdp_target::CdpTarget;
use rust_mcp_sdk::{
    macros,
    schema::{CallToolError, CallToolRequestParams, CallToolResult},
};
use serde_json::json;

#[macros::mcp_tool(
    name = "drag",
    description = "Drags an element onto another with the mouse: presses on 'from' (scrolled into view; fails when hidden or covered), moves in steps to the centre of 'to' (which must then be in the viewport) and releases. For sortable lists, boards, sliders and HTML5 drag and drop (on Firefox, HTML5 drag and drop is simulated with script events)."
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct DragTool {
    /// Instance id; omit for the default instance.
    pub instance_id: Option<String>,
    /// Tab id; omit for the active tab.
    pub tab_id: Option<String>,
    /// Element to drag: CSS selector, 'host >>> inner' (inside an open shadow root or same-origin iframe), or a snapshot ref (e12).
    pub from: String,
    /// Element to drop onto, same forms as 'from'.
    pub to: String,
}

/// Mouse moves between the press and the release: libraries start a drag
/// only after the pointer travelled a few pixels, and some track every move.
pub(crate) const DRAG_STEPS: u32 = 10;

/// The pause between two moves of a drag, and over the target before the
/// release: sortable lists animate each reordering (SortableJS: 150 ms)
/// and only react to the moves that come after it, as a hand's would.
pub(crate) const DRAG_STEP_PAUSE: std::time::Duration = std::time::Duration::from_millis(40);

/// The viewport points from `start` to `end` the mouse passes through, the
/// last one `end`.
pub(crate) fn drag_path(start: (f64, f64), end: (f64, f64)) -> Vec<(f64, f64)> {
    (1..=DRAG_STEPS)
        .map(|step| {
            let part = f64::from(step) / f64::from(DRAG_STEPS);
            (
                (start.0 + (end.0 - start.0) * part).round(),
                (start.1 + (end.1 - start.1) * part).round(),
            )
        })
        .collect()
}

/// The reply once the element was dropped; `note` says how, when it was
/// not real mouse input.
pub(crate) fn dragged_reply(
    args: &DragTool,
    start: (f64, f64),
    end: (f64, f64),
    note: Option<&str>,
) -> CallToolResult {
    let mut text = format!(
        "Dragged '{}' onto '{}', from ({}, {}) to ({}, {}).",
        args.from, args.to, start.0, start.1, end.0, end.1
    );
    if let Some(note) = note {
        text.push_str("\nNote: ");
        text.push_str(note);
    }
    CallToolResult::text_content(vec![text.into()])
}

impl DragTool {
    /// The viewport point to drop on, from the drop-point snippet
    /// (`util::js::drop_point`), or why there is none. The target is not
    /// scrolled: that could take the element to drag out of view.
    pub(crate) fn drop_point(
        &self,
        measured: &serde_json::Value,
    ) -> Result<(f64, f64), CallToolError> {
        if measured["status"].as_str() == Some("outside") {
            return Err(CallToolError::from_message(format!(
                "Drop target '{}' is outside the viewport once '{}' is in view; nothing was dragged. Scroll so both show, or drag in shorter steps.",
                self.to, self.from
            )));
        }
        super::click_element::click_point(measured, &self.to, "dragged")
    }

    pub async fn handle(
        params: CallToolRequestParams,
        handler: &McpHandler,
    ) -> Result<CallToolResult, CallToolError> {
        let ids = |a: &Self| (a.instance_id.clone(), a.tab_id.clone());
        let (args, target) = crate::handler::args_and_target(params, handler, ids).await?;
        let settled = json!({"expression": crate::util::js::ANIMATIONS_DONE, "awaitPromise": true, "returnByValue": true});
        crate::handler::send_or(&target, "Runtime.evaluate", settled, "CDP Error").await?;
        let start = super::click_element::checked_point(&target, &args.from, "dragged").await?;
        let expression = crate::util::js::drop_point(&args.to);
        let measured = crate::handler::snippet_value(&target, &expression).await?;
        let end = args.drop_point(&measured)?;
        drag_between(&target, start, end).await?;
        Ok(dragged_reply(&args, start, end, None))
    }
}

/// Presses the left button at `start`, moves along [`drag_path`] with it
/// held and releases it at `end`: Chrome turns these into pointer events
/// and, over a `draggable` element, into HTML5 drag and drop.
async fn drag_between(
    target: &CdpTarget,
    start: (f64, f64),
    end: (f64, f64),
) -> Result<(), CallToolError> {
    let event = |kind: &str, (x, y): (f64, f64), buttons: u8| json!({"type": kind, "x": x, "y": y, "button": "left", "buttons": buttons, "clickCount": 1});
    let send =
        |params| crate::handler::send_or(target, "Input.dispatchMouseEvent", params, "CDP Error");
    send(json!({"type": "mouseMoved", "x": start.0, "y": start.1, "button": "none"})).await?;
    send(event("mousePressed", start, 1)).await?;
    // Every move, the first included, comes after a pause.
    for point in drag_path(start, end) {
        tokio::time::sleep(DRAG_STEP_PAUSE).await;
        send(event("mouseMoved", point, 1)).await?;
    }
    tokio::time::sleep(DRAG_STEP_PAUSE).await;
    send(event("mouseReleased", end, 0)).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::cdp_domains::tests::{
        call, mock_handler, reply_text, spawn_recording_mock,
    };
    use serde_json::Value;

    /// A Chrome page where the element to drag is at (10, 20) and the drop
    /// target answers `drop` (the drop-point snippet).
    async fn page(
        drop: Value,
    ) -> &'static cdp_browser_lite::test_support::mock_devtools::MockDevTools {
        let mock = spawn_recording_mock().await;
        mock.respond_with("Runtime.evaluate", move |params| {
            let expression = params["expression"].as_str().unwrap_or_default();
            let value = if expression.contains("elementFromPoint") {
                json!({"status": "ok", "x": 10, "y": 20})
            } else {
                drop.clone()
            };
            json!({"result": {"result": {"type": "object", "value": value}}})
        });
        mock
    }

    /// ISSUE-30: drag presses on the checked point of 'from', moves in
    /// steps with the button held and releases on the centre of 'to'.
    #[tokio::test]
    async fn drag_presses_moves_and_releases() {
        // Given an element at (10, 20) and a drop target at (110, 220)
        let mock = page(json!({"status": "ok", "x": 110, "y": 220})).await;
        let handler = mock_handler(mock.http_port);
        // When it is dragged onto the target
        let args = json!({"from": "#card", "to": "#column"});
        let reply = DragTool::handle(call("drag", args), &handler)
            .await
            .unwrap();
        // Then the mouse is pressed there, moved in steps and released there
        let events: Vec<Value> = mock
            .commands_named("Input.dispatchMouseEvent")
            .into_iter()
            .map(|c| c.params)
            .collect();
        let kinds: Vec<&str> = events.iter().filter_map(|e| e["type"].as_str()).collect();
        assert_eq!(kinds.len(), 3 + DRAG_STEPS as usize, "{kinds:?}");
        assert_eq!(&kinds[..2], ["mouseMoved", "mousePressed"]);
        assert_eq!(kinds.last(), Some(&"mouseReleased"));
        assert_eq!(
            (events[1]["x"].clone(), events[1]["buttons"].clone()),
            (json!(10.0), json!(1))
        );
        let last_move = &events[events.len() - 2];
        assert_eq!(
            (last_move["x"].clone(), last_move["y"].clone()),
            (json!(110.0), json!(220.0))
        );
        assert_eq!(last_move["buttons"], 1);
        assert_eq!(
            reply_text(&reply),
            "Dragged '#card' onto '#column', from (10, 20) to (110, 220)."
        );
    }

    /// ISSUE-30: a drop target out of the viewport fails before anything
    /// is pressed.
    #[tokio::test]
    async fn a_drop_target_out_of_view_presses_nothing() {
        let mock = page(json!({"status": "outside"})).await;
        let handler = mock_handler(mock.http_port);
        let args = json!({"from": "#card", "to": "#far"});
        let error = DragTool::handle(call("drag", args), &handler)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("'#far' is outside the viewport"), "{error}");
        assert!(mock.commands_named("Input.dispatchMouseEvent").is_empty());
    }

    /// ISSUE-30: the path ends on the target, in whole pixels.
    #[test]
    fn the_path_ends_on_the_target() {
        let path = drag_path((0.0, 0.0), (15.0, 5.0));
        assert_eq!(path.len(), DRAG_STEPS as usize);
        assert_eq!(path[0], (2.0, 1.0));
        assert_eq!(path.last(), Some(&(15.0, 5.0)));
    }
}
