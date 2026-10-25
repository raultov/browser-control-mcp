use crate::handler::{McpHandler, extract_from_value, find_line_column};
use rust_mcp_sdk::{
    macros,
    schema::{CallToolError, CallToolRequestParams, CallToolResult},
};
use serde_json::json;

#[macros::mcp_tool(
    name = "search_scripts",
    description = "Searches all cached script sources for a text pattern and returns matching locations (line and column numbers). Side effects: none (read-only query). Prerequisites: scripts must have been parsed and cached by the debugger. Returns: JSON array of matches with script ID, line/column numbers, and line preview (a line over 400 characters, e.g. minified code, only around the match, `…` where cut). Use this to locate code before setting breakpoints. Alternatives: 'set_breakpoint' for direct breakpoint placement, 'evaluate_js' for runtime code discovery."
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct SearchScriptsTool {
    /// Chrome instance id from open_instance/list_instances. Omit for the default instance.
    pub instance_id: Option<String>,
    /// The Tab ID of the target tab. Omit to use the active tab.
    pub tab_id: Option<String>,
    /// Text pattern or special command to search for. Constraints: non-empty string (empty string returns cached script count). Interactions: '@source' returns first 1000 chars of each script; 'debug' returns script lengths and errors. Defaults to: None (required).
    pub query: String,
}

impl SearchScriptsTool {
    pub async fn handle(
        params: CallToolRequestParams,
        handler: &McpHandler,
    ) -> Result<CallToolResult, CallToolError> {
        let args: SearchScriptsTool = crate::handler::parse_args(params)?;
        let session = handler.session(args.instance_id.clone()).await?;
        let debugger_state = session.debugger_state(args.tab_id.clone())?;

        // Check empty query BEFORE connecting — this is a pure state query
        if args.query.is_empty() {
            let count = debugger_state.lock().await.scripts.len();
            return Ok(CallToolResult::text_content(vec![
                format!("Total cached scripts: {}", count).into(),
            ]));
        }

        let target = session.target(args.tab_id.clone()).await?;
        let scripts = debugger_state.lock().await.scripts.clone();
        let mut report = SearchReport::default();
        for (script_id, script) in scripts {
            let reply = target
                .send_raw_command("Debugger.getScriptSource", json!({"scriptId": script_id}))
                .await;
            match reply {
                Ok(reply) => match extract_from_value(&reply.result, "scriptSource") {
                    Some(source) => {
                        let offset = |n: i32| u32::try_from(n).unwrap_or(0);
                        let text = ScriptText {
                            id: &script_id,
                            hash: &script.hash,
                            source,
                            start: (offset(script.start_line), offset(script.start_column)),
                        };
                        report.add_source(&args.query, &text)
                    }
                    None => report.errors.push(format!(
                        "No scriptSource for {}: {:?}",
                        script_id, reply.result
                    )),
                },
                Err(e) => report.add_error(&script_id, format!("{:?}", e)),
            }
        }
        Ok(CallToolResult::text_content(vec![
            report.text(&args.query).into(),
        ]))
    }
}

/// One script's id, content hash and source text.
#[derive(Clone, Copy)]
pub(crate) struct ScriptText<'a> {
    pub(crate) id: &'a str,
    pub(crate) hash: &'a str,
    pub(crate) source: &'a str,
    /// Where the source starts in its resource (line, column): an inline
    /// script's source is its body only, while breakpoints by URL count
    /// lines of the whole document. `(0, 0)` for a whole resource.
    pub(crate) start: (u32, u32),
}

/// Characters of a long line `search_scripts` previews on each side of a
/// match: a minified script is one line of megabytes.
const PREVIEW_CONTEXT: usize = 100;

/// The line of `query`'s first match in `source`: the whole line, trimmed,
/// or for a line over `4 * PREVIEW_CONTEXT` characters the match with
/// [`PREVIEW_CONTEXT`] characters on each side, `…` where it was cut.
fn line_preview(source: &str, query: &str) -> String {
    let at = source.find(query).unwrap_or_default();
    let start = source[..at].rfind('\n').map_or(0, |i| i + 1);
    let end = source[at..].find('\n').map_or(source.len(), |i| at + i);
    let line = &source[start..end];
    if line.chars().count() <= 4 * PREVIEW_CONTEXT {
        return line.trim().to_string();
    }
    let keep_after = query.chars().count() + PREVIEW_CONTEXT;
    let ahead: Vec<char> = source[start..at]
        .chars()
        .rev()
        .take(PREVIEW_CONTEXT + 1)
        .collect();
    let behind: Vec<char> = source[at..end].chars().take(keep_after + 1).collect();
    let mut preview = String::new();
    if ahead.len() > PREVIEW_CONTEXT {
        preview.push('…');
    }
    preview.extend(ahead.iter().take(PREVIEW_CONTEXT).rev());
    preview.extend(behind.iter().take(keep_after));
    if behind.len() > keep_after {
        preview.push('…');
    }
    preview
}

/// Matches and errors of one `search_scripts` call. The query language is
/// shared by every browser: `@source` dumps the first 1000 characters of
/// each script, `debug` lists lengths, anything else is a text search
/// reporting the first match per script.
#[derive(Default)]
pub(crate) struct SearchReport {
    pub(crate) results: Vec<serde_json::Value>,
    pub(crate) errors: Vec<String>,
}

impl SearchReport {
    /// Applies `query` to one script's source.
    pub(crate) fn add_source(&mut self, query: &str, script: &ScriptText<'_>) {
        let ScriptText {
            id,
            hash,
            source,
            start: (start_line, start_column),
        } = *script;
        let row = match query {
            "@source" => {
                Some(json!({"id": id, "source": source.chars().take(1000).collect::<String>()}))
            }
            "debug" => Some(json!({"id": id, "source_len": source.len()})),
            _ => find_line_column(source, query).map(|(line, column)| {
                let preview = line_preview(source, query);
                let column = if line == 0 {
                    column.saturating_add(start_column)
                } else {
                    column
                };
                json!({
                    "scriptId": id,
                    "scriptHash": hash,
                    "lineNumber": line.saturating_add(start_line),
                    "columnNumber": column,
                    "linePreview": preview
                })
            }),
        };
        self.results.extend(row);
    }

    /// Records a failed source fetch; a script that no longer exists is not an error.
    pub(crate) fn add_error(&mut self, id: &str, message: String) {
        if !message.contains("No script for id") {
            self.errors.push(format!("Err for {}: {}", id, message));
        }
    }

    /// The tool's text output.
    pub(crate) fn text(&self, query: &str) -> String {
        if query == "debug" {
            format!("Results: {:?}\nErrors: {:?}", self.results, self.errors)
        } else if self.results.is_empty() && self.errors.is_empty() {
            "No matches found.".to_string()
        } else if self.results.is_empty() {
            format!("No matches found. Errors encountered: {:?}", self.errors)
        } else {
            serde_json::to_string_pretty(&self.results).unwrap_or_default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::ScriptInfo;
    use crate::handler::cdp_domains::tests::spawn_mock_chrome_server;
    use crate::handler::chrome_instance::MockChromeManager;
    use rust_mcp_sdk::schema::CallToolRequestParams;
    use serde_json::json;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    #[tokio::test]
    async fn test_search_scripts_empty_query_returns_cached_count() {
        let handler = McpHandler::new_test();

        // Prepopulate 3 scripts in state
        {
            let mut st = handler.default_session.debugger_state.lock().await;
            for i in 0..3 {
                st.scripts.insert(
                    format!("script-{}", i),
                    ScriptInfo {
                        hash: format!("hash-{}", i),
                        start_line: 0,
                        start_column: 0,
                        url: String::new(),
                    },
                );
            }
        }

        let params: CallToolRequestParams = serde_json::from_value(json!({
            "name": "search_scripts",
            "arguments": {
                "query": ""
            }
        }))
        .unwrap();

        let result = SearchScriptsTool::handle(params, &handler).await;
        assert!(result.is_ok());
        let res = result.unwrap();
        let res_json = serde_json::to_value(&res).unwrap();
        let text = res_json["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("Total cached scripts: 3"));
    }

    #[tokio::test]
    async fn test_search_scripts_empty_query_with_no_scripts() {
        let handler = McpHandler::new_test();
        let params: CallToolRequestParams = serde_json::from_value(json!({
            "name": "search_scripts",
            "arguments": {
                "query": ""
            }
        }))
        .unwrap();

        let result = SearchScriptsTool::handle(params, &handler).await;
        assert!(result.is_ok());
        let res = result.unwrap();
        let res_json = serde_json::to_value(&res).unwrap();
        let text = res_json["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("Total cached scripts: 0"));
    }

    #[tokio::test]
    async fn test_search_scripts_missing_query_fails_deserialization() {
        let handler = McpHandler::new_test();
        let params: CallToolRequestParams = serde_json::from_value(json!({
            "name": "search_scripts",
            "arguments": {}
        }))
        .unwrap();

        let result = SearchScriptsTool::handle(params, &handler).await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("missing field `query`")
        );
    }

    #[tokio::test]
    async fn test_search_scripts_handle() {
        let port = spawn_mock_chrome_server().await;

        let mut handler = McpHandler::new_test();
        Arc::get_mut(&mut handler.default_session)
            .unwrap()
            .chrome_manager = Arc::new(Mutex::new(MockChromeManager::new(port)));

        {
            let mut st = handler.default_session.debugger_state.lock().await;
            st.scripts.insert(
                "mock-script-id".to_string(),
                ScriptInfo {
                    hash: "mock-hash".to_string(),
                    start_line: 0,
                    start_column: 0,
                    url: String::new(),
                },
            );
        }

        let params: CallToolRequestParams = serde_json::from_value(json!({
            "name": "search_scripts",
            "arguments": {
                "query": "something"
            }
        }))
        .unwrap();

        let result = SearchScriptsTool::handle(params, &handler).await;
        assert!(result.is_ok(), "Handle should succeed: {:?}", result.err());

        let call_result = result.unwrap();
        assert!(!call_result.content.is_empty());
    }

    /// Registers a real tab over the mock DevTools server and returns its id,
    /// so tests can exercise `session.debugger_state(tab_id)` resolution.
    async fn register_mock_tab(handler: &crate::handler::McpHandler) -> String {
        use cdp_browser_lite::BrowserClient;
        use std::time::Duration;

        let session = handler.session(None).await.expect("default session");
        let port = crate::handler::cdp_domains::tests::spawn_mock_chrome_server().await;
        let browser =
            BrowserClient::connect(&format!("127.0.0.1:{}", port), Duration::from_secs(5))
                .await
                .expect("BrowserClient connect to mock");
        let tab = browser.attach("T-page-1").await.expect("attach to mock");
        session
            .tabs
            .write()
            .unwrap()
            .register_tab(
                tab,
                crate::handler::chrome_instance::tab_registry::NewTab::new(
                    None,
                    "https://example.test",
                ),
                false,
            )
            .expect("register tab")
    }

    #[tokio::test]
    async fn test_search_scripts_reads_tab_state_not_session_state() {
        let handler = McpHandler::new_test();
        let session = handler.session(None).await.unwrap();

        // Session-level decoy scripts that must NOT be picked up.
        {
            let st = session.debugger_state(None).unwrap();
            let mut st = st.lock().await;
            st.scripts.insert(
                "SESSION-script".to_string(),
                ScriptInfo {
                    hash: "session-hash".to_string(),
                    start_line: 0,
                    start_column: 0,
                    url: String::new(),
                },
            );
        }

        // Tab-level script that MUST be picked up.
        let tab_id = register_mock_tab(&handler).await;
        let tab_state = session.debugger_state(Some(tab_id.clone())).unwrap();
        {
            let mut st = tab_state.lock().await;
            st.scripts.insert(
                "TAB-script".to_string(),
                ScriptInfo {
                    hash: "tab-hash".to_string(),
                    start_line: 0,
                    start_column: 0,
                    url: String::new(),
                },
            );
        }

        let params: CallToolRequestParams = serde_json::from_value(json!({
            "name": "search_scripts",
            "arguments": {
                "tab_id": tab_id,
                "query": ""
            }
        }))
        .unwrap();

        let result = SearchScriptsTool::handle(params, &handler)
            .await
            .expect("empty query is a pure state read");
        let res_json = serde_json::to_value(&result).unwrap();
        let text = res_json["content"][0]["text"].as_str().unwrap();
        assert!(
            text.contains("Total cached scripts: 1"),
            "must count only the TAB script, got: {text}"
        );
    }

    /// AUD-DB-08: a match in a long line (minified code) previews only the
    /// text around it, marked as cut, never the whole line.
    #[test]
    fn long_line_preview_is_a_window_around_the_match() {
        // Given a 1.6 MB single-line script with a match in the middle
        let filler = "é".repeat(800_000);
        let source = format!("{filler}needle(){filler}\nnext();");
        let script = ScriptText {
            id: "1",
            hash: "h",
            source: &source,
            start: (0, 0),
        };
        let mut report = SearchReport::default();
        // When it is searched
        report.add_source("needle", &script);
        // Then the preview holds the match with PREVIEW_CONTEXT characters on each side
        let preview = report.results[0]["linePreview"].as_str().unwrap();
        let before = "é".repeat(PREVIEW_CONTEXT);
        let after = "é".repeat(PREVIEW_CONTEXT - 2);
        assert_eq!(preview, format!("…{before}needle(){after}…"));
        assert_eq!(report.results[0]["columnNumber"], 800_000);
        // And a short line is still shown whole
        let short = ScriptText {
            source: "a();\n  needle(1);\n",
            ..script
        };
        report.add_source("needle", &short);
        assert_eq!(report.results[1]["linePreview"], "needle(1);");
    }

    /// AUD-DB-02: matches in an inline script are reported in document
    /// lines (where `set_breakpoint` by URL counts), not script lines.
    #[test]
    fn inline_script_matches_are_in_document_lines() {
        // Given an inline script whose body starts at line 14, column 8 of the page
        let source = "\nfunction inlineTarget(v) {\n  const w = v + 100;\n}";
        let script = ScriptText {
            id: "45",
            hash: "h",
            source,
            start: (14, 8),
        };
        let mut report = SearchReport::default();
        // When a line below the first and a match on the first line are searched
        report.add_source("const w", &script);
        let first_line = ScriptText {
            source: "var x = 1;",
            start: (3, 8),
            ..script
        };
        report.add_source("x =", &first_line);
        // Then lines are shifted by the start line, and only the first line's
        // columns by the start column
        assert_eq!(
            (
                &report.results[0]["lineNumber"],
                &report.results[0]["columnNumber"]
            ),
            (&json!(16), &json!(2))
        );
        assert_eq!(report.results[0]["linePreview"], "const w = v + 100;");
        assert_eq!(
            (
                &report.results[1]["lineNumber"],
                &report.results[1]["columnNumber"]
            ),
            (&json!(3), &json!(12))
        );
    }

    #[tokio::test]
    async fn test_search_scripts_unknown_tab_errors() {
        let handler = McpHandler::new_test();
        let params: CallToolRequestParams = serde_json::from_value(json!({
            "name": "search_scripts",
            "arguments": { "query": "", "tab_id": "tab-nope" }
        }))
        .unwrap();
        let err = SearchScriptsTool::handle(params, &handler)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("not found"),
            "expected tab-not-found error, got: {err}"
        );
    }
}
