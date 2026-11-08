use crate::handler::{McpHandler, extract_from_value, find_line_column};
use rust_mcp_sdk::{
    macros,
    schema::{CallToolError, CallToolRequestParams, CallToolResult},
};
use serde_json::json;

#[macros::mcp_tool(
    name = "search_scripts",
    description = "Searches the tab's loaded scripts for a text and returns each match's script id, hash, 0-based line and column and a preview, to find where to set a breakpoint. Scripts of frames of another origin are searched too (on Chrome their ids name the frame: '2.17' is frame 2's script 17)."
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct SearchScriptsTool {
    /// Default: the default instance.
    pub instance_id: Option<String>,
    /// Default: the active tab.
    pub tab_id: Option<String>,
    /// Text to find. '' returns the script count, '@source' the first 1000 characters of each script, 'debug' their lengths and errors.
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
        // Each script is read in its session: the tab's, or its frame's.
        let scripts: Vec<_> = {
            let state = debugger_state.lock().await;
            let scripts = state.scripts.clone().into_iter();
            scripts
                .filter_map(|(id, script)| Some((state.script_target(&id, &target)?, id, script)))
                .collect()
        };
        let mut report = SearchReport::default();
        let mut known = std::collections::HashSet::new();
        for ((on, raw), script_id, script) in scripts {
            let reply = on
                .send_raw_command("Debugger.getScriptSource", json!({"scriptId": raw}))
                .await;
            match reply {
                Ok(reply) => {
                    let source = extract_from_value(&reply.result, "scriptSource");
                    let read = (script_id.as_str(), &script, source);
                    if let Some(source) = report.add_read(&args.query, read, &reply.result) {
                        known.insert(source);
                    }
                }
                Err(e) => report.add_error(&script_id, format!("{:?}", e)),
            }
        }
        if !matches!(args.query.as_str(), "debug" | "@source")
            && let Ok(page) = crate::handler::snippet_value(&target, INLINE_SCRIPTS).await
        {
            report.add_released_inline(&args.query, &page, &known);
        }
        Ok(CallToolResult::text_content(vec![
            report.text(&args.query).into(),
        ]))
    }
}

/// The page's inline scripts: `{url, texts}`.
const INLINE_SCRIPTS: &str = "({ url: location.href, texts: Array.from(document.scripts).filter(s => !s.src).map(s => s.text) })";

/// What a match in an inline script the debugger no longer holds says.
const RELEASED_NOTE: &str = "an inline script of the page that the debugger no longer holds (it already ran and was released, as after coming back to the page from the back-forward cache): reload the page to set a breakpoint in it; the line counts from the script's first line";

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

    /// Applies `query` to the source the debugger gave for a cached script
    /// (`read`: its id, its info, its source if any; `result`: the reply,
    /// quoted when it has no source); the source, when there is one.
    fn add_read(
        &mut self,
        query: &str,
        (id, script, source): (&str, &crate::handler::ScriptInfo, Option<&str>),
        result: &Option<serde_json::Value>,
    ) -> Option<String> {
        let Some(source) = source else {
            self.errors
                .push(format!("No scriptSource for {id}: {result:?}"));
            return None;
        };
        let offset = |n: i32| u32::try_from(n).unwrap_or(0);
        let text = ScriptText {
            id,
            hash: &script.hash,
            source,
            start: (offset(script.start_line), offset(script.start_column)),
        };
        self.add_source(query, &text);
        Some(source.to_string())
    }

    /// Searches the page's inline scripts (`page`: [`INLINE_SCRIPTS`]'s
    /// answer) whose text is none of the debugger's `known` sources: Chrome
    /// does not announce again a script released while its page sat in the
    /// back-forward cache, though its `<script>` is still there (ISSUE-62).
    pub(crate) fn add_released_inline(
        &mut self,
        query: &str,
        page: &serde_json::Value,
        known: &std::collections::HashSet<String>,
    ) {
        let url = page["url"].as_str().unwrap_or_default();
        let texts = page["texts"].as_array().into_iter().flatten();
        for text in texts.filter_map(serde_json::Value::as_str) {
            if known.contains(text) {
                continue;
            }
            if let Some((line, column)) = find_line_column(text, query) {
                self.results.push(json!({
                    "scriptId": null,
                    "url": url,
                    "lineInScript": line,
                    "columnNumber": column,
                    "linePreview": line_preview(text, query),
                    "note": RELEASED_NOTE,
                }));
            }
        }
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
            let report = json!({"scripts": self.results, "errors": self.errors});
            serde_json::to_string_pretty(&report).unwrap_or_default()
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

    /// ISSUE-66: the `debug` report is JSON, as every other reply (it was
    /// Rust's `Debug` format: `Object {"id": String("5")}`).
    #[test]
    fn debug_report_is_json() {
        // Given one script read and one that failed
        let mut report = SearchReport::default();
        let script = ScriptText {
            id: "5",
            hash: "h",
            source: "let a = 1;",
            start: (0, 0),
        };
        report.add_source("debug", &script);
        report.add_error("6", "boom".into());
        // When the report is written
        let text = report.text("debug");
        // Then it is JSON with the lengths and errors
        let body: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(body["scripts"], json!([{"id": "5", "source_len": 10}]));
        assert_eq!(body["errors"], json!(["Err for 6: boom"]));
    }
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
                        end_line: 0,
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

    /// ISSUE-62 (Chrome): an inline script the debugger no longer holds
    /// (it ran and was released while its page sat in the back-forward
    /// cache: Chrome does not announce it again) is still found in the
    /// page's `<script>` elements, marked so; one the debugger holds is
    /// reported once, from the debugger.
    #[tokio::test]
    async fn released_inline_scripts_are_found_in_the_page() {
        use crate::handler::cdp_domains::tests::{mock_handler, reply_text, spawn_recording_mock};
        let mock = spawn_recording_mock().await;
        mock.respond_with(
            "Debugger.getScriptSource",
            |_| json!({"result": {"scriptSource": "function keep() { return 'hello-kept'; }"}}),
        );
        mock.respond_with("Runtime.evaluate", |_| {
            let page = json!({"url": "http://a/", "texts": ["function keep() { return 'hello-kept'; }", "\nconsole.log('hello-released');"]});
            json!({"result": {"result": {"type": "object", "value": page}}})
        });
        let handler = mock_handler(mock.http_port);
        let script = ScriptInfo {
            hash: "h".into(),
            start_line: 3,
            start_column: 8,
            end_line: 3,
            url: "http://a/".into(),
        };
        handler
            .default_session
            .debugger_state
            .lock()
            .await
            .scripts
            .insert("7".into(), script);
        let search = |query: &str| {
            let params =
                crate::handler::cdp_domains::tests::call("search_scripts", json!({"query": query}));
            SearchScriptsTool::handle(params, &handler)
        };
        let found: serde_json::Value =
            serde_json::from_str(&reply_text(&search("hello").await.unwrap())).unwrap();
        let rows = found.as_array().unwrap();
        assert_eq!(rows.len(), 2, "{found}");
        assert_eq!(rows[0]["scriptId"], "7");
        let released = &rows[1];
        assert_eq!(released["scriptId"], serde_json::Value::Null);
        assert_eq!(
            (released["url"].clone(), released["lineInScript"].clone()),
            (json!("http://a/"), json!(1))
        );
        assert!(
            released["note"]
                .as_str()
                .unwrap()
                .contains("reload the page"),
            "{released}"
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
                    end_line: 0,
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
                    end_line: 0,
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
                    end_line: 0,
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
