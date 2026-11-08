use crate::handler::McpHandler;
use rust_mcp_sdk::{
    macros,
    schema::{CallToolError, CallToolRequestParams, CallToolResult, ContentBlock, ImageContent},
};
use serde_json::json;

#[macros::mcp_tool(
    name = "capture_screenshot",
    description = "Screenshot of the tab's viewport, of the whole page with full_page, or of one element with selector (at most 16384 CSS pixels per side; a larger capture gets a note). To find elements to act on, prefer 'snapshot'.",
    read_only_hint = true
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct CaptureScreenshotTool {
    /// Default: 'default'.
    pub instance_id: Option<String>,
    /// Default: the active tab.
    pub tab_id: Option<String>,
    /// 'png' (default), 'jpeg' (or 'jpg') or 'webp'.
    pub format: Option<String>,
    /// 0 to 100, for jpeg and webp. Default: 100.
    pub quality: Option<u32>,
    /// Capture the whole scrollable page. Default: false.
    pub full_page: Option<bool>,
    /// Only this element's box: CSS selector, 'host >>> inner' (into an open shadow root or same-origin iframe) or snapshot ref (e12).
    pub selector: Option<String>,
}

/// Why a page in the background is not captured.
const BACKGROUND: &str = "The tab is in the background (its page is hidden) and Chrome draws no frames for it, so nothing was captured: bring it to the front with switch_tab first.";

/// Whether the page is hidden (its tab in the background, or the window
/// minimized). Read without awaiting a promise, so a page paused in the
/// debugger still answers; a page that does not answer counts as shown.
async fn hidden(target: &crate::handler::cdp_domains::cdp_target::CdpTarget) -> bool {
    let params = json!({"expression": "document.visibilityState", "returnByValue": true});
    let reply = target.send_raw_command("Runtime.evaluate", params).await;
    reply
        .ok()
        .and_then(|r| r.result)
        .is_some_and(|r| r["result"]["value"] == "hidden")
}

/// How long a restored window has to show its page.
const RESTORE_WAIT: std::time::Duration = std::time::Duration::from_secs(1);

/// The id of the window showing `target`'s tab when it is minimized.
async fn minimized_window(
    target: &crate::handler::cdp_domains::cdp_target::CdpTarget,
) -> Option<u64> {
    let reply = target
        .send_raw_command("Browser.getWindowForTarget", json!({}))
        .await
        .ok()?
        .result?;
    let minimized = reply["bounds"]["windowState"] == "minimized";
    reply["windowId"].as_u64().filter(|_| minimized)
}

/// Sets the state (`normal`, `minimized`) of window `window`; best effort.
async fn set_window_state(
    target: &crate::handler::cdp_domains::cdp_target::CdpTarget,
    window: u64,
    state: &str,
) {
    let params = json!({"windowId": window, "bounds": {"windowState": state}});
    let _ = target
        .send_raw_command("Browser.setWindowBounds", params)
        .await;
}

/// Whether `target`'s page is shown within `limit`.
async fn shown_within(
    target: &crate::handler::cdp_domains::cdp_target::CdpTarget,
    limit: std::time::Duration,
) -> bool {
    let deadline = tokio::time::Instant::now() + limit;
    loop {
        if !hidden(target).await {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

impl CaptureScreenshotTool {
    pub async fn handle(
        params: CallToolRequestParams,
        handler: &McpHandler,
    ) -> Result<CallToolResult, CallToolError> {
        let args: CaptureScreenshotTool = crate::handler::parse_args(params)?;
        args.check_frame()?;
        let target =
            crate::handler::resolve_target(handler, args.instance_id.clone(), args.tab_id.clone())
                .await?;
        let format = image_format(args.format.as_deref())?;
        let minimized = args.ensure_drawn(handler, &target).await?;
        // As text: the error type is not `Send` across the await below.
        let captured = args
            .capture(&target, format)
            .await
            .map_err(|e| e.to_string());
        // A window restored for the capture is minimized again (ISSUE-98).
        if let Some(window) = minimized {
            set_window_state(&target, window, "minimized").await;
        }
        captured.map_err(CallToolError::from_message)
    }

    /// Captures the page of `target` (shown) as the arguments say.
    async fn capture(
        &self,
        target: &crate::handler::cdp_domains::cdp_target::CdpTarget,
        format: &'static str,
    ) -> Result<CallToolResult, CallToolError> {
        let (args, target) = (self, target);
        let mut command_params = json!({"format": format, "fromSurface": true});
        if let (Some(q), Some(map)) = (args.quality, command_params.as_object_mut()) {
            map.insert("quality".to_string(), json!(q));
        }
        let note = match (args.selector.as_deref(), args.full_page.unwrap_or(false)) {
            (Some(_), true) => return Err(selector_or_full_page()),
            (Some(selector), false) => {
                element_clip_params(target, selector, &mut command_params).await?
            }
            (None, true) => add_full_page_clip(target, &mut command_params).await?,
            (None, false) => None,
        };
        let failure = "CDP Error";
        let captured =
            crate::handler::send_or(target, "Page.captureScreenshot", command_params, failure)
                .await
                .map_err(|e| e.to_string());
        // The element's scroll is put back, whatever the capture did (ISSUE-89).
        if args.selector.is_some() {
            restore_scroll(target).await;
        }
        let val = captured.map_err(CallToolError::from_message)?;
        let data = val
            .result
            .as_ref()
            .and_then(|r| r.get("data"))
            .and_then(|d| d.as_str());
        let base64_data = data.ok_or_else(|| {
            CallToolError::from_message("No data returned from Page.captureScreenshot")
        })?;
        Ok(image_reply(base64_data.to_string(), format, note))
    }
}

impl CaptureScreenshotTool {
    /// Chrome draws no frame of a page that is hidden: a capture of it
    /// waits for the 10 s timeout. So a minimized window is restored (its
    /// id returned, to minimize it again after the capture: ISSUE-98) and
    /// the active tab brought to the front; a tab still hidden (in the
    /// background of its window) is refused at once, naming switch_tab,
    /// the window minimized again.
    async fn ensure_drawn(
        &self,
        handler: &McpHandler,
        target: &crate::handler::cdp_domains::cdp_target::CdpTarget,
    ) -> Result<Option<u64>, CallToolError> {
        if !hidden(target).await {
            return Ok(None);
        }
        let window = minimized_window(target).await;
        if let Some(window) = window {
            set_window_state(target, window, "normal").await;
        }
        let session = handler.session(self.instance_id.clone()).await?;
        let active = crate::util::sync::read(&session.tabs).active_tab_id.clone();
        if self.tab_id.is_none() || self.tab_id == active {
            crate::handler::chrome_instance::tab_lifecycle::front_active(&session.tabs).await;
        }
        // A restored window takes a moment to show its page again.
        let wait = window.map_or(std::time::Duration::ZERO, |_| RESTORE_WAIT);
        if shown_within(target, wait).await {
            return Ok(window);
        }
        if let Some(window) = window {
            set_window_state(target, window, "minimized").await;
        }
        Err(CallToolError::from_message(BACKGROUND))
    }

    /// Refuses an element inside a frame of another origin: its box is
    /// measured in the frame, not in the page the capture is taken of.
    pub(crate) fn check_frame(&self) -> Result<(), CallToolError> {
        match self.selector.as_deref() {
            Some(selector) if crate::util::js::is_frame_selector(selector) => {
                Err(CallToolError::from_message(format!(
                    "'{selector}' is inside a frame of another origin, which capture_screenshot cannot clip to: pass the ref of its iframe element instead."
                )))
            }
            _ => Ok(()),
        }
    }
}

/// Most CSS pixels per side of a full-page capture: Chrome draws at most
/// 16384, and a larger image floods the agent.
pub(crate) const FULL_PAGE_MAX: f64 = 16_384.0;

/// What a full-page capture of a `width` x `height` document says when it
/// is cut to [`FULL_PAGE_MAX`].
pub(crate) fn full_page_note(width: f64, height: f64) -> Option<String> {
    cut_note("Full page", width, height)
}

/// What a capture of `what` (`width` x `height`) says when it is cut to
/// [`FULL_PAGE_MAX`].
fn cut_note(what: &str, width: f64, height: f64) -> Option<String> {
    if width <= FULL_PAGE_MAX && height <= FULL_PAGE_MAX {
        return None;
    }
    let (shown_width, shown_height) = (width.min(FULL_PAGE_MAX), height.min(FULL_PAGE_MAX));
    Some(format!(
        "{what} cut to {FULL_PAGE_MAX} px per side, from the top left: height {shown_height:.0} of {height:.0} px, width {shown_width:.0} of {width:.0} px. Scroll and capture the viewport for the rest."
    ))
}

/// A document box to capture: `(x, y, width, height)` in CSS pixels.
pub(crate) type Clip = (f64, f64, f64, f64);

/// The refusal of a capture asked for both one element and the whole page.
pub(crate) fn selector_or_full_page() -> CallToolError {
    CallToolError::from_message("Pass either 'selector' (one element) or 'full_page', not both.")
}

/// The document box `(x, y, width, height)` to capture for the element the
/// box snippet (`util::js::element_box`) measured, at most
/// [`FULL_PAGE_MAX`] per side, with the note when it was cut; or why the
/// element cannot be captured.
pub(crate) fn element_clip(
    measured: &serde_json::Value,
    selector: &str,
) -> Result<(Clip, Option<String>), CallToolError> {
    let side = |key: &str| measured[key].as_f64();
    match measured["status"].as_str() {
        Some("ok") => {
            let (x, y) = (side("x").unwrap_or_default(), side("y").unwrap_or_default());
            let (width, height) = (
                side("width").unwrap_or_default(),
                side("height").unwrap_or_default(),
            );
            let clip = (x, y, width.min(FULL_PAGE_MAX), height.min(FULL_PAGE_MAX));
            let outside = measured["cut"].as_bool() == Some(true);
            let note = match outside {
                true => Some(PARTLY_OUTSIDE.to_string()),
                false => None,
            };
            let notes = [cut_note("Element", width, height), note];
            let notes: Vec<String> = notes.into_iter().flatten().collect();
            Ok((clip, (!notes.is_empty()).then(|| notes.join(" "))))
        }
        Some("outside") => Err(CallToolError::from_message(format!(
            "Element '{selector}' is outside the page (its box starts at x {}, y {} of the document): nothing was captured.",
            side("x").unwrap_or_default(),
            side("y").unwrap_or_default()
        ))),
        Some("hidden") => Err(CallToolError::from_message(format!(
            "Element '{selector}' is not visible (display:none, visibility:hidden or zero size); nothing was captured."
        ))),
        _ => Err(CallToolError::from_message(format!(
            "Element not found for selector: {selector}{}",
            crate::util::js::ref_hint(selector)
        ))),
    }
}

/// Measures the element `selector` names and clips the capture
/// `params` to it (document coordinates, beyond the viewport); its note.
/// An element that cannot be captured fails, the scroll put back.
async fn element_clip_params(
    target: &crate::handler::cdp_domains::cdp_target::CdpTarget,
    selector: &str,
    params: &mut serde_json::Value,
) -> Result<Option<String>, CallToolError> {
    let expression = crate::util::js::element_box(selector);
    let measured = crate::handler::snippet_value(target, &expression).await;
    let clip = measured.and_then(|m| element_clip(&m, selector));
    // As text: the error type is not `Send` across the await.
    let ((x, y, width, height), note) = match clip.map_err(|e| e.to_string()) {
        Ok(clip) => clip,
        Err(e) => {
            restore_scroll(target).await;
            return Err(CallToolError::from_message(e));
        }
    };
    if let Some(map) = params.as_object_mut() {
        let clip = json!({"x": x, "y": y, "width": width, "height": height, "scale": 1});
        map.insert("clip".to_string(), clip);
        map.insert("captureBeyondViewport".to_string(), json!(true));
    }
    Ok(note)
}

/// Puts back the scroll an element capture changed (`util::js::RESTORE_SCROLL`,
/// best effort), on every way out of the capture (ISSUE-89).
async fn restore_scroll(target: &crate::handler::cdp_domains::cdp_target::CdpTarget) {
    let restore = json!({"expression": crate::util::js::RESTORE_SCROLL});
    let _ = target.send_raw_command("Runtime.evaluate", restore).await;
}

/// The note of an element captured only in part: the rest is outside the
/// page (ISSUE-89).
const PARTLY_OUTSIDE: &str =
    "The element is partly outside the page: only its part inside the page was captured.";

/// The capture format `format` names (any case; `jpg` is `jpeg`), `png`
/// when omitted. Checked before anything is sent: Firefox answered a PNG
/// for a format it does not know, labelled with that format.
pub(crate) fn image_format(format: Option<&str>) -> Result<&'static str, CallToolError> {
    let Some(name) = format else {
        return Ok("png");
    };
    match name.to_ascii_lowercase().as_str() {
        "png" => Ok("png"),
        "jpeg" | "jpg" => Ok("jpeg"),
        "webp" => Ok("webp"),
        _ => Err(CallToolError::from_message(format!(
            "Unsupported image format '{name}': use png, jpeg or webp."
        ))),
    }
}

/// The image format of base64 `data`, from its first bytes.
fn format_of(data: &str) -> Option<&'static str> {
    [("iVBORw0KGgo", "png"), ("/9j/", "jpeg"), ("UklGR", "webp")]
        .into_iter()
        .find(|(prefix, _)| data.starts_with(prefix))
        .map(|(_, format)| format)
}

/// The tool reply: the image, labelled with the format of its bytes (else
/// `format`, the one requested), then `note` when there is one.
pub(crate) fn image_reply(data: String, format: &str, note: Option<String>) -> CallToolResult {
    let format = format_of(&data).unwrap_or(format);
    let image = ImageContent::new(data, format!("image/{format}"), None, None);
    let mut content = vec![ContentBlock::ImageContent(image)];
    content.extend(note.map(|text| ContentBlock::TextContent(text.into())));
    CallToolResult {
        content,
        is_error: None,
        meta: None,
        structured_content: None,
    }
}

/// Clips the capture to the whole document (`Page.getLayoutMetrics`), at
/// most [`FULL_PAGE_MAX`] per side; the note says when it was cut.
async fn add_full_page_clip(
    target: &crate::handler::cdp_domains::cdp_target::CdpTarget,
    command_params: &mut serde_json::Value,
) -> Result<Option<String>, CallToolError> {
    let failure = "Failed to get metrics";
    let metrics =
        crate::handler::send_or(target, "Page.getLayoutMetrics", json!({}), failure).await?;
    let content_size = metrics.result.as_ref().and_then(|r| r.get("contentSize"));
    if let (Some(size), Some(map)) = (content_size, command_params.as_object_mut()) {
        let width = size.get("width").and_then(|v| v.as_f64()).unwrap_or(1280.0);
        let height = size.get("height").and_then(|v| v.as_f64()).unwrap_or(720.0);
        let (clip_width, clip_height) = (width.min(FULL_PAGE_MAX), height.min(FULL_PAGE_MAX));
        let clip = json!({"x": 0, "y": 0, "width": clip_width, "height": clip_height, "scale": 1});
        map.insert("clip".to_string(), clip);
        // Also enable captureBeyondViewport just in case for newer Chrome versions
        map.insert("captureBeyondViewport".to_string(), json!(true));
        return Ok(full_page_note(width, height));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::cdp_domains::tests::spawn_mock_chrome_server;
    use crate::handler::chrome_instance::MockChromeManager;
    use rust_mcp_sdk::schema::CallToolRequestParams;
    use serde_json::json;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    #[tokio::test]
    async fn test_capture_screenshot_params_deserialization() {
        let params: Result<CallToolRequestParams, _> = serde_json::from_value(json!({
            "name": "capture_screenshot",
            "arguments": {
                "format": "jpeg",
                "quality": 80,
                "full_page": true
            }
        }));
        assert!(params.is_ok());
    }

    #[tokio::test]
    async fn test_capture_screenshot_tool_deserialization() {
        let tool: Result<CaptureScreenshotTool, _> = serde_json::from_value(json!({
            "format": "jpeg",
            "quality": 80,
            "full_page": true
        }));
        assert!(tool.is_ok());
        let tool = tool.unwrap();
        assert_eq!(tool.format.as_deref(), Some("jpeg"));
        assert_eq!(tool.quality, Some(80));
        assert_eq!(tool.full_page, Some(true));
    }

    #[tokio::test]
    async fn test_capture_screenshot_handle() {
        let port = spawn_mock_chrome_server().await;

        let mut handler = McpHandler::new_test();
        Arc::get_mut(&mut handler.default_session)
            .unwrap()
            .chrome_manager = Arc::new(Mutex::new(MockChromeManager::new(port)));

        let params: CallToolRequestParams = serde_json::from_value(json!({
            "name": "capture_screenshot",
            "arguments": {}
        }))
        .unwrap();

        let result = CaptureScreenshotTool::handle(params, &handler).await;
        // In the mock server, if the result is not exactly right, we might get an error because the mock server
        // doesn't return the mocked screenshot data. We'll update the mock server to return valid data.
        assert!(result.is_ok(), "Handle should succeed: {:?}", result.err());
    }

    /// ISSUE-68 (Chrome): a tab whose page stays hidden (in the background)
    /// is not captured: Chrome draws no frame of it, so the capture would
    /// wait for the 10 s timeout. The reply says to bring it to the front.
    #[tokio::test]
    async fn a_hidden_page_is_refused_at_once() {
        use crate::handler::cdp_domains::tests::{call, mock_handler, spawn_recording_mock};
        // Given a page that stays hidden
        let mock = spawn_recording_mock().await;
        mock.respond_with(
            "Runtime.evaluate",
            |_| json!({"result": {"result": {"type": "string", "value": "hidden"}}}),
        );
        let handler = mock_handler(mock.http_port);
        // When it is captured
        let error = CaptureScreenshotTool::handle(call("capture_screenshot", json!({})), &handler)
            .await
            .unwrap_err()
            .to_string();
        // Then it fails at once naming switch_tab, and nothing is captured
        assert!(error.contains("in the background"), "{error}");
        assert!(error.contains("switch_tab"), "{error}");
        assert!(mock.commands_named("Page.captureScreenshot").is_empty());
    }

    /// ISSUE-98 (Chrome): a page hidden because its window is minimized
    /// is captured: the window is restored for the capture and minimized
    /// again (the reply sent the agent to switch_tab, which cannot help).
    #[tokio::test]
    async fn a_minimized_window_is_restored_for_the_capture() {
        use crate::handler::cdp_domains::tests::{call, mock_handler, spawn_recording_mock};
        use std::sync::atomic::{AtomicBool, Ordering};
        // Given a page hidden until its minimized window is restored
        let mock = spawn_recording_mock().await;
        let shown = std::sync::Arc::new(AtomicBool::new(false));
        let seen = shown.clone();
        mock.respond_with("Runtime.evaluate", move |_| {
            let state = if seen.load(Ordering::SeqCst) {
                "visible"
            } else {
                "hidden"
            };
            json!({"result": {"result": {"type": "string", "value": state}}})
        });
        mock.respond_with(
            "Browser.getWindowForTarget",
            |_| json!({"result": {"windowId": 7, "bounds": {"windowState": "minimized"}}}),
        );
        let restore = shown.clone();
        mock.respond_with("Browser.setWindowBounds", move |params| {
            let state = params["bounds"]["windowState"].as_str() == Some("normal");
            restore.store(state, Ordering::SeqCst);
            json!({"result": {}})
        });
        mock.respond_with(
            "Page.captureScreenshot",
            |_| json!({"result": {"data": "iVBO"}}),
        );
        let handler = mock_handler(mock.http_port);
        // When it is captured
        let reply =
            CaptureScreenshotTool::handle(call("capture_screenshot", json!({})), &handler).await;
        // Then it is captured with the window restored, then minimized again
        assert!(reply.is_ok(), "{reply:?}");
        let states: Vec<_> = mock
            .commands_named("Browser.setWindowBounds")
            .into_iter()
            .map(|c| {
                (
                    c.params["windowId"].clone(),
                    c.params["bounds"]["windowState"].clone(),
                )
            })
            .collect();
        assert_eq!(
            states,
            [(json!(7), json!("normal")), (json!(7), json!("minimized"))]
        );
    }

    /// ISSUE-98 (Chrome): a tab in the background of a minimized window
    /// stays hidden once the window is restored: refused naming switch_tab,
    /// the window minimized again.
    #[tokio::test]
    async fn a_background_tab_of_a_minimized_window_is_refused() {
        use crate::handler::cdp_domains::tests::{call, mock_handler, spawn_recording_mock};
        // Given a page that stays hidden, in a minimized window
        let mock = spawn_recording_mock().await;
        mock.respond_with(
            "Runtime.evaluate",
            |_| json!({"result": {"result": {"type": "string", "value": "hidden"}}}),
        );
        mock.respond_with(
            "Browser.getWindowForTarget",
            |_| json!({"result": {"windowId": 7, "bounds": {"windowState": "minimized"}}}),
        );
        let handler = mock_handler(mock.http_port);
        // When it is captured
        let error = CaptureScreenshotTool::handle(call("capture_screenshot", json!({})), &handler)
            .await
            .unwrap_err()
            .to_string();
        // Then it fails naming switch_tab, and the window is minimized again
        assert!(error.contains("switch_tab"), "{error}");
        let states: Vec<_> = mock
            .commands_named("Browser.setWindowBounds")
            .into_iter()
            .map(|c| c.params["bounds"]["windowState"].clone())
            .collect();
        assert_eq!(states, [json!("normal"), json!("minimized")]);
        assert!(mock.commands_named("Page.captureScreenshot").is_empty());
    }

    /// AUD-IN-22: a full-page capture of a 30 000 px page is cut to
    /// FULL_PAGE_MAX and the reply says so.
    #[tokio::test]
    async fn tall_full_page_is_cut_with_a_note() {
        use crate::handler::cdp_domains::tests::{call, mock_handler, spawn_recording_mock};
        // Given a page 1905 x 30000 px
        let mock = spawn_recording_mock().await;
        mock.respond_with("Page.getLayoutMetrics", |_| {
            json!({"result": {"contentSize": {"x": 0, "y": 0, "width": 1905, "height": 30000}}})
        });
        mock.respond_with(
            "Page.captureScreenshot",
            |_| json!({"result": {"data": "iVBO"}}),
        );
        let handler = mock_handler(mock.http_port);
        // When the whole page is captured
        let reply = CaptureScreenshotTool::handle(
            call("capture_screenshot", json!({"full_page": true})),
            &handler,
        )
        .await
        .unwrap();
        // Then the clip stops at FULL_PAGE_MAX and a note follows the image
        let sent = &mock.commands_named("Page.captureScreenshot")[0].params;
        assert_eq!(sent["clip"]["height"], FULL_PAGE_MAX);
        assert_eq!(sent["clip"]["width"], 1905.0);
        let ContentBlock::TextContent(note) = &reply.content[1] else {
            panic!("no note: {:?}", reply.content);
        };
        assert!(note.text.contains("16384 of 30000"), "{}", note.text);
        assert_eq!(full_page_note(1905.0, 300.0), None);
    }

    /// ISSUE-33: with a selector the capture is clipped to the element's
    /// box in document coordinates (beyond the viewport when it is taller).
    #[tokio::test]
    async fn an_element_is_captured_alone() {
        use crate::handler::cdp_domains::tests::{call, mock_handler, spawn_recording_mock};
        // Given an element at (10, 2000) of 123 x 45 px in the document
        let mock = spawn_recording_mock().await;
        mock.respond_with("Runtime.evaluate", |_| {
            let value = json!({"status": "ok", "x": 10, "y": 2000, "width": 123, "height": 45});
            json!({"result": {"result": {"type": "object", "value": value}}})
        });
        mock.respond_with(
            "Page.captureScreenshot",
            |_| json!({"result": {"data": "iVBO"}}),
        );
        let handler = mock_handler(mock.http_port);
        // When it is captured
        let args = json!({"selector": "#chart"});
        let reply = CaptureScreenshotTool::handle(call("capture_screenshot", args), &handler).await;
        // Then the capture is clipped to its box
        assert!(reply.is_ok(), "{reply:?}");
        let sent = &mock.commands_named("Page.captureScreenshot")[0].params;
        assert_eq!(
            sent["clip"],
            json!({"x": 10.0, "y": 2000.0, "width": 123.0, "height": 45.0, "scale": 1})
        );
        assert_eq!(sent["captureBeyondViewport"], true);
    }

    /// ISSUE-89: an element outside the page fails saying so; one partly
    /// outside is captured cut to the page, with a note.
    #[test]
    fn elements_outside_the_page_are_refused_or_cut() {
        let outside = json!({"status": "outside", "x": -9999, "y": 10});
        let error = element_clip(&outside, "#off").unwrap_err().to_string();
        assert!(error.contains("'#off' is outside the page"), "{error}");
        let cut = json!({"status": "ok", "x": 0, "y": 10, "width": 20, "height": 50, "cut": true});
        let (clip, note) = element_clip(&cut, "#half").unwrap();
        assert_eq!(clip, (0.0, 10.0, 20.0, 50.0));
        assert!(note.unwrap().contains("partly outside the page"));
    }

    /// ISSUE-89: the page's scroll is put back after an element capture,
    /// also when the capture fails or the element cannot be captured (it
    /// was left where the element was).
    #[tokio::test]
    async fn the_scroll_is_restored_after_an_element_capture() {
        use crate::handler::cdp_domains::tests::{call, mock_handler, spawn_recording_mock};
        let inside = json!({"status": "ok", "x": 10, "y": 2000, "width": 123, "height": 45});
        let outside = json!({"status": "outside", "x": -9999, "y": 10});
        let captured = json!({"result": {"data": "iVBO"}});
        let failed = json!({"error": {"code": -32000, "message": "boom"}});
        // A capture that works, one that fails, and an element outside
        for (measured, answer) in [
            (inside.clone(), captured.clone()),
            (inside, failed),
            (outside, captured),
        ] {
            let mock = spawn_recording_mock().await;
            mock.respond_with("Runtime.evaluate", move |_| {
                json!({"result": {"result": {"type": "object", "value": measured.clone()}}})
            });
            mock.respond_with("Page.captureScreenshot", move |_| answer.clone());
            let handler = mock_handler(mock.http_port);
            let args = json!({"selector": "#chart"});
            let _ = CaptureScreenshotTool::handle(call("capture_screenshot", args), &handler).await;
            let restored = mock
                .commands_named("Runtime.evaluate")
                .into_iter()
                .any(|c| c.params["expression"].as_str() == Some(crate::util::js::RESTORE_SCROLL));
            assert!(restored);
        }
    }

    /// ISSUE-33: a missing element, or a selector with full_page, fails
    /// without capturing.
    #[tokio::test]
    async fn a_missing_element_is_not_captured() {
        use crate::handler::cdp_domains::tests::{call, mock_handler, spawn_recording_mock};
        let mock = spawn_recording_mock().await;
        mock.respond_with(
            "Runtime.evaluate",
            |_| json!({"result": {"result": {"type": "object", "value": {"status": "missing"}}}}),
        );
        let handler = mock_handler(mock.http_port);
        let missing = json!({"selector": "#nope"});
        let error = CaptureScreenshotTool::handle(call("capture_screenshot", missing), &handler)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("Element not found for selector: #nope"),
            "{error}"
        );
        let both = json!({"selector": "#chart", "full_page": true});
        let error = CaptureScreenshotTool::handle(call("capture_screenshot", both), &handler)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("either"), "{error}");
        assert!(mock.commands_named("Page.captureScreenshot").is_empty());
    }

    /// AUD-IN-20: the format is checked before anything is sent; `jpg` and
    /// any case are accepted (Firefox labelled a PNG `image/gif`).
    #[test]
    fn formats_are_checked() {
        assert_eq!(image_format(None).unwrap(), "png");
        assert_eq!(image_format(Some("JPEG")).unwrap(), "jpeg");
        assert_eq!(image_format(Some("jpg")).unwrap(), "jpeg");
        assert_eq!(image_format(Some("webp")).unwrap(), "webp");
        let error = image_format(Some("gif")).unwrap_err().to_string();
        assert_eq!(
            error,
            "Unsupported image format 'gif': use png, jpeg or webp."
        );
    }

    /// AUD-IN-20: the reply's MIME type is the one of the image's bytes.
    #[test]
    fn mime_type_follows_the_bytes() {
        let mime = |data: &str| {
            let reply = image_reply(data.to_string(), "jpeg", None);
            let ContentBlock::ImageContent(image) = &reply.content[0] else {
                panic!("no image");
            };
            image.mime_type.clone()
        };
        assert_eq!(mime("iVBORw0KGgoAAAANSUhEUg"), "image/png");
        assert_eq!(mime("/9j/4AAQSkZJRg"), "image/jpeg");
        assert_eq!(mime("UklGRlYAAABXRUJQ"), "image/webp");
        assert_eq!(mime("AAAA"), "image/jpeg");
    }

    /// AUD-IN-20: Chrome gets no capture command for a bad format.
    #[tokio::test]
    async fn bad_format_sends_nothing() {
        use crate::handler::cdp_domains::tests::{call, mock_handler, spawn_recording_mock};
        let mock = spawn_recording_mock().await;
        let handler = mock_handler(mock.http_port);
        let args = json!({"format": "gif"});
        let result =
            CaptureScreenshotTool::handle(call("capture_screenshot", args), &handler).await;
        assert!(result.is_err());
        assert!(mock.commands_named("Page.captureScreenshot").is_empty());
    }
}
