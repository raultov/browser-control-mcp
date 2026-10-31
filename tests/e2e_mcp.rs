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
    <label><input type="checkbox" id="cb"> agree</label>
    <div id="hov" onmouseover="window.__hovered = true">hover me</div>
    <input id="pk" onkeydown="window.__keys = (window.__keys || '') + event.key + (event.ctrlKey ? '^' : '') + ';'">
    <button id="dbl" ondblclick="window.__dbl = (window.__dbl || 0) + 1" oncontextmenu="window.__ctx = true; return false">dbl</button>
    <input type="file" id="up" onchange="window.__up = this.files[0].name + ':' + this.files[0].size">
    <my-widget></my-widget><iframe id="fr" src="/frame.html" width="300" height="80"></iframe>
    <script>customElements.define('my-widget', class extends HTMLElement { connectedCallback() {
      const root = this.attachShadow({mode: 'open'}); root.innerHTML = '<button id="sb">shadow</button>';
      root.getElementById('sb').onclick = () => { window.__pierced = (window.__pierced || '') + 'shadow;'; }; } });</script>
    <input id="q" value="old"><div id="marker">needle-marker</div>
    <div id="rich" contenteditable="true">old-rich</div>
    <script>(() => { const ed = document.getElementById('rich'); let model = 'old-rich'; window.__rich = model;
      const at = (node, off) => node === ed ? (off === 0 ? 0 : model.length) : off;
      const render = caret => { ed.textContent = model; window.__rich = model; const r = document.createRange();
        r.setStart(ed.firstChild || ed, ed.firstChild ? caret : 0); r.collapse(true); getSelection().removeAllRanges(); getSelection().addRange(r); };
      ed.addEventListener('beforeinput', e => { e.preventDefault(); const r = getSelection().rangeCount ? getSelection().getRangeAt(0) : null;
        let start = model.length, end = model.length;
        if (r && ed.contains(r.startContainer)) { start = at(r.startContainer, r.startOffset); end = at(r.endContainer, r.endOffset); }
        if (e.inputType.startsWith('delete') && start === end) start = Math.max(0, start - 1);
        const data = e.inputType === 'insertText' ? (e.data || '') : '';
        model = model.slice(0, start) + data + model.slice(end); render(start + data.length); }); })();</script>
    <button id="late" onclick="fetch('/api?late').then(r => r.text()).then(t => { document.getElementById('lateout').textContent = 'late:' + t; })">late</button><div id="lateout"></div>
    <table id="lay"><tr><td><a href="/lay">lay-link</a></td><td>lay-text</td></tr></table>
    <table id="dat"><tr><th>Name</th></tr><tr><td><a href="/dat">dat-link</a></td></tr><tr><td>dat-text <b>bold</b></td></tr></table>
    <script>console.log("hello from e2e", {n: 1});</script><script>throw new Error("boom-e2e");</script><script src="/app.js"></script></body></html>"#;
    const FRAME: &str = r#"<!doctype html><html><body>
    <button id="fb" onclick="parent.__pierced = (parent.__pierced || '') + 'frame;'">framed</button><input id="fq"></body></html>"#;
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
            p if p.starts_with("/frame.html") => ("text/html", FRAME.to_string()),
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
    /// contains the rest; `$TAB` in the arguments is the last opened tab,
    /// `$REF` the last ref a reply showed.
    type Step = (&'static str, &'static str, Value, &'static str);

    /// One MCP client session over the child's stdio.
    struct Mcp {
        child: Child,
        stdin: ChildStdin,
        stdout: BufReader<ChildStdout>,
        next_id: u64,
        failures: Vec<String>,
        last_tab: String,
        last_ref: String,
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
                last_ref: String::new(),
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
            let args = args
                .to_string()
                .replace("$TAB", &self.last_tab)
                .replace("$REF", &self.last_ref);
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
            if let Some(start) = text.rfind("[ref=") {
                let rest = &text[start + "[ref=".len()..];
                self.last_ref = rest[..rest.find(']').unwrap_or(0)].to_string();
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

    /// ISSUE-3: the input tools reach a shadow root and a same-origin iframe
    /// through `>>>` selectors.
    fn pierce_steps(instance: &str) -> Vec<Step> {
        let i = json!(instance);
        let effects = "window.__pierced + document.querySelector('#fr').contentDocument.querySelector('#fq').value";
        vec![
            (
                "ISSUE-3 wait_for inside the iframe",
                "wait_for",
                json!({"instance_id": i, "selector": "#fr >>> #fb"}),
                "Waited",
            ),
            (
                "ISSUE-3 click inside a shadow root",
                "click_element",
                json!({"instance_id": i, "selector": "my-widget >>> #sb"}),
                "Successfully clicked",
            ),
            (
                "ISSUE-3 click inside an iframe",
                "click_element",
                json!({"instance_id": i, "selector": "#fr >>> #fb"}),
                "Successfully clicked",
            ),
            (
                "ISSUE-3 fill inside an iframe",
                "fill_input",
                json!({"instance_id": i, "selector": "#fr >>> #fq", "text": "in-frame"}),
                "Successfully",
            ),
            (
                "ISSUE-3 effects",
                "evaluate_js",
                json!({"instance_id": i, "expression": effects}),
                "shadow;frame;in-frame",
            ),
        ]
    }

    /// ISSUE-4: the snapshot outlines the page with refs (into the iframe),
    /// and a ref found in it works as a selector.
    fn snapshot_steps(instance: &str) -> Vec<Step> {
        let i = json!(instance);
        vec![
            (
                "ISSUE-4 snapshot",
                "snapshot",
                json!({"instance_id": i}),
                "button \"go\" [ref=",
            ),
            (
                "ISSUE-4 find_in_snapshot",
                "find_in_snapshot",
                json!({"instance_id": i, "text": "framed"}),
                "button \"framed\" [ref=",
            ),
            (
                "ISSUE-4 click by ref",
                "click_element",
                json!({"instance_id": i, "selector": "$REF"}),
                "Successfully clicked",
            ),
            (
                "ISSUE-4 effect of the click by ref",
                "evaluate_js",
                json!({"instance_id": i, "expression": "window.__pierced"}),
                "frame;frame;",
            ),
        ]
    }

    /// ISSUE-20: a layout table (no headers) leaves only its content; a
    /// data table's cell with a link is not named after it, and a cell of
    /// plain text is named by it.
    fn table_steps(instance: &str) -> Vec<Step> {
        let i = json!(instance);
        let scoped = |selector: &str| json!({"instance_id": i, "selector": selector});
        vec![
            (
                "ISSUE-20 a layout table leaves its content",
                "snapshot",
                scoped("#lay"),
                "\n- link \"lay-link\" [href=/lay]",
            ),
            (
                "ISSUE-20 a cell with a link is not named after it",
                "snapshot",
                scoped("#dat"),
                "- cell [ref=",
            ),
            (
                "ISSUE-20 a cell of plain text is named by it",
                "snapshot",
                scoped("#dat"),
                "- cell \"dat-text bold\" [ref=",
            ),
        ]
    }

    /// ISSUE-23: fill_input replaces the content of a model-driven rich
    /// text editor (its model, not only its DOM).
    fn rich_editor_steps(instance: &str) -> Vec<Step> {
        let i = json!(instance);
        vec![
            (
                "ISSUE-23 fill_input in a rich text editor",
                "fill_input",
                json!({"instance_id": i, "selector": "#rich", "text": "rich-typed"}),
                "Successfully",
            ),
            (
                "ISSUE-23 the editor's model holds the text",
                "evaluate_js",
                json!({"instance_id": i, "expression": "window.__rich"}),
                "\"value\": \"rich-typed\"",
            ),
        ]
    }

    /// ISSUE-21: a click whose handler fetches then updates the page waits
    /// for it, so the next read sees the update.
    fn settle_steps(instance: &str) -> Vec<Step> {
        let i = json!(instance);
        let read = "document.getElementById('lateout').textContent";
        vec![
            (
                "ISSUE-21 a click waits for the page to settle",
                "click_element",
                json!({"instance_id": i, "selector": "#late"}),
                "and settled after",
            ),
            (
                "ISSUE-21 the next read sees the update",
                "evaluate_js",
                json!({"instance_id": i, "expression": read}),
                "late:{",
            ),
        ]
    }

    /// ISSUE-5: one fill_form call fills a text field, checks a box and
    /// fills a field inside the iframe.
    fn form_steps(instance: &str) -> Vec<Step> {
        let i = json!(instance);
        let fields = json!([
            {"selector": "#q", "value": "formed"},
            {"selector": "#cb", "value": "true"},
            {"selector": "#fr >>> #fq", "value": "framed-form"},
        ]);
        let effects = "document.querySelector('#q').value + document.querySelector('#cb').checked + document.querySelector('#fr').contentDocument.querySelector('#fq').value";
        vec![
            (
                "ISSUE-5 fill_form",
                "fill_form",
                json!({"instance_id": i, "fields": fields}),
                "Filled 3 fields.",
            ),
            (
                "ISSUE-5 fill_form effects",
                "evaluate_js",
                json!({"instance_id": i, "expression": effects}),
                "formedtrueframed-form",
            ),
        ]
    }

    /// ISSUE-6: upload_file hands the file input a file of the repository
    /// (under the server's working directory, an upload root).
    fn upload_steps(instance: &str) -> Vec<Step> {
        let i = json!(instance);
        let manifest = format!("{}/Cargo.toml", env!("CARGO_MANIFEST_DIR"));
        vec![
            (
                "ISSUE-6 upload_file",
                "upload_file",
                json!({"instance_id": i, "selector": "#up", "paths": [manifest]}),
                "Set 1 file(s) on '#up': Cargo.toml.",
            ),
            (
                "ISSUE-6 the page read the file",
                "evaluate_js",
                json!({"instance_id": i, "expression": "window.__up"}),
                "Cargo.toml:",
            ),
            (
                "ISSUE-6 a file outside the roots is refused",
                "upload_file",
                json!({"instance_id": i, "selector": "#up", "paths": ["/etc/hostname"]}),
                "!",
            ),
        ]
    }

    /// ISSUE-13: save_pdf writes the first page to a file under the
    /// server's working directory (an upload root), and without a path
    /// returns the PDF in the reply.
    fn pdf_steps(instance: &str) -> Vec<Step> {
        let i = json!(instance);
        let path = format!("{}/target/e2e-{instance}.pdf", env!("CARGO_MANIFEST_DIR"));
        vec![
            (
                "ISSUE-13 save_pdf to a file",
                "save_pdf",
                json!({"instance_id": i, "path": path, "page_ranges": "1", "landscape": true}),
                "\"pages\": 1",
            ),
            (
                "ISSUE-13 save_pdf in the reply",
                "save_pdf",
                json!({"instance_id": i}),
                "-byte PDF",
            ),
        ]
    }

    /// ISSUE-7: hover, keys with modifiers, double and right clicks.
    fn key_and_pointer_steps(instance: &str) -> Vec<Step> {
        let i = json!(instance);
        let effects = "window.__hovered + '|' + window.__keys + '|' + document.querySelector('#pk').value + '|' + window.__dbl + ':' + window.__ctx";
        vec![
            (
                "ISSUE-7 hover",
                "hover",
                json!({"instance_id": i, "selector": "#hov"}),
                "Hovering over '#hov'",
            ),
            (
                "ISSUE-7 press a character",
                "press_key",
                json!({"instance_id": i, "selector": "#pk", "key": "a"}),
                "Pressed a in '#pk'.",
            ),
            (
                "ISSUE-7 press a shortcut",
                "press_key",
                json!({"instance_id": i, "key": "Enter", "modifiers": ["Control"]}),
                "Pressed Control+Enter in the focused element.",
            ),
            (
                "ISSUE-7 double click",
                "click_element",
                json!({"instance_id": i, "selector": "#dbl", "click_count": 2}),
                "2 clicks",
            ),
            (
                "ISSUE-7 right click",
                "click_element",
                json!({"instance_id": i, "selector": "#dbl", "button": "right"}),
                "right button",
            ),
            (
                "ISSUE-7 effects",
                "evaluate_js",
                json!({"instance_id": i, "expression": effects}),
                "true|a;Control^;Enter^;|a|1:true",
            ),
        ]
    }

    /// ISSUE-8: a cookie and a localStorage item set, read by the page,
    /// listed and removed.
    fn storage_steps(instance: &str) -> Vec<Step> {
        let i = json!(instance);
        vec![
            (
                "ISSUE-8 set a cookie",
                "cookies",
                json!({"instance_id": i, "action": "set", "name": "e2e", "value": "cookie-v"}),
                "Set cookie 'e2e'",
            ),
            (
                "ISSUE-8 the page reads it",
                "evaluate_js",
                json!({"instance_id": i, "expression": "document.cookie"}),
                "e2e=cookie-v",
            ),
            (
                "ISSUE-8 get it, value hidden",
                "cookies",
                json!({"instance_id": i, "action": "get", "name": "e2e"}),
                "1 cookie(s) sent to",
            ),
            (
                "ISSUE-8 delete it",
                "cookies",
                json!({"instance_id": i, "action": "delete", "name": "e2e"}),
                "Deleted 1 cookie(s): e2e",
            ),
            (
                "ISSUE-8 set a storage item",
                "web_storage",
                json!({"instance_id": i, "action": "set", "key": "k", "value": "stored-v"}),
                "Set 'k' in the localStorage",
            ),
            (
                "ISSUE-8 get it with its value",
                "web_storage",
                json!({"instance_id": i, "action": "get", "key": "k", "show_values": true}),
                "\"value\": \"stored-v\"",
            ),
            (
                "ISSUE-8 clear the storage",
                "web_storage",
                json!({"instance_id": i, "action": "clear"}),
                "Cleared 1 item(s)",
            ),
        ]
    }

    /// ISSUE-9: a phone-sized viewport at 2x, dark mode where the browser
    /// can emulate it, then a reset.
    fn emulate_steps(instance: &str, chrome: bool) -> Vec<Step> {
        let i = json!(instance);
        let dark = if chrome {
            "\"color_scheme\": \"dark\""
        } else {
            "!color_scheme not available on Firefox"
        };
        vec![
            (
                "ISSUE-9 emulate a viewport",
                "emulate",
                json!({"instance_id": i, "width": 500, "height": 600, "device_scale_factor": 2.0}),
                "\"width\": 500",
            ),
            (
                "ISSUE-9 emulate dark mode",
                "emulate",
                json!({"instance_id": i, "color_scheme": "dark"}),
                dark,
            ),
            (
                "ISSUE-9 reset",
                "emulate",
                json!({"instance_id": i, "reset": true}),
                "Emulation reset",
            ),
        ]
    }

    /// `evaluate_js` of a fetch of `url` plus a fresh suffix (never cached):
    /// its text, or `failed: <error>`.
    fn fresh_fetch(i: &Value, url: &str) -> Value {
        json!({"instance_id": i, "expression": format!(
            "fetch('{url}' + Date.now()).then(r => r.text(), e => 'failed: ' + e)")})
    }

    /// ISSUE-12: a mocked route answers a fetch, counts its hit and goes;
    /// then offline.
    fn network_steps(instance: &str) -> Vec<Step> {
        let i = json!(instance);
        let fetch = |url: &str| fresh_fetch(&i, url);
        let route = json!({"instance_id": i, "url_pattern": "*/api/mocked*",
            "body": "from-the-route", "content_type": "text/plain"});
        let mut steps = vec![
            (
                "ISSUE-12 mock_route",
                "mock_route",
                route,
                "Route route-1 added",
            ),
            (
                "ISSUE-12 the route answers",
                "evaluate_js",
                fetch("/api/mocked?"),
                "from-the-route",
            ),
            (
                "ISSUE-12 list_routes counts the hit",
                "list_routes",
                json!({"instance_id": i}),
                "\"hits\": 1",
            ),
            (
                "ISSUE-12 remove_route",
                "remove_route",
                json!({"instance_id": i}),
                "0 left",
            ),
        ];
        steps.extend(offline_steps(&i));
        steps
    }

    /// ISSUE-12: offline makes an uncached fetch fail, then online again.
    fn offline_steps(i: &Value) -> Vec<Step> {
        vec![
            (
                "ISSUE-12 set_offline",
                "set_offline",
                json!({"instance_id": i, "offline": true}),
                "\"navigator_online\": false",
            ),
            (
                "ISSUE-12 offline fetch fails",
                "evaluate_js",
                fresh_fetch(i, "/api/fresh?"),
                "failed: ",
            ),
            (
                "ISSUE-12 back online",
                "set_offline",
                json!({"instance_id": i, "offline": false}),
                "online again",
            ),
        ]
    }

    /// Observation steps: console, network, screenshot, DOM.
    fn observation_steps(instance: &str) -> Vec<Step> {
        let i = json!(instance);
        vec![
            (
                "ISSUE-19 console log: object preview and location",
                "get_console_logs",
                json!({"instance_id": i}),
                "[log] hello from e2e {n: 1} @ http",
            ),
            (
                "ISSUE-19 console error: uncaught",
                "get_console_logs",
                json!({"instance_id": i}),
                "Uncaught Error: boom-e2e",
            ),
            (
                "ISSUE-18 network: one line per request",
                "get_network_logs",
                json!({"instance_id": i, "url_filter": "/api"}),
                "/api => 200",
            ),
            (
                "ISSUE-18 network details on demand",
                "get_network_logs",
                json!({"instance_id": i, "url_filter": "/api", "include_details": true}),
                "\"response_headers\"",
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
        let mut steps = vec![
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
                "ISSUE-16 evaluate_js while paused answers at once",
                "evaluate_js",
                json!({"instance_id": i, "expression": "Promise.resolve(6 * 7)"}),
                "42",
            ),
            (
                "step_over",
                "step_over",
                json!({"instance_id": i}),
                "Stepped",
            ),
            ("resume", "resume", json!({"instance_id": i}), "resumed"),
        ];
        steps.extend(click_into_breakpoint_steps(&i));
        steps
    }

    /// A click whose handler hits the breakpoint, then the paused frame.
    fn click_into_breakpoint_steps(i: &Value) -> Vec<Step> {
        vec![
            (
                "ISSUE-1/11 click into a breakpoint answers at once, noting the pause",
                "click_element",
                json!({"instance_id": i, "selector": "#hit"}),
                "The page paused in the debugger at http",
            ),
            (
                "ISSUE-17 input while paused is refused at once",
                "click_element",
                json!({"instance_id": i, "selector": "#hit"}),
                "!The page is paused in the debugger at http",
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

    /// ISSUE-22: insights explain the profile: Chrome lists the
    /// render-blocking requests, Firefox notes it cannot.
    fn insights_step(browser: &str) -> Step {
        let args = json!({"instance_id": "e2e", "duration_ms": 500, "insights": true});
        let expected = if browser == "chrome" {
            "\"render_blocking\": ["
        } else {
            "render_blocking is null: this browser does not say"
        };
        (
            "ISSUE-22 profile_page_performance insights",
            "profile_page_performance",
            args,
            expected,
        )
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
                insights_step(browser),
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
            insights_step(browser),
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

    /// The secret the launched server masks (`--secrets`).
    const E2E_SECRET: &str = "s3cret-e2e-value";

    /// A secrets file with `E2E_PASSWORD` (under `target/`, per browser).
    fn secrets_file(browser: &str) -> String {
        let path = format!("{}/target/e2e-{browser}.env", env!("CARGO_MANIFEST_DIR"));
        std::fs::write(&path, format!("E2E_PASSWORD={E2E_SECRET}\n")).unwrap();
        path
    }

    /// ISSUE-14: fill_input types the value of `<secret:E2E_PASSWORD>`,
    /// and reading it back shows the name, not the value.
    fn secret_steps(instance: &str) -> Vec<Step> {
        let i = json!(instance);
        let read = |expression: &str| json!({"instance_id": i, "expression": expression});
        vec![
            (
                "ISSUE-14 fill_input types a secret",
                "fill_input",
                json!({"instance_id": i, "selector": "#q", "text": "<secret:E2E_PASSWORD>"}),
                "Successfully",
            ),
            (
                "ISSUE-14 the page got the value",
                "evaluate_js",
                read(&format!(
                    "document.querySelector('#q').value === '{E2E_SECRET}'"
                )),
                "true",
            ),
            (
                "ISSUE-14 reading it back is masked",
                "evaluate_js",
                read("'read: ' + document.querySelector('#q').value"),
                "read: <secret:E2E_PASSWORD>",
            ),
        ]
    }

    /// Steps 1–15 in launched mode.
    async fn smoke(browser: &str) {
        let base = serve().await;
        // Every opt-in tool group: the suite drives them all.
        let secrets = secrets_file(browser);
        let mut mcp = Mcp::start(&["--tool-groups", "all", "--secrets", &secrets]).await;
        mcp.list_tools(48).await;
        let all = [
            page_steps(browser, &base),
            tab_steps("e2e", &base),
            runtime_steps("e2e"),
            dialog_and_wait_steps("e2e"),
            pierce_steps("e2e"),
            snapshot_steps("e2e"),
            table_steps("e2e"),
            settle_steps("e2e"),
            rich_editor_steps("e2e"),
            form_steps("e2e"),
            secret_steps("e2e"),
            upload_steps("e2e"),
            pdf_steps("e2e"),
            key_and_pointer_steps("e2e"),
            storage_steps("e2e"),
            emulate_steps("e2e", browser == "chrome"),
            observation_steps("e2e"),
            network_steps("e2e"),
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
        let mut args: Vec<&str> = args.split_whitespace().collect();
        args.extend(["--tool-groups", "all"]);
        let base = serve().await;
        let mut mcp = Mcp::start(&args).await;
        let all = [
            tab_steps("default", &base),
            runtime_steps("default"),
            dialog_and_wait_steps("default"),
            pierce_steps("default"),
            snapshot_steps("default"),
            table_steps("default"),
            settle_steps("default"),
            rich_editor_steps("default"),
            form_steps("default"),
            upload_steps("default"),
            pdf_steps("default"),
            key_and_pointer_steps("default"),
            storage_steps("default"),
            emulate_steps("default", variable.contains("CHROME")),
            network_steps("default"),
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
