use crate::handler::McpHandler;
use crate::util::outline::SNAPSHOT_MAX_CHARS;
use rust_mcp_sdk::{
    macros,
    schema::{CallToolError, CallToolRequestParams, CallToolResult},
};
use serde_json::Value;

#[macros::mcp_tool(
    name = "snapshot",
    description = "Outlines the page to act on it: one line per element with a role (button, link, textbox, heading...), name, states, value and a ref ([ref=e12]), indented under its parent; an invalid field shows [invalid] [error='why'], a <select> its first 25 options (pick one with fill_input); through open shadow roots and iframes (another origin's frame is [frame=f2], refs f2e5; selector 'f2' outlines it alone); hidden elements left out, passwords never shown; at most 50000 characters (find_in_snapshot searches it whole). Refs work as the selector of the input tools until the page navigates. Same on Chrome and Firefox."
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct SnapshotTool {
    /// Default: the default instance.
    pub instance_id: Option<String>,
    /// Default: the active tab.
    pub tab_id: Option<String>,
    /// Outline only this element (CSS, 'host >>> inner' or a ref). Default: the whole page.
    pub selector: Option<String>,
    /// Most nesting levels (0 = the top level only). Default: no limit.
    pub depth: Option<u32>,
}

impl SnapshotTool {
    /// The JavaScript that outlines the page (shared by every browser).
    pub(crate) fn expression(&self) -> String {
        crate::util::outline::snapshot(self.selector.as_deref(), self.depth, SNAPSHOT_MAX_CHARS)
    }

    /// The reply from the outline (shared by every browser).
    pub(crate) fn reply(&self, outcome: &Value) -> Result<CallToolResult, CallToolError> {
        if outcome["status"] != "ok" {
            let selector = self.selector.as_deref().unwrap_or_default();
            return Err(CallToolError::from_message(format!(
                "Element not found for selector: {selector}{}",
                crate::util::js::ref_hint(selector)
            )));
        }
        let text = |key: &str| outcome[key].as_str().unwrap_or_default();
        let mut reply = format!(
            "Snapshot of {} ({:?}). Pass a ref as the 'selector' of click_element, fill_input, scroll or wait_for; refs last until the page navigates.\n{}",
            text("url"),
            text("title"),
            text("text")
        );
        if outcome["truncated"] == true {
            reply.push_str(&format!(
                "\n[Cut at {SNAPSHOT_MAX_CHARS} characters: narrow it with 'selector' or 'depth', or search it with 'find_in_snapshot'.]"
            ));
        }
        Ok(CallToolResult::text_content(vec![reply.into()]))
    }

    /// The outline of the page with its frames of another origin in place
    /// (see `handler::frames`).
    pub async fn handle(
        params: CallToolRequestParams,
        handler: &McpHandler,
    ) -> Result<CallToolResult, CallToolError> {
        use crate::handler::cdp_domains::frame_scope::frames_of;
        let args: SnapshotTool = crate::handler::parse_args(params)?;
        let session = handler.session(args.instance_id.clone()).await?;
        let target = session.target(args.tab_id.clone()).await?;
        let outcome = match frames_of(&target) {
            Some((frames, tab)) => {
                let (selector, depth) = (args.selector.as_deref(), args.depth);
                crate::handler::frames::TabFrames {
                    reader: &frames,
                    tables: &session.frames,
                    tab: &tab,
                }
                .snapshot(selector, depth)
                .await?
            }
            None => crate::handler::snippet_value(&target, &args.expression()).await?,
        };
        args.reply(&outcome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool(args: Value) -> SnapshotTool {
        serde_json::from_value(args).unwrap()
    }

    /// ISSUE-4: the reply gives the page, how to use refs, the outline, and
    /// a note when the outline was cut.
    #[test]
    fn reply_gives_the_outline_and_how_to_use_refs() {
        // Given an outline the page cut
        let outline = "- heading \"Sign in\" [level=1] [ref=e2]\n- button \"Go\" [ref=e3]";
        let outcome = json!({"status": "ok", "url": "http://a/", "title": "Login", "text": outline, "truncated": true});
        // When the reply is built
        let reply = tool(json!({})).reply(&outcome).unwrap();
        let text = crate::handler::cdp_domains::tests::reply_text(&reply);
        // Then it names the page, carries the outline and the cut note
        assert!(
            text.starts_with("Snapshot of http://a/ (\"Login\"). Pass a ref as the 'selector'"),
            "{text}"
        );
        assert!(text.contains(outline), "{text}");
        assert!(
            text.ends_with("or search it with 'find_in_snapshot'.]"),
            "{text}"
        );
    }

    /// ISSUE-4: a scope that names nothing fails; a stale ref says why.
    #[test]
    fn missing_scope_fails() {
        let missing = json!({"status": "missing"});
        let error = tool(json!({"selector": "e9"}))
            .reply(&missing)
            .unwrap_err()
            .to_string();
        assert!(
            error.starts_with("Element not found for selector: e9 (a ref from 'snapshot'"),
            "{error}"
        );
        assert!(error.contains("take a new snapshot"), "{error}");
    }

    /// ISSUE-4 (Chrome): the outline snippet is evaluated by value.
    #[tokio::test]
    async fn chrome_evaluates_the_shared_outline() {
        let mock = crate::handler::cdp_domains::tests::spawn_recording_mock().await;
        let outcome = json!({"status": "ok", "url": "http://a/", "title": "", "text": "- button \"Go\" [ref=e1]", "truncated": false});
        mock.respond_with(
            "Runtime.evaluate",
            move |_| json!({"result": {"result": {"type": "object", "value": outcome.clone()}}}),
        );
        let handler = crate::handler::cdp_domains::tests::mock_handler(mock.http_port);
        let params = crate::handler::cdp_domains::tests::call("snapshot", json!({"depth": 3}));
        let reply = SnapshotTool::handle(params, &handler).await.unwrap();
        let text = crate::handler::cdp_domains::tests::reply_text(&reply);
        assert!(text.ends_with("- button \"Go\" [ref=e1]"), "{text}");
        let sent = &mock.commands_named("Runtime.evaluate")[0].params;
        assert!(
            sent["expression"]
                .as_str()
                .unwrap()
                .contains("outline(root, 3, ")
        );
        // ISSUE-64: the outline is synchronous; awaiting a promise would hang
        // on a page paused in the debugger (no microtask runs)
        let awaited = mock
            .commands_named("Runtime.evaluate")
            .into_iter()
            .filter(|c| c.params["awaitPromise"] == json!(true))
            .count();
        assert_eq!(awaited, 0);
    }
}
