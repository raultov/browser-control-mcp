use crate::handler::McpHandler;
use crate::util::outline::FIND_MAX_MATCHES;
use rust_mcp_sdk::{
    macros,
    schema::{CallToolError, CallToolRequestParams, CallToolResult},
};
use serde_json::Value;

#[macros::mcp_tool(
    name = "find_in_snapshot",
    description = "Searches the whole 'snapshot' outline (never cut) and returns only the matching lines under their ancestors, with their refs (at most 20 matches). Cheaper than 'snapshot' on a large page."
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct FindInSnapshotTool {
    /// Instance id; omit for the default instance.
    pub instance_id: Option<String>,
    /// Tab id; omit for the active tab.
    pub tab_id: Option<String>,
    /// Text to find in the lines, case-insensitive. Give this or 'regex'.
    pub text: Option<String>,
    /// JavaScript regular expression, case-sensitive. Give this or 'text'.
    pub regex: Option<String>,
}

impl FindInSnapshotTool {
    /// The search, or why the arguments make no sense.
    fn needle(&self) -> Result<String, CallToolError> {
        match (&self.text, &self.regex) {
            (Some(text), None) => Ok(format!("{text:?}")),
            (None, Some(regex)) => Ok(format!("/{regex}/")),
            _ => Err(CallToolError::from_message(
                "find_in_snapshot needs exactly one of 'text' or 'regex'",
            )),
        }
    }

    /// Checks the arguments (before any browser work).
    pub(crate) fn check_arguments(&self) -> Result<(), CallToolError> {
        self.needle().map(|_| ())
    }

    /// The JavaScript that searches the outline (shared by every browser).
    pub(crate) fn expression(&self) -> String {
        crate::util::outline::find(self.text.as_deref(), self.regex.as_deref())
    }

    /// The reply from the search (shared by every browser).
    pub(crate) fn reply(&self, outcome: &Value) -> Result<CallToolResult, CallToolError> {
        let needle = self.needle()?;
        let url = outcome["url"].as_str().unwrap_or_default();
        let matches = outcome["matches"].as_u64().unwrap_or(0);
        let reply = if matches == 0 {
            format!("No line of the snapshot of {url} matches {needle}.")
        } else {
            let shown = matches.min(FIND_MAX_MATCHES as u64);
            format!(
                "{matches} matches for {needle} in the snapshot of {url} (showing {shown}), each under its ancestors:\n{}",
                outcome["text"].as_str().unwrap_or_default()
            )
        };
        Ok(CallToolResult::text_content(vec![reply.into()]))
    }

    pub async fn handle(
        params: CallToolRequestParams,
        handler: &McpHandler,
    ) -> Result<CallToolResult, CallToolError> {
        let args: FindInSnapshotTool = crate::handler::parse_args(params)?;
        args.check_arguments()?;
        let target =
            crate::handler::resolve_target(handler, args.instance_id.clone(), args.tab_id.clone())
                .await?;
        let outcome = crate::handler::snippet_value(&target, &args.expression()).await?;
        args.reply(&outcome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool(args: Value) -> FindInSnapshotTool {
        serde_json::from_value(args).unwrap()
    }

    /// ISSUE-4: exactly one of 'text' or 'regex'.
    #[test]
    fn one_search_is_required() {
        for args in [json!({}), json!({"text": "a", "regex": "b"})] {
            let error = tool(args).check_arguments().unwrap_err().to_string();
            assert!(
                error.contains("exactly one of 'text' or 'regex'"),
                "{error}"
            );
        }
        assert!(tool(json!({"regex": "^button"})).check_arguments().is_ok());
    }

    /// ISSUE-4: matches come under their ancestors with a count; no match
    /// is an answer, not an error.
    #[test]
    fn reply_counts_and_shows_the_matches() {
        let lines = "- list [ref=e3]\n  - listitem [ref=e9]\n    - button \"Buy now\" [ref=e12]";
        let found = json!({"status": "ok", "url": "http://a/", "matches": 31, "text": lines});
        let reply = tool(json!({"text": "buy"})).reply(&found).unwrap();
        let text = crate::handler::cdp_domains::tests::reply_text(&reply);
        assert!(
            text.starts_with("31 matches for \"buy\" in the snapshot of http://a/ (showing 20), each under its ancestors:\n"),
            "{text}"
        );
        assert!(text.ends_with(lines), "{text}");
        let none = json!({"status": "ok", "url": "http://a/", "matches": 0, "text": ""});
        let reply = tool(json!({"regex": "^x"})).reply(&none).unwrap();
        assert_eq!(
            crate::handler::cdp_domains::tests::reply_text(&reply),
            "No line of the snapshot of http://a/ matches /^x/."
        );
    }
}
