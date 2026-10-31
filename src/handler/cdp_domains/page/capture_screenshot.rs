use crate::handler::McpHandler;
use rust_mcp_sdk::{
    macros,
    schema::{CallToolError, CallToolRequestParams, CallToolResult, ContentBlock, ImageContent},
};
use serde_json::json;

#[macros::mcp_tool(
    name = "capture_screenshot",
    description = "Screenshot of the tab's viewport, or of the whole page with full_page (at most 16384 CSS pixels per side; a larger page gets a note). To find elements to act on, prefer 'snapshot'."
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct CaptureScreenshotTool {
    /// Instance id; omit for the default instance.
    pub instance_id: Option<String>,
    /// Tab id; omit for the active tab.
    pub tab_id: Option<String>,
    /// 'png' (default), 'jpeg' (or 'jpg') or 'webp'.
    pub format: Option<String>,
    /// 0 to 100, for jpeg and webp. Default: 100.
    pub quality: Option<u32>,
    /// Capture the whole scrollable page. Default: false.
    pub full_page: Option<bool>,
}

impl CaptureScreenshotTool {
    pub async fn handle(
        params: CallToolRequestParams,
        handler: &McpHandler,
    ) -> Result<CallToolResult, CallToolError> {
        let args: CaptureScreenshotTool = crate::handler::parse_args(params)?;
        let target =
            crate::handler::resolve_target(handler, args.instance_id.clone(), args.tab_id.clone())
                .await?;
        let format = image_format(args.format.as_deref())?;
        let mut command_params = json!({"format": format, "fromSurface": true});
        if let (Some(q), Some(map)) = (args.quality, command_params.as_object_mut()) {
            map.insert("quality".to_string(), json!(q));
        }
        let note = if args.full_page.unwrap_or(false) {
            add_full_page_clip(&target, &mut command_params).await?
        } else {
            None
        };
        let failure = "CDP Error";
        let val =
            crate::handler::send_or(&target, "Page.captureScreenshot", command_params, failure)
                .await?;
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

/// Most CSS pixels per side of a full-page capture: Chrome draws at most
/// 16384, and a larger image floods the agent.
pub(crate) const FULL_PAGE_MAX: f64 = 16_384.0;

/// What a full-page capture of a `width` x `height` document says when it
/// is cut to [`FULL_PAGE_MAX`].
pub(crate) fn full_page_note(width: f64, height: f64) -> Option<String> {
    if width <= FULL_PAGE_MAX && height <= FULL_PAGE_MAX {
        return None;
    }
    let (shown_width, shown_height) = (width.min(FULL_PAGE_MAX), height.min(FULL_PAGE_MAX));
    Some(format!(
        "Full page cut to {FULL_PAGE_MAX} px per side, from the top left: height {shown_height:.0} of {height:.0} px, width {shown_width:.0} of {width:.0} px. Scroll and capture the viewport for the rest."
    ))
}

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
