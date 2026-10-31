use crate::handler::McpHandler;
use rust_mcp_sdk::{
    macros,
    schema::{CallToolError, CallToolRequestParams, CallToolResult},
};

#[macros::mcp_tool(
    name = "inspect_dom",
    description = "Returns the page's HTML (open shadow roots as <template shadowrootmode>, same-origin iframes appended after <!-- iframe src: -->; at most 100000 characters), or with 'query' the first match and its context. To find elements to act on, prefer 'snapshot'."
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct InspectDomTool {
    /// Instance id; omit for the default instance.
    pub instance_id: Option<String>,
    /// Tab id; omit for the active tab.
    pub tab_id: Option<String>,
    /// Case-sensitive text to find: returns its context instead of the whole HTML.
    pub query: Option<String>,
    /// Characters of context before the match, at most 10000. Default: 200.
    pub before: Option<u32>,
    /// Characters of context after the match, at most 10000. Default: 200.
    pub after: Option<u32>,
}

impl InspectDomTool {
    pub async fn handle(
        params: CallToolRequestParams,
        handler: &McpHandler,
    ) -> Result<CallToolResult, CallToolError> {
        let args: InspectDomTool = crate::handler::parse_args(params)?;
        let target =
            crate::handler::resolve_target(handler, args.instance_id.clone(), args.tab_id.clone())
                .await?;
        let (before, after) = args.context();
        let expression = crate::util::js::dom_query(args.query.as_deref(), before, after);
        let outcome = crate::handler::snippet_value(&target, &expression).await?;
        let report = crate::util::text::dom_report(&outcome, args.query.as_deref(), before, after);
        Ok(CallToolResult::text_content(vec![report.into()]))
    }

    /// Characters of context before and after a match (defaults 200).
    pub(crate) fn context(&self) -> (usize, usize) {
        let chars = |n: Option<u32>| usize::try_from(n.unwrap_or(200)).unwrap_or(usize::MAX);
        (chars(self.before), chars(self.after))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_mcp_sdk::schema::CallToolRequestParams;
    use serde_json::json;

    #[tokio::test]
    async fn test_inspect_dom_params_deserialization() {
        let params: Result<CallToolRequestParams, _> = serde_json::from_value(json!({
            "name": "inspect_dom",
            "arguments": {}
        }));
        assert!(params.is_ok());
    }

    #[tokio::test]
    async fn test_inspect_dom_tool_deserialization() {
        let tool: Result<InspectDomTool, _> = serde_json::from_value(json!({}));
        assert!(tool.is_ok());
    }

    /// A Chrome mock whose inspect_dom snippet answers `outcome`.
    async fn page_answering(
        outcome: serde_json::Value,
    ) -> &'static cdp_browser_lite::test_support::mock_devtools::MockDevTools {
        let mock = crate::handler::cdp_domains::tests::spawn_recording_mock().await;
        mock.respond_with("Runtime.evaluate", move |_| outcome.clone());
        mock
    }

    async fn inspect(
        mock: &cdp_browser_lite::test_support::mock_devtools::MockDevTools,
        args: serde_json::Value,
    ) -> Result<CallToolResult, CallToolError> {
        let handler = crate::handler::cdp_domains::tests::mock_handler(mock.http_port);
        let params = crate::handler::cdp_domains::tests::call("inspect_dom", args);
        InspectDomTool::handle(params, &handler).await
    }

    /// AUD-IN-27: the search and the cut run in the page, so only the
    /// report's text crosses the connection.
    #[tokio::test]
    async fn search_runs_in_the_page() {
        let found =
            json!({"total": 3_400_000, "found": true, "position": 7, "context": "ab needle cd"});
        let mock =
            page_answering(json!({"result": {"result": {"type": "object", "value": found}}})).await;
        let reply = inspect(mock, json!({"query": "needle", "before": 3, "after": 3}))
            .await
            .unwrap();
        let text = crate::handler::cdp_domains::tests::reply_text(&reply);
        assert!(text.starts_with("Found 'needle' at position 7."), "{text}");
        let sent = &mock.commands_named("Runtime.evaluate")[0].params["expression"];
        assert!(sent.as_str().unwrap().contains("indexOf"), "{sent}");
    }

    /// AUD-IN-23: an exception in the page is an error (Chrome used to
    /// answer an empty success).
    #[tokio::test]
    async fn page_exception_is_an_error() {
        let thrown = json!({"result": {"result": {"type": "object"}, "exceptionDetails": {"text": "Uncaught", "exception": {"description": "TypeError: Cannot read properties of null (reading 'outerHTML')"}}}});
        let mock = page_answering(thrown).await;
        let error = inspect(mock, json!({})).await.unwrap_err().to_string();
        assert!(
            error.starts_with("TypeError: Cannot read properties of null"),
            "{error}"
        );
    }
}
