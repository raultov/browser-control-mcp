//! End-to-end smoke (specs 10 §4): drives the built binary over stdio MCP
//! against real browsers. Ignored in `make check`; `tests/run_all_e2e.sh`
//! runs it with `--ignored`. Pages are served on 127.0.0.1 by the test.
//!
//! Environment: `BCM_E2E_ARGS` (extra server arguments, space separated),
//! `BCM_E2E_BIN` (binary path, default `target/debug/browser-control-mcp`).

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
    <input id="q" value="old"><div id="marker">needle-marker</div>
    <script>console.log("hello from e2e");</script><script src="/app.js"></script></body></html>"#;
    const APP_JS: &str = "function appTarget(x) {\n  const y = x * 2;\n  return y + 1;\n}\nwindow.appTarget = appTarget;\nfetch('/api');\n";

    /// A tiny HTTP server for the test pages; returns its base URL.
    async fn serve() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    let n = socket.read(&mut buf).await.unwrap_or(0);
                    let request = String::from_utf8_lossy(&buf[..n]).to_string();
                    let path = request.split_whitespace().nth(1).unwrap_or("/").to_string();
                    let (kind, body) = match path.as_str() {
                        p if p.starts_with("/app.js") => ("application/javascript", APP_JS),
                        p if p.starts_with("/api") => ("application/json", "{\"ok\":true}"),
                        _ => ("text/html", INDEX),
                    };
                    let reply = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = socket.write_all(reply.as_bytes()).await;
                });
            }
        });
        base
    }

    /// `(label, tool, arguments, expected text)`; tool `sleep` waits.
    type Step = (&'static str, &'static str, Value, &'static str);

    /// One MCP client session over the child's stdio.
    struct Mcp {
        child: Child,
        stdin: ChildStdin,
        stdout: BufReader<ChildStdout>,
        next_id: u64,
        failures: Vec<String>,
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

        /// Calls a tool; returns (ok, first text block).
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

        /// One reported step: PASS when the call succeeds and its text
        /// contains `needle`.
        async fn step(&mut self, (label, name): (&str, &str), args: Value, needle: &str) {
            let (ok, text) = self.call(name, args).await;
            let passed = ok && text.contains(needle);
            println!("{} {label}", if passed { "PASS" } else { "FAIL" });
            if !passed {
                println!("      {}", text.chars().take(300).collect::<String>());
                self.failures.push(label.to_string());
            }
        }

        /// Runs `(label, tool, arguments, expected text)` steps in order.
        async fn steps(&mut self, steps: Vec<Step>) {
            for (label, name, args, needle) in steps {
                if name == "sleep" {
                    tokio::time::sleep(Duration::from_millis(1500)).await;
                    continue;
                }
                self.step((label, name), args, needle).await;
            }
        }

        async fn finish(mut self) -> Vec<String> {
            drop(self.stdin);
            let _ = tokio::time::timeout(Duration::from_secs(30), self.child.wait()).await;
            self.failures
        }
    }

    /// Instance, tab and navigation steps of the smoke on one browser.
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
                "open_tab",
                "open_tab",
                json!({"instance_id": i, "url": url, "label": "t"}),
                "tab_id",
            ),
            ("list_tabs", "list_tabs", json!({"instance_id": i}), "tab-"),
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
        ]
    }

    /// Runtime and observation steps on the page opened by `page_steps`.
    fn runtime_steps() -> Vec<Step> {
        let i = json!("e2e");
        vec![
            (
                "evaluate_js",
                "evaluate_js",
                json!({"instance_id": i, "expression": "({sum: 1+2})"}),
                "\"sum\"",
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
                json!({"instance_id": i, "expression": "document.querySelector('#q').value + window.__clicked"}),
                "typed1",
            ),
            (
                "console",
                "get_console_logs",
                json!({"instance_id": i}),
                "hello from e2e",
            ),
            (
                "network",
                "get_network_logs",
                json!({"instance_id": i, "url_filter": "/api"}),
                "200",
            ),
            (
                "inspect_dom",
                "inspect_dom",
                json!({"instance_id": i, "query": "needle"}),
                "Found",
            ),
        ]
    }

    /// Steps 11–12: breakpoint pause, frame evaluation, step, resume, search.
    fn debugger_steps(base: &str) -> Vec<Step> {
        let i = json!("e2e");
        let bp = json!({"instance_id": i, "url": format!("{base}/app.js"), "line_number": 1});
        let trigger =
            json!({"instance_id": i, "expression": "setTimeout(() => appTarget(5), 50); 1"});
        vec![
            (
                "screenshot",
                "capture_screenshot",
                json!({"instance_id": i}),
                "",
            ),
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
                json!({"instance_id": i, "expression": "x"}),
                "5",
            ),
            (
                "step_over",
                "step_over",
                json!({"instance_id": i}),
                "Stepped",
            ),
            ("resume", "resume", json!({"instance_id": i}), "resumed"),
            (
                "close_instance",
                "close_instance",
                json!({"instance_id": i}),
                "\"stopped\":true",
            ),
        ]
    }

    async fn smoke(browser: &str) {
        let base = serve().await;
        let mut mcp = Mcp::start(&[]).await;
        let tools = mcp.request("tools/list", json!({})).await;
        let count = tools["result"]["tools"]
            .as_array()
            .map(Vec::len)
            .unwrap_or(0);
        println!(
            "{} tools/list ({count} tools)",
            if count == 32 { "PASS" } else { "FAIL" }
        );
        if count != 32 {
            mcp.failures.push("tools/list".into());
        }
        mcp.steps(page_steps(browser, &base)).await;
        mcp.steps(runtime_steps()).await;
        mcp.steps(debugger_steps(&base)).await;
        let failures = mcp.finish().await;
        assert!(failures.is_empty(), "{browser}: failed steps {failures:?}");
    }

    #[tokio::test]
    #[ignore = "requires a real Chrome installation"]
    async fn e2e_chrome() {
        smoke("chrome").await;
    }

    #[tokio::test]
    #[ignore = "requires a real Firefox installation"]
    async fn e2e_firefox() {
        smoke("firefox").await;
    }
}
