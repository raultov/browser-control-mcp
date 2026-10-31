use crate::handler::McpHandler;
use crate::util::outline::SNAPSHOT_MAX_CHARS;
use rust_mcp_sdk::{
    macros,
    schema::{CallToolError, CallToolRequestParams, CallToolResult},
};
use serde_json::Value;

#[macros::mcp_tool(
    name = "snapshot",
    description = "Outlines the page for acting on it: one line per element with a role (button, link, textbox, heading...), its name, states, value and a ref ([ref=e12]), indented under its parent; through open shadow roots and same-origin iframes, hidden elements left out, password values never shown; at most 50000 characters. Pass a ref as the selector of the input tools; refs last until the page navigates. Computed in the page, the same on Chrome and Firefox."
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct SnapshotTool {
    /// Instance id; omit for the default instance.
    pub instance_id: Option<String>,
    /// Tab id; omit for the active tab.
    pub tab_id: Option<String>,
    /// Outline only this element (CSS, 'host >>> inner' or a ref). Default: the whole page.
    pub selector: Option<String>,
    /// Most nesting levels (0 = the top level only). Default: no limit.
    pub depth: Option<u32>,
}

impl SnapshotTool {
    /// The JavaScript that outlines the page (shared by every browser).
    pub(crate) fn expression(&self) -> String {
        crate::util::outline::snapshot(self.selector.as_deref(), self.depth)
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

    pub async fn handle(
        params: CallToolRequestParams,
        handler: &McpHandler,
    ) -> Result<CallToolResult, CallToolError> {
        crate::handler::handle_snippet_tool::<Self>(params, handler).await
    }
}

impl crate::handler::SnippetTool for SnapshotTool {
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
    }
}
