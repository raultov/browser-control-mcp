use crate::handler::McpHandler;
use rust_mcp_sdk::{
    macros,
    schema::{CallToolError, CallToolRequestParams, CallToolResult, ContentBlock, ImageContent},
};
use serde_json::json;

#[macros::mcp_tool(
    name = "capture_screenshot",
    description = "Captures a visual representation of the current page viewport or entire page as a base64 encoded image ('full_page' defaults to false, capturing only the visible viewport; set true to capture the entire page, at most 16384 CSS pixels per side from the top left; a larger page adds a text note with its size). Side effects: none (read-only). Prerequisites: requires an active Chrome tab. Returns: base64 encoded image in specified format. Use this to visually verify UI state, layout, or rendering. Alternatives: 'inspect_dom' for raw HTML structure, 'get_performance_metrics' for rendering metrics."
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct CaptureScreenshotTool {
    /// Chrome instance id from open_instance/list_instances. Omit for the default instance.
    pub instance_id: Option<String>,
    /// The Tab ID of the target tab. Omit to use the active tab.
    pub tab_id: Option<String>,
    /// Output image format. Constraints: must be 'png', 'jpeg', or 'webp'. Interactions: 'quality' applies only to 'jpeg' and 'webp' formats. Defaults to: "png".
    pub format: Option<String>,
    /// Compression quality (0-100, higher=better quality). Constraints: integer between 0 and 100. Interactions: only applies when 'format' is 'jpeg' or 'webp'; ignored for 'png'. Defaults to: 100.
    pub quality: Option<u32>,
    /// Capture the entire page beyond visible viewport. Constraints: boolean value. Interactions: if true, captures full page height; if false, captures only visible area. Defaults to: false.
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
        let format = args.format.clone().unwrap_or_else(|| "png".to_string());
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
        Ok(image_reply(base64_data.to_string(), &format, note))
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

/// The tool reply: the image, then `note` when there is one.
pub(crate) fn image_reply(data: String, format: &str, note: Option<String>) -> CallToolResult {
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
}
