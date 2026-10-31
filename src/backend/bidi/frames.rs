//! Frames of another origin on Firefox (#27, see `handler::frames`). Each
//! frame is a browsing context of its own: `script.evaluate` runs there,
//! and input actions sent to it land in the frame, in its coordinates (sent
//! to the tab's context they do not reach a frame of another process). A
//! tool acting in a frame acts on its context as it would on a tab's.

use super::page;
use super::session::FirefoxTab;
use crate::handler::frames::{FrameReader, FrameTables};
use bidi_browser_lite::{BrowserClient, Tab};
use rust_mcp_sdk::schema::CallToolError;
use serde_json::Value;

/// How Firefox reaches the frames of a tab's page.
pub(crate) struct FirefoxFrames {
    pub(crate) page: Tab,
}

impl FrameReader for FirefoxFrames {
    type Frame = Tab;

    async fn evaluate(
        &self,
        frame: Option<&Tab>,
        expression: &str,
    ) -> Result<Value, CallToolError> {
        Ok(page::run_snippet(frame.unwrap_or(&self.page), expression).await?)
    }

    /// The `<iframe>`'s window, serialized as its browsing context (the key).
    async fn child(
        &self,
        parent: Option<&Tab>,
        owner: &str,
        scroll: bool,
    ) -> Result<(Tab, String), CallToolError> {
        let tab = parent.unwrap_or(&self.page);
        let expression = crate::util::js::frame_owner(owner, scroll, ".contentWindow");
        let Some(context) = page::window_context(tab, &expression).await? else {
            return Err(CallToolError::from_message(
                "The iframe is gone from the page: take a new snapshot.",
            ));
        };
        let browser = BrowserClient::from_client(self.page.client().clone());
        let frame = browser
            .attach(&context)
            .await
            .map_err(|e| super::protocol_error("browsingContext.getTree", &e))?;
        Ok((frame, context))
    }
}

/// `tab` acting in frame `number` (from `tables`, under the tab's MCP id
/// `id`): its context replaced by the frame's.
pub(crate) async fn enter_frame(
    mut tab: FirefoxTab,
    tables: &FrameTables,
    id: &str,
    number: usize,
) -> Result<FirefoxTab, CallToolError> {
    let frames = FirefoxFrames {
        page: tab.tab.clone(),
    };
    let route = crate::handler::frames::TabFrames {
        reader: &frames,
        tables,
        tab: id,
    }
    .enter(number)
    .await?;
    tab.tab = route.frame;
    Ok(tab)
}
