//! End-to-end smoke (specs 10 §4): drives the built binary over stdio MCP
//! against real browsers. Ignored in `make check`; `tests/run_all_e2e.sh`
//! runs it with `--ignored`. Pages are served on 127.0.0.1 by the test.
//!
//! Environment: `BCM_E2E_ARGS` (extra server arguments, space separated),
//! `BCM_E2E_BIN` (binary path, default `target/debug/browser-control-mcp`),
//! `BCM_E2E_ATTACH_CHROME` / `BCM_E2E_ATTACH_FIREFOX` (server arguments that
//! attach to a browser started by `tests/chrome.sh` / `tests/firefox.sh`;
//! the attach suites SKIP when unset).

// One cfg(test) module: clippy then treats the helpers as test code
// (unwrap/expect allowed), as in unit tests.
#[cfg(test)]
mod e2e {
    use serde_json::{Value, json};
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    use tokio::process::{Child, ChildStdin, ChildStdout, Command};

    const INDEX: &str = r#"<!doctype html><html><body>
    <button id="go" onclick="window.__clicked=(window.__clicked||0)+1">go</button>
    <button id="hit" onclick="appTarget(3)">hit</button>
    <button id="ask" onclick="window.__answer = confirm('Sure?')">ask</button>
    <input id="q" value="old"><div id="marker">needle-marker</div>
    <script>console.log("hello from e2e");</script><script>throw new Error("boom-e2e");</script><script src="/app.js"></script></body></html>"#;
    const APP_JS: &str = "function appTarget(x) {\n  const y = x * 2;\n  return y + 1;\n}\nwindow.appTarget = appTarget;\nfetch('/api');\n";

    /// The WebSocket page: sends `hello-ws` to the echo server on `port`.
    fn ws_page(port: u16) -> String {
        format!(
            "<!doctype html><html><body><script>const ws = new WebSocket('ws://127.0.0.1:{port}/'); \
             ws.onopen = () => ws.send('hello-ws'); ws.onmessage = e => console.log('ws got ' + e.data);</script></body></html>"
        )
    }

    /// Echoes the text messages of one WebSocket client.
    async fn echo(socket: tokio::net::TcpStream) {
        use futures_util::{SinkExt, StreamExt};
        let Ok(mut ws) = tokio_tungstenite::accept_async(socket).await else {
            return;
        };
        while let Some(Ok(message)) = ws.next().await {
            if message.is_text() && ws.send(message).await.is_err() {
                break;
            }
        }
    }

    /// A WebSocket echo server on 127.0.0.1; returns its port.
    async fn serve_ws() -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                tokio::spawn(echo(socket));
            }
        });
        port
    }

    /// The content type and body served for `path`.
    fn page(path: &str, ws: &str) -> (&'static str, String) {
        match path {
            p if p.starts_with("/app.js") => ("application/javascript", APP_JS.to_string()),
            p if p.starts_with("/api") => ("application/json", "{\"ok\":true}".to_string()),
            p if p.starts_with("/ws.html") => ("text/html", ws.to_string()),
            _ => ("text/html", INDEX.to_string()),
        }
    }

    /// Answers one HTTP request.
    async fn respond(mut socket: tokio::net::TcpStream, ws: String) {
        let mut buf = vec![0u8; 4096];
        let n = socket.read(&mut buf).await.unwrap_or(0);
        let request = String::from_utf8_lossy(&buf[..n]).to_string();
        let path = request.split_whitespace().nth(1).unwrap_or("/").to_string();
        let (kind, body) = page(&path, &ws);
        let reply = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = socket.write_all(reply.as_bytes()).await;
    }

    /// A tiny HTTP server for the test pages (and the WebSocket echo
    /// behind `/ws.html`); returns its base URL.
    async fn serve() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let ws = ws_page(serve_ws().await);
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                tokio::spawn(respond(socket, ws.clone()));
            }
        });
        base
    }

    /// `(label, tool, arguments, expected text)`. A tool named `sleep`
    /// waits; an expected text starting with `!` expects a tool error that
    /// contains the rest; `$TAB` in the arguments is the last opened tab.
    type Step = (&'static str, &'static str, Value, &'static str);

    /// One MCP client session over the child's stdio.
    struct Mcp {
        child: Child,
        stdin: ChildStdin,
        stdout: BufReader<ChildStdout>,
        next_id: u64,
        failures: Vec<String>,
        last_tab: String,
    }

    impl Mcp {
        async fn start(extra: &[&str]) -> Self {
            let bin = std::env::var("BCM_E2E_BIN")
                .unwrap_or_else(|_| "target/debug/browser-control-mcp".into());
            let env_args = std::env::var("BCM_E2E_ARGS").unwrap_or_default();
            let mut child = Command::new(bin)
                .args(env_args.split_whitespace())
                .args(extra)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .expect("start the server binary");
            let stdin = child.stdin.take().unwrap();
            let stdout = BufReader::new(child.stdout.take().unwrap());
            let mut mcp = Self {
                child,
                stdin,
                stdout,
                next_id: 0,
                failures: Vec::new(),
                last_tab: String::new(),
            };
            let init = json!({"protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": {"name": "e2e", "version": "0"}});
            mcp.request("initialize", init).await;
            mcp.notify("notifications/initialized").await;
            mcp
        }

        async fn notify(&mut self, method: &str) {
            let line = json!({"jsonrpc": "2.0", "method": method}).to_string() + "\n";
            self.stdin.write_all(line.as_bytes()).await.unwrap();
        }

        async fn request(&mut self, method: &str, params: Value) -> Value {
            self.next_id += 1;
            let id = self.next_id;
            let line = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
                .to_string()
                + "\n";
            self.stdin.write_all(line.as_bytes()).await.unwrap();
            loop {
                let mut text = String::new();
                let read =
                    tokio::time::timeout(Duration::from_secs(90), self.stdout.read_line(&mut text))
                        .await;
                let Ok(Ok(n)) = read else {
                    return json!({"error": "no reply"});
                };
                if n == 0 {
                    return json!({"error": "server closed stdout"});
                }
                if let Ok(message) = serde_json::from_str::<Value>(&text)
                    && message["id"] == json!(id)
                {
                    return message;
                }
            }
        }

        /// Calls a tool; returns (ok, first text block or the whole reply).
        async fn call(&mut self, name: &str, arguments: Value) -> (bool, String) {
            let reply = self
                .request("tools/call", json!({"name": name, "arguments": arguments}))
                .await;
            let result = &reply["result"];
            let ok = reply.get("error").is_none() && result["isError"] != json!(true);
            let text = result["content"][0]["text"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            let text = if text.is_empty() {
                reply.to_string()
            } else {
                text
            };
            (ok, text)
        }

        /// Records one PASS/FAIL line.
        fn report(&mut self, label: &str, passed: bool, text: &str) {
            println!("{} {label}", if passed { "PASS" } else { "FAIL" });
            if !passed {
                println!("      {}", text.chars().take(300).collect::<String>());
                self.failures.push(label.to_string());
            }
        }

        /// One step: PASS when the outcome and the text match `expected`.
        async fn step(&mut self, (label, name): (&str, &str), args: Value, expected: &str) {
            let args = args.to_string().replace("$TAB", &self.last_tab);
            let expected = expected.replace("$TAB", &self.last_tab);
            let (ok, text) = self.call(name, serde_json::from_str(&args).unwrap()).await;
            let passed = match expected.strip_prefix('!') {
                Some(error) => !ok && text.contains(error),
                None => ok && text.contains(&expected),
            };
            if name == "open_tab"
                && let Ok(reply) = serde_json::from_str::<Value>(&text)
            {
                self.last_tab = reply["tab_id"].as_str().unwrap_or_default().to_string();
            }
            self.report(label, passed, &text);
        }

        /// Runs the steps in order.
        async fn steps(&mut self, steps: Vec<Step>) {
            for (label, name, args, expected) in steps {
                if name == "sleep" {
                    tokio::time::sleep(Duration::from_millis(1500)).await;
                    continue;
                }
                self.step((label, name), args, expected).await;
            }
        }

        /// Step 1: the tool list has the expected size.
        async fn list_tools(&mut self, expected: usize) {
            let tools = self.request("tools/list", json!({})).await;
            let count = tools["result"]["tools"]
                .as_array()
                .map(Vec::len)
                .unwrap_or(0);
            let label = format!("tools/list ({count} tools)");
            self.report(&label, count == expected, "");
        }

        /// Closes stdin and waits for the server to exit.
        async fn finish(mut self) -> Vec<String> {
            drop(self.stdin);
            let _ = tokio::time::timeout(Duration::from_secs(30), self.child.wait()).await;
            self.failures
        }
    }

    /// Instance, tab and navigation steps (launched mode).
    fn page_steps(browser: &str, base: &str) -> Vec<Step> {
        let i = json!("e2e");
        let url = format!("{base}/");
        vec![
            (
                "open_instance",
                "open_instance",
                json!({"browser": browser, "label": "e2e"}),
                "e2e",
            ),
            (
                "navigate",
                "navigate",
                json!({"instance_id": i, "url": url}),
                "Navigated to",
            ),
            (
                "reload",
                "reload",
                json!({"instance_id": i}),
                "Page reloaded",
            ),
            (
                "NAV-03 unknown host fails",
                "navigate",
                json!({"instance_id": i, "url": "http://nonexistent-host.invalid/"}),
                "!Navigation failed",
            ),
            (
                "WIN-04 window is 1920 wide",
                "evaluate_js",
                json!({"instance_id": i, "expression": "innerWidth >= 1880 && innerWidth <= 1920"}),
                "\"value\": true",
            ),
        ]
    }

    /// Tab steps: open the local page in a new tab and make it active.
    fn tab_steps(instance: &str, base: &str) -> Vec<Step> {
        let i = json!(instance);
        let url = format!("{base}/");
        vec![
            (
                "open_tab",
                "open_tab",
                json!({"instance_id": i, "url": url, "label": "t"}),
                "tab_id",
            ),
            (
                "list_tabs",
                "list_tabs",
                json!({"instance_id": i}),
                "\"label\":\"t\"",
            ),
            (
                "switch_tab",
                "switch_tab",
                json!({"instance_id": i, "tab_id": "$TAB"}),
                "$TAB",
            ),
            ("wait for the page", "sleep", json!({}), ""),
        ]
    }

    /// Runtime and input steps on the active page.
    fn runtime_steps(instance: &str) -> Vec<Step> {
        let i = json!(instance);
        let effects = "document.querySelector('#q').value + window.__clicked";
        vec![
            (
                "evaluate_js",
                "evaluate_js",
                json!({"instance_id": i, "expression": "({sum: 1+2})"}),
                "\"sum\": 3",
            ),
            (
                "fill_input",
                "fill_input",
                json!({"instance_id": i, "selector": "#q", "text": "typed"}),
                "Successfully",
            ),
            (
                "click_element",
                "click_element",
                json!({"instance_id": i, "selector": "#go"}),
                "clicked",
            ),
            (
                "input effects",
                "evaluate_js",
                json!({"instance_id": i, "expression": effects}),
                "typed1",
            ),
        ]
    }

    /// ISSUE-11 and ISSUE-2: a click that opens a dialog, then content that
    /// arrives later, waited for.
    fn dialog_and_wait_steps(instance: &str) -> Vec<Step> {
        let i = json!(instance);
        let late = "setTimeout(() => document.body.insertAdjacentHTML('beforeend', '<p id=\"late\">late-arrival</p>'), 700); 1";
        vec![
            (
                "ISSUE-11 click that opens a dialog succeeds, noting it",
                "click_element",
                json!({"instance_id": i, "selector": "#ask"}),
                r#"confirm dialog ("Sure?")"#,
            ),
            (
                "handle_dialog",
                "handle_dialog",
                json!({"instance_id": i, "accept": true}),
                "Dialog accepted",
            ),
            (
                "dialog answer",
                "evaluate_js",
                json!({"instance_id": i, "expression": "window.__answer"}),
                "true",
            ),
            (
                "ISSUE-2 content arrives later",
                "evaluate_js",
                json!({"instance_id": i, "expression": late}),
                "\"value\": 1",
            ),
            (
                "ISSUE-2 wait_for text",
                "wait_for",
                json!({"instance_id": i, "text": "late-arrival"}),
                "Waited",
            ),
            (
                "ISSUE-2 wait_for selector",
                "wait_for",
                json!({"instance_id": i, "selector": "#late"}),
                "Waited",
            ),
            (
                "ISSUE-2 wait_for times out naming what is missing",
                "wait_for",
                json!({"instance_id": i, "text": "never-there", "timeout_ms": 300}),
                "!waiting for the text \"never-there\" to appear",
            ),
        ]
    }

    /// Observation steps: console, network, screenshot, DOM.
    fn observation_steps(instance: &str) -> Vec<Step> {
        let i = json!(instance);
        vec![
            (
                "console log",
                "get_console_logs",
                json!({"instance_id": i}),
                "hello from e2e",
            ),
            (
                "console error",
                "get_console_logs",
                json!({"instance_id": i}),
                "boom-e2e",
            ),
            (
                "network",
                "get_network_logs",
                json!({"instance_id": i, "url_filter": "/api"}),
                "200",
            ),
            (
                "metrics",
                "get_performance_metrics",
                json!({"instance_id": i}),
                "Nodes",
            ),
            // Base64 of the PNG signature.
            (
                "screenshot is a PNG",
                "capture_screenshot",
                json!({"instance_id": i}),
                "iVBORw0KGgo",
            ),
            (
                "inspect_dom",
                "inspect_dom",
                json!({"instance_id": i, "query": "needle"}),
                "Found",
            ),
        ]
    }

    /// FWS: the WebSocket page's frames reach get_network_logs.
    fn websocket_steps(instance: &str, base: &str) -> Vec<Step> {
        let i = json!(instance);
        vec![
            (
                "navigate to the WebSocket page",
                "navigate",
                json!({"instance_id": i, "url": format!("{base}/ws.html")}),
                "ws.html",
            ),
            ("wait for the echo", "sleep", json!({}), ""),
            (
                "websocket frames",
                "get_network_logs",
                json!({"instance_id": i, "type_filter": "websocket", "ws_content_filter": "hello-ws"}),
                "hello-ws",
            ),
            (
                "back to the test page",
                "navigate",
                json!({"instance_id": i, "url": format!("{base}/")}),
                "Navigated to",
            ),
            ("wait for the page again", "sleep", json!({}), ""),
        ]
    }

    /// Breakpoint pause, frame evaluation, step, resume, script search.
    fn debugger_steps(instance: &str, base: &str) -> Vec<Step> {
        let i = json!(instance);
        let bp = json!({"instance_id": i, "url": format!("{base}/app.js"), "line_number": 1});
        let trigger =
            json!({"instance_id": i, "expression": "setTimeout(() => appTarget(5), 50); 1"});
        vec![
            (
                "search_scripts",
                "search_scripts",
                json!({"instance_id": i, "query": "appTarget"}),
                "appTarget",
            ),
            ("set_breakpoint", "set_breakpoint", bp, "Breakpoint set"),
            ("trigger", "evaluate_js", trigger, ""),
            ("wait for the pause", "sleep", json!({}), ""),
            (
                "evaluate_on_call_frame",
                "evaluate_on_call_frame",
                json!({"instance_id": i, "expression": "x * 7"}),
                "35",
            ),
            (
                "step_over",
                "step_over",
                json!({"instance_id": i}),
                "Stepped",
            ),
            ("resume", "resume", json!({"instance_id": i}), "resumed"),
            (
                "ISSUE-1/11 click into a breakpoint answers at once, noting the pause",
                "click_element",
                json!({"instance_id": i, "selector": "#hit"}),
                "The page paused in the debugger",
            ),
            (
                "evaluate_on_call_frame after the click",
                "evaluate_on_call_frame",
                json!({"instance_id": i, "expression": "x * 7"}),
                "21",
            ),
            (
                "resume after the click",
                "resume",
                json!({"instance_id": i}),
                "resumed",
            ),
        ]
    }

    /// Steps 13–15: tracing and pause on load (Chrome only; Firefox
    /// answers a capability error), then close.
    fn closing_steps(browser: &str) -> Vec<Step> {
        let i = json!("e2e");
        let profile = json!({"instance_id": i, "duration_ms": 500});
        let close = (
            "close_instance",
            "close_instance",
            json!({"instance_id": i}),
            "\"stopped\":true",
        );
        if browser != "chrome" {
            let reload = json!({"instance_id": i, "duration_ms": 500, "action": "reload"});
            return vec![
                (
                    "FPF-02 profile_page_performance",
                    "profile_page_performance",
                    reload,
                    "first_contentful_paint_ms",
                ),
                (
                    "pause_on_load",
                    "pause_on_load",
                    json!({"instance_id": i}),
                    "!is not available on",
                ),
                close,
            ];
        }
        vec![
            (
                "profile_page_performance",
                "profile_page_performance",
                profile,
                "profile_duration_ms",
            ),
            (
                "pause_on_load",
                "pause_on_load",
                json!({"instance_id": i}),
                "paused",
            ),
            ("wait for the load pause", "sleep", json!({}), ""),
            (
                "resume after load",
                "resume",
                json!({"instance_id": i}),
                "resumed",
            ),
            close,
        ]
    }

    /// Steps 1–15 in launched mode.
    async fn smoke(browser: &str) {
        let base = serve().await;
        let mut mcp = Mcp::start(&[]).await;
        mcp.list_tools(34).await;
        let all = [
            page_steps(browser, &base),
            tab_steps("e2e", &base),
            runtime_steps("e2e"),
            dialog_and_wait_steps("e2e"),
            observation_steps("e2e"),
            websocket_steps("e2e", &base),
            debugger_steps("e2e", &base),
            closing_steps(browser),
        ];
        mcp.steps(all.concat()).await;
        let failures = mcp.finish().await;
        assert!(failures.is_empty(), "{browser}: failed steps {failures:?}");
    }

    /// Step 16: SIGTERM with an open instance, then a new server launches
    /// the same browser.
    async fn sigterm_then_relaunch(browser: &str) {
        let open = json!({"browser": browser, "label": "e2e"});
        let mut first = Mcp::start(&[]).await;
        first
            .step(("open_instance", "open_instance"), open.clone(), "e2e")
            .await;
        let pid = first.child.id().unwrap().to_string();
        let status = std::process::Command::new("kill")
            .args(["-TERM", &pid])
            .status();
        let exited = tokio::time::timeout(Duration::from_secs(15), first.child.wait()).await;
        let clean = status.is_ok_and(|s| s.success()) && exited.is_ok();
        first.report("SIGTERM stops the server", clean, "server did not exit");
        let mut failures = first.failures;

        let mut second = Mcp::start(&[]).await;
        second
            .step(("relaunch after SIGTERM", "open_instance"), open, "e2e")
            .await;
        let eval = json!({"instance_id": "e2e", "expression": "1 + 1"});
        second
            .step(("relaunched browser answers", "evaluate_js"), eval, "2")
            .await;
        let close = json!({"instance_id": "e2e"});
        second
            .step(
                ("close_instance", "close_instance"),
                close,
                "\"stopped\":true",
            )
            .await;
        failures.extend(second.finish().await);
        assert!(failures.is_empty(), "{browser}: failed steps {failures:?}");
    }

    /// What `stop_browser` answers in the attach suite of `variable`: Chrome
    /// only disconnects from a browser it did not launch and says so;
    /// Firefox ends its automation session.
    fn stop_reply(variable: &str) -> &'static str {
        if variable.contains("CHROME") {
            "keeps running"
        } else {
            "stopped successfully"
        }
    }

    /// Attach mode: steps 3, 5 and 11 on the default instance, then
    /// `stop_browser`; a second attach must succeed afterwards.
    async fn attach(variable: &str) {
        let Ok(args) = std::env::var(variable) else {
            println!("SKIP attach suite ({variable} not set)");
            return;
        };
        let args: Vec<&str> = args.split_whitespace().collect();
        let base = serve().await;
        let mut mcp = Mcp::start(&args).await;
        let all = [
            tab_steps("default", &base),
            runtime_steps("default"),
            dialog_and_wait_steps("default"),
            debugger_steps("default", &base),
        ];
        mcp.steps(all.concat()).await;
        let stop = json!({"instance_id": "default"});
        mcp.step(("stop_browser", "stop_browser"), stop, stop_reply(variable))
            .await;
        let mut failures = mcp.finish().await;

        let mut again = Mcp::start(&args).await;
        let eval = json!({"expression": "1 + 1"});
        again
            .step(("second attach answers", "evaluate_js"), eval, "2")
            .await;
        failures.extend(again.finish().await);
        assert!(failures.is_empty(), "{variable}: failed steps {failures:?}");
    }

    #[tokio::test]
    #[ignore = "requires a real Chrome installation"]
    async fn e2e_chrome() {
        smoke("chrome").await;
        sigterm_then_relaunch("chrome").await;
    }

    #[tokio::test]
    #[ignore = "requires a real Firefox installation"]
    async fn e2e_firefox() {
        smoke("firefox").await;
        sigterm_then_relaunch("firefox").await;
    }

    #[tokio::test]
    #[ignore = "requires tests/chrome.sh start"]
    async fn e2e_chrome_attach() {
        attach("BCM_E2E_ATTACH_CHROME").await;
    }

    #[tokio::test]
    #[ignore = "requires tests/firefox.sh start"]
    async fn e2e_firefox_attach() {
        attach("BCM_E2E_ATTACH_FIREFOX").await;
    }
}
