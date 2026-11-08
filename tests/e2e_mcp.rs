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
    <my-widget></my-widget><iframe id="fr" src="/frame.html" width="300" height="80"></iframe><iframe id="fr2" src="/frame.html" width="300" height="80"></iframe>
    <script>customElements.define('my-widget', class extends HTMLElement { connectedCallback() {
      const root = this.attachShadow({mode: 'open'}); root.innerHTML = '<button id="sb">shadow</button>';
      root.getElementById('sb').onclick = () => { window.__pierced = (window.__pierced || '') + 'shadow;'; }; } });</script>
    <input id="q" value="old"><div id="marker">needle-marker</div>
    <label style="position:relative;display:inline-block;width:60px;height:24px"><input type="checkbox" id="sw" style="position:absolute;left:0;top:0;width:60px;height:24px;margin:0"><span style="position:absolute;inset:0;background:#ccc"></span></label>
    <div style="position:relative;display:inline-block"><button id="covered">covered</button><div style="position:absolute;inset:0;background:#fff"></div></div>
    <div id="shot" style="width:123px;height:45px;background:#c00">shot</div>
    <button id="arm" onclick="setTimeout(() => { const b = document.createElement('button'); b.id = 'appear'; b.textContent = 'appear'; b.onclick = () => window.__late = (window.__late || 0) + 1; document.getElementById('arm').after(b); document.getElementById('soon').disabled = false; }, 700)">arm</button>
    <input id="vdate" type="date"><input id="vtime" type="time"><input id="vrange" type="range" min="0" max="100" step="5"><input id="vcolor" type="color">
    <button id="soon" disabled onclick="window.__late = (window.__late || 0) + 1">soon</button><button id="never" disabled onclick="window.__never = 1">never</button>
    <div id="custom" role="textbox" tabindex="0" aria-label="custom editor">old</div>
    <script>(() => { const ed = document.getElementById('custom'); let model = 'old', all = false; window.__custom = model;
      ed.addEventListener('keydown', e => { const mod = e.ctrlKey || e.metaKey;
        if (mod && e.key === 'a') all = true; else if (mod && (e.key === 'End' || e.key === 'ArrowDown')) all = false;
        else if (e.key === 'Backspace') { model = all ? '' : model.slice(0, -1); all = false; }
        else if (e.key.length === 1 && !mod) { model = all ? e.key : model + e.key; all = false; } else return;
        e.preventDefault(); ed.textContent = model; window.__custom = model; }); })();</script>
    <input role="combobox" aria-label="Movie" value="The Godfather"><button role="combobox" aria-label="Food">Blueberry</button>
    <span role="spinbutton" aria-label="Month" aria-valuenow="10" aria-valuetext="October" tabindex="0">10</span><div role="slider" aria-label="Volume" aria-valuenow="30" tabindex="0">|</div>
    <input id="app" value="draft"><input id="appn" type="number" value="12">
    <div id="card" style="cursor:pointer" onclick="window.__card = true"><a href="/card">card-link</a> <button id="card-btn">card-button</button></div>
    <div id="plaincard" style="cursor:pointer" onclick="window.__plain = true">plain card text</div>
    <style>.row .tool { opacity: 0; pointer-events: none; } .row:hover .tool { opacity: 1; pointer-events: auto; }</style>
    <div class="row" style="padding:8px">a message <button class="tool" id="more" onclick="window.__more = (window.__more || 0) + 1">More</button></div>
    <div id="rich" contenteditable="true">old-rich</div>
    <script>(() => { const ed = document.getElementById('rich'); let model = 'old-rich'; window.__rich = model;
      // Like CodeMirror 6 on Firefox: the editor puts its caret back at the start once focused.
      ed.addEventListener('focus', () => setTimeout(() => { const r = document.createRange(); r.setStart(ed.firstChild || ed, 0); r.collapse(true); getSelection().removeAllRanges(); getSelection().addRange(r); }, 0));
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
    /// ISSUE-30: an HTML5 drag source and drop target, and a knob moved by
    /// pointer events onto a zone.
    const DRAG: &str = r#"<!doctype html><html><body style="margin:0">
    <div id="src" draggable="true" style="position:absolute;left:20px;top:20px;width:80px;height:40px">src</div>
    <div id="dst" style="position:absolute;left:300px;top:20px;width:120px;height:80px">dst</div>
    <div id="knob" style="position:absolute;left:20px;top:200px;width:40px;height:40px;touch-action:none">knob</div>
    <div id="zone" style="position:absolute;left:300px;top:180px;width:120px;height:80px">zone</div>
    <script>
    src.addEventListener('dragstart', e => e.dataTransfer.setData('text/plain', 'payload'));
    dst.addEventListener('dragover', e => e.preventDefault());
    dst.addEventListener('drop', e => { e.preventDefault(); window.__html5 = 'dropped:' + e.dataTransfer.getData('text/plain'); });
    let grab = null;
    knob.addEventListener('pointerdown', e => { grab = [e.clientX - knob.offsetLeft, e.clientY - knob.offsetTop]; knob.setPointerCapture(e.pointerId); });
    knob.addEventListener('pointermove', e => { if (grab) { knob.style.left = (e.clientX - grab[0]) + 'px'; knob.style.top = (e.clientY - grab[1]) + 'px'; } });
    knob.addEventListener('pointerup', e => { grab = null; const z = zone.getBoundingClientRect();
      window.__pointer = e.clientX >= z.left && e.clientX <= z.right && e.clientY >= z.top && e.clientY <= z.bottom ? 'dropped' : 'missed'; });
    </script></body></html>"#;
    /// ISSUE-27: a page whose iframe is of another origin (`localhost`
    /// against `127.0.0.1`, same port), and that frame's page.
    const XO_TOP: &str = r#"<!doctype html><html><body><p>cross-origin host</p>
    <iframe id="xo" width="400" height="160"></iframe>
    <script>document.getElementById('xo').src = 'http://localhost:' + location.port + '/xo-inner.html';</script>
    </body></html>"#;
    const XO_INNER: &str = r#"<!doctype html><html><body>
    <button id="ib" onclick="document.getElementById('out').textContent = 'xo clicked'">inner button</button>
    <input id="iq" aria-label="inner field"><span id="out"></span>
    <script src="/xo-inner.js"></script><button id="ibp" onclick="document.getElementById('pout').textContent = 'xo ' + xoInner(21)">inner pause</button><span id="pout"></span>
    <script>console.log('xo inner log'); fetch('/api?from-frame');</script></body></html>"#;
    /// ISSUE-36: the script of the frame of another origin, to debug.
    const XO_INNER_JS: &str =
        "function xoInner(n) {\n  const doubled = n * 2;\n  return doubled;\n}\n";
    const APP_JS: &str = "function appTarget(x) {\n  const y = x * 2;\n  return y + 1;\n}\nwindow.appTarget = appTarget;\nfetch('/api');\nfunction appLoop(n) {\n  let s = 0;\n  for (let k = 0; k < n; k++) {\n    s += k;\n  }\n  return s;\n}\nwindow.appLoop = appLoop;\nfunction appPrice(n) {\n  return n * 2;\n}\nfunction appTotal(list) {\n  return list.map(appPrice).reduce((a, b) => a + b, 0);\n}\nfunction appBuy() {\n  const total = appTotal([1, 2]);\n  return total;\n}\nfunction appBoom() {\n  return null.x;\n}\nwindow.appBuy = appBuy; window.appBoom = appBoom;\n";

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

    /// ISSUE-85: two fields with neither an id nor a name (Chrome reports
    /// each twice) and a synchronous XHR (a deprecation on both browsers).
    const FORMS85: &str = "<!doctype html><title>forms85</title><form><input class=\"f85\"><input class=\"f85\"></form>\
        <script>const x = new XMLHttpRequest(); x.open('GET', '/api?sync85', false); x.send();</script>";

    /// ISSUE-86: an external stylesheet with a rule in a named container
    /// query.
    const CSS86: &str = ".wrap86 { container: box86 / inline-size; width: 200px; }\n@container box86 (min-width: 10px) { .k86 { color: rgb(1, 2, 3); } }\n";

    /// The scripts served (by path prefix).
    const SCRIPTS: [(&str, &str); 3] = [
        ("/app.js", APP_JS),
        ("/xo-inner.js", XO_INNER_JS),
        (
            "/worker75.js",
            "console.log('worker75-log'); fetch('/api?from=worker75').then(() => postMessage('done'));",
        ),
    ];

    /// The pages served as they are (by path prefix): content type, body.
    const PAGES: [(&str, &str, &str); 10] = [
        ("/data.csv", "text/csv", "a,b\n1,2\n"),
        (
            "/self-close.html",
            "text/html",
            "<!doctype html><title>closing</title><script>setTimeout(() => window.close(), 2000)</script>",
        ),
        ("/api", "application/json", "{\"ok\":true}"),
        ("/frame.html", "text/html", FRAME),
        ("/drag.html", "text/html", DRAG),
        ("/e2e86.css", "text/css", CSS86),
        ("/forms85.html", "text/html", FORMS85),
        (
            "/close90.html",
            "text/html",
            "<!doctype html><title>close90</title><button id=\"close90\" onclick=\"window.close()\">close</button>",
        ),
        ("/xo-top.html", "text/html", XO_TOP),
        ("/xo-inner.html", "text/html", XO_INNER),
    ];

    /// The content type and body served for `path`.
    fn page(path: &str, ws: &str) -> (&'static str, String) {
        if let Some((_, script)) = SCRIPTS.iter().find(|(at, _)| path.starts_with(at)) {
            return ("application/javascript", (*script).to_string());
        }
        if path.starts_with("/ws.html") {
            return ("text/html", ws.to_string());
        }
        match PAGES.iter().find(|(at, _, _)| path.starts_with(at)) {
            Some((_, kind, body)) => (kind, (*body).to_string()),
            None => ("text/html", INDEX.to_string()),
        }
    }

    /// The status line (and extra headers) answering `request` for `path`:
    /// `/auth` asks for HTTP authentication (Basic, user:pass) until it
    /// gets it; `/hop78` redirects to `/api?after-hop78` (ISSUE-78).
    fn status_line(path: &str, request: &str) -> &'static str {
        let authorized = request
            .to_ascii_lowercase()
            .contains("authorization: basic dxnlcjpwyxnz");
        if path.starts_with("/auth") && !authorized {
            return "401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"e2e\"";
        }
        if path.starts_with("/hop78") {
            return "302 Found\r\nLocation: /api?after-hop78";
        }
        "200 OK"
    }

    /// Answers one HTTP request.
    async fn respond(mut socket: tokio::net::TcpStream, ws: String) {
        let mut buf = vec![0u8; 4096];
        let n = socket.read(&mut buf).await.unwrap_or(0);
        let request = String::from_utf8_lossy(&buf[..n]).to_string();
        let path = request.split_whitespace().nth(1).unwrap_or("/").to_string();
        let (kind, body) = page(&path, &ws);
        let status = status_line(&path, &request);
        let reply = format!(
            "HTTP/1.1 {status}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
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
        /// The tab the last action that opened one named (`$OPENED`).
        last_opened: String,
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
                last_opened: String::new(),
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
                .replace("$OPENED", &self.last_opened)
                .replace("$REF", &self.last_ref);
            let expected = expected
                .replace("$TAB", &self.last_tab)
                .replace("$OPENED", &self.last_opened);
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
            if let Some(start) = text.find("The action opened tab ") {
                let rest = &text[start + "The action opened tab ".len()..];
                let end = rest.find(|c: char| !(c.is_ascii_alphanumeric() || c == '-'));
                self.last_opened = rest[..end.unwrap_or(rest.len())].to_string();
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

        /// The ids of the default instance's tabs, and its active one.
        async fn tabs(&mut self) -> (Vec<String>, String) {
            let (_, text) = self.call("list_tabs", json!({})).await;
            let reply: Value = serde_json::from_str(&text).unwrap_or_default();
            let ids = reply["tabs"].as_array().into_iter().flatten();
            let ids = ids.filter_map(|t| t["tab_id"].as_str().map(String::from));
            let active = reply["active_tab_id"].as_str().unwrap_or_default();
            (ids.collect(), active.to_string())
        }

        /// ISSUE-95: with the default instance at its 16 tabs, a click that
        /// opens one more says it is not tracked, and the agent's tab stays
        /// in front (the new page closes itself 2 s after loading).
        async fn tab_cap(&mut self) {
            let (tabs, active) = self.tabs().await;
            for _ in tabs.len()..16 {
                let (ok, text) = self.call("open_tab", json!({"url": "about:blank"})).await;
                if !ok {
                    self.report("ISSUE-95 fill the instance", false, &text);
                    return;
                }
            }
            let back = json!({"tab_id": active});
            self.step(("ISSUE-95 back to the tab", "switch_tab"), back, "")
                .await;
            let link = "(() => { const a = document.createElement('a'); a.id = 'cap95'; a.href = '/self-close.html'; a.target = '_blank'; a.textContent = 'cap95'; document.body.prepend(a); return 1; })()";
            let steps = vec![
                (
                    "ISSUE-95 a link to a new tab",
                    "evaluate_js",
                    json!({"expression": link}),
                    "",
                ),
                (
                    "ISSUE-95 a tab beyond the cap is named untracked",
                    "click_element",
                    json!({"selector": "#cap95"}),
                    "opened a tab that is not tracked",
                ),
                (
                    "ISSUE-95 the agent's tab stays in front",
                    "evaluate_js",
                    json!({"expression": "document.visibilityState"}),
                    "visible",
                ),
                (
                    "ISSUE-95 remove the link",
                    "evaluate_js",
                    json!({"expression": "document.getElementById('cap95').remove(); 1"}),
                    "",
                ),
            ];
            self.steps(steps).await;
        }

        /// Closes the default instance's tabs that are neither in `keep` nor
        /// the active one, so a browser the suite attached to does not pile
        /// them up (ISSUE-95). (Its pages are registered only once a tool
        /// uses them: `keep` can be empty.)
        async fn close_tabs_but(&mut self, keep: &[String]) {
            let (tabs, active) = self.tabs().await;
            let gone = tabs.iter().filter(|t| !keep.contains(t) && **t != active);
            for tab in gone {
                let (ok, text) = self.call("close_tab", json!({"tab_id": tab})).await;
                if !ok {
                    self.report("close the suite's tabs", false, &text);
                }
            }
        }

        /// Runs `steps` on a browser the suite attached to, then the
        /// ISSUE-95 check, then closes the tabs it opened.
        async fn attached_steps(&mut self, steps: Vec<Step>) {
            let (there, _) = self.tabs().await;
            self.steps(steps).await;
            self.tab_cap().await;
            self.close_tabs_but(&there).await;
        }

        /// ISSUE-96: without --port the default instance launches a Chrome
        /// of its own on a port Chrome picks, even with one already on 9222
        /// (it attached to it); then it is stopped.
        async fn own_default_chrome(&mut self) {
            let (ok, text) = self
                .call("evaluate_js", json!({"expression": "1 + 1"}))
                .await;
            self.report("ISSUE-96 the default instance starts", ok, &text);
            let (_, text) = self.call("list_instances", json!({})).await;
            let rows: Value = text
                .split("\n\n")
                .next()
                .and_then(|rows| serde_json::from_str(rows).ok())
                .unwrap_or_default();
            let port = rows[0]["port"].as_u64().unwrap_or_default();
            let own = rows[0]["id"] == "default" && port != 0 && port != 9222;
            self.report("ISSUE-96 it runs a Chrome of its own", own, &text);
            let stop = json!({"instance_id": "default"});
            self.step(("ISSUE-96 stop it", "stop_browser"), stop, "stopped")
                .await;
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
                "ISSUE-70 navigate to a hash route",
                "navigate",
                json!({"instance_id": i, "url": format!("{base}/#/e2e70")}),
                "/#/e2e70.",
            ),
            (
                "ISSUE-70 navigate to another hash route",
                "navigate",
                json!({"instance_id": i, "url": format!("{base}/#/e2e70b")}),
                "/#/e2e70b.",
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
                "ISSUE-68 open_tab made it active, in front",
                "evaluate_js",
                json!({"instance_id": i, "expression": "document.visibilityState"}),
                "\"visible\"",
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

    /// ISSUE-91: a labelled field shows its placeholder in the snapshot
    /// (both browsers run the same outline).
    fn placeholder_steps(instance: &str) -> Vec<Step> {
        let i = json!(instance);
        let html = "<div id=\"w91\"><label for=\"f91\">Email91</label><input id=\"f91\" placeholder=\"you91@example.com\"><input placeholder=\"Search91\"></div>";
        let add = format!(
            "document.body.insertAdjacentHTML('afterbegin', {}); 1",
            json!(html)
        );
        vec![
            (
                "ISSUE-91 a labelled field with a placeholder",
                "evaluate_js",
                json!({"instance_id": i, "expression": add}),
                "",
            ),
            (
                "ISSUE-91 the snapshot shows its placeholder",
                "find_in_snapshot",
                json!({"instance_id": i, "text": "Email91"}),
                "textbox \"Email91\" [placeholder=\"you91@example.com\"] [ref=",
            ),
            (
                "ISSUE-91 a field named by its placeholder does not repeat it",
                "find_in_snapshot",
                json!({"instance_id": i, "text": "Search91"}),
                "textbox \"Search91\" [ref=",
            ),
            (
                "ISSUE-91 remove it",
                "evaluate_js",
                json!({"instance_id": i, "expression": "document.getElementById('w91').remove(); 1"}),
                "",
            ),
        ]
    }

    /// ISSUE-103: a javascript: URL or one without a scheme is refused
    /// before anything runs, the same on both browsers.
    fn url_check_steps(instance: &str) -> Vec<Step> {
        let go = |url: &str| json!({"instance_id": instance, "url": url});
        vec![
            (
                "ISSUE-103 a javascript: URL",
                "navigate",
                go("javascript:alert(1)"),
                "!Run the script with evaluate_js",
            ),
            (
                "ISSUE-103 a URL without a scheme",
                "navigate",
                go("not a url"),
                "!is not an absolute URL",
            ),
            (
                "ISSUE-103 a host and port without a scheme",
                "navigate",
                go("localhost:1/x"),
                "!did you mean http://localhost:1/x",
            ),
            (
                "ISSUE-103 the page answers",
                "evaluate_js",
                json!({"instance_id": instance, "expression": "1 + 1"}),
                "2",
            ),
        ]
    }

    /// ISSUE-90: a click whose page closes its own tab is a click, and the
    /// reply says the tab closed and which one is active (both browsers).
    fn self_close_steps(instance: &str) -> Vec<Step> {
        let i = json!(instance);
        // Opened by the page (Chrome lets a script close only such a tab)
        let link = "(() => { const a = document.createElement('a'); a.id = 'open90'; a.href = '/close90.html'; a.target = '_blank'; a.textContent = 'open90'; document.body.prepend(a); return 1; })()";
        vec![
            (
                "ISSUE-90 a link to a page that closes itself",
                "evaluate_js",
                json!({"instance_id": i, "expression": link}),
                "",
            ),
            (
                "ISSUE-90 open it",
                "click_element",
                json!({"instance_id": i, "selector": "#open90"}),
                "The action opened tab",
            ),
            (
                "ISSUE-90 act in it",
                "switch_tab",
                json!({"instance_id": i, "tab_id": "$OPENED"}),
                "$OPENED",
            ),
            (
                "ISSUE-90 its click closes its tab",
                "click_element",
                json!({"instance_id": i, "selector": "#close90"}),
                "Tab $OPENED closed during the action",
            ),
            (
                "ISSUE-90 the opener answers",
                "evaluate_js",
                json!({"instance_id": i, "expression": "document.getElementById('open90').remove(); 1"}),
                "1",
            ),
        ]
    }

    /// ISSUE-68: after open_tab and close_tab, the active tab is the one
    /// the browser shows (its page visible).
    fn foreground_steps(instance: &str) -> Vec<Step> {
        let i = json!(instance);
        let visible = json!({"instance_id": i, "expression": "document.visibilityState"});
        vec![
            (
                "ISSUE-68 open a tab to close",
                "open_tab",
                json!({"instance_id": i, "label": "closing68"}),
                "tab_id",
            ),
            (
                "ISSUE-68 close the active tab",
                "close_tab",
                json!({"instance_id": i, "tab_id": "$TAB"}),
                "active_tab_id",
            ),
            (
                "ISSUE-68 the next active tab is in front",
                "evaluate_js",
                visible.clone(),
                "\"visible\"",
            ),
            (
                "ISSUE-69 open another tab to close",
                "open_tab",
                json!({"instance_id": i, "label": "closing69"}),
                "tab_id",
            ),
            (
                "ISSUE-69 close_tab without tab_id closes the active tab",
                "close_tab",
                json!({"instance_id": i}),
                "\"closed_tab_id\":\"$TAB\"",
            ),
            (
                "ISSUE-69 the most recently active tab is back in front",
                "evaluate_js",
                visible,
                "\"visible\"",
            ),
        ]
    }

    /// ISSUE-88: values without a JSON form come back as a preview, the
    /// same on both browsers (they were `value: null`, and Firefox lost an
    /// Error's message).
    fn preview_steps(instance: &str) -> Vec<Step> {
        let i = json!(instance);
        let eval = |expression: &str| json!({"instance_id": i, "expression": expression});
        vec![
            (
                "ISSUE-88 a Map is previewed",
                "evaluate_js",
                eval("new Map([['a', 1]])"),
                "\"preview\": true",
            ),
            (
                "ISSUE-88 a cycle is marked",
                "evaluate_js",
                eval("(() => { const o = {n: 1}; o.self = o; return o; })()"),
                "[Circular]",
            ),
            (
                "ISSUE-88 a getter that throws is marked",
                "evaluate_js",
                eval("({get x() { throw new Error('g88'); }, y: 2})"),
                "[Thrown: g88]",
            ),
            (
                "ISSUE-88 an Error keeps its message",
                "evaluate_js",
                eval("new TypeError('bad88')"),
                "TypeError: bad88",
            ),
            (
                "ISSUE-88 a typed array is named",
                "evaluate_js",
                eval("new Uint8Array([1, 2, 3])"),
                "Uint8Array(3)",
            ),
        ]
    }

    /// Runtime and input steps on the active page.
    fn runtime_steps(instance: &str) -> Vec<Step> {
        let i = json!(instance);
        let effects = "document.querySelector('#q').value + window.__clicked";
        let mut steps = vec![
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
        ];
        steps.extend(preview_steps(instance));
        steps
    }

    /// ISSUE-11 and ISSUE-2: a click that opens a dialog, then content that
    /// arrives later, waited for.
    fn dialog_and_wait_steps(instance: &str) -> Vec<Step> {
        let i = json!(instance);
        let late = "setTimeout(() => document.body.insertAdjacentHTML('beforeend', '<p id=\"late\">late-arrival</p>'), 700); 1";
        let mut steps = vec![
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
        ];
        steps.extend(console_detail_steps(instance));
        steps.extend(issue_steps(instance));
        steps
    }

    /// ISSUE-60: a cookie the browser rejects is an issue (Chrome's Issues
    /// panel; Firefox's security console message), cleared on demand.
    fn issue_steps(instance: &str) -> Vec<Step> {
        let i = json!(instance);
        let cookie = "document.cookie = 'nosecure60=1; SameSite=None'; 1";
        let cookies = json!({"instance_id": i, "code": "Cookie"});
        let clear = json!({"instance_id": i, "code": "Cookie", "clear": true});
        vec![
            (
                "ISSUE-60 a cookie the browser rejects",
                "evaluate_js",
                json!({"instance_id": i, "expression": cookie}),
                "",
            ),
            ("ISSUE-60 wait for the issue", "sleep", json!({}), ""),
            (
                "ISSUE-60 it is a cookie issue",
                "get_issues",
                cookies.clone(),
                "[CookieIssue]",
            ),
            (
                "ISSUE-60 naming the cookie",
                "get_issues",
                cookies.clone(),
                "nosecure60",
            ),
            ("ISSUE-60 clear them", "get_issues", clear, "nosecure60"),
            (
                "ISSUE-60 they are gone",
                "get_issues",
                cookies,
                "0 issue(s)",
            ),
        ]
    }

    /// ISSUE-85: in a tab of its own, Chrome's form issues count each field
    /// once and name them by selector and ref; Firefox reports the
    /// deprecated synchronous XHR as an issue too.
    fn form_issue_steps(instance: &str, base: &str, chrome: bool) -> Vec<Step> {
        let i = json!(instance);
        let on = |code: &str| json!({"instance_id": i, "tab_id": "$TAB", "code": code});
        let mut steps = vec![
            (
                "ISSUE-85 a page with form problems",
                "open_tab",
                json!({"instance_id": i, "url": format!("{base}/forms85.html")}),
                "tab_id",
            ),
            ("ISSUE-85 wait for its issues", "sleep", json!({}), ""),
            (
                "ISSUE-85 the deprecated sync XHR is an issue",
                "get_issues",
                on("Deprecation"),
                "[DeprecationIssue]",
            ),
        ];
        if chrome {
            steps.extend([
                (
                    "ISSUE-85 two fields, counted once each",
                    "get_issues",
                    on("Generic"),
                    "neither an id nor a name: FormEmptyIdAndNameAttributesForInputError [x2]",
                ),
                (
                    "ISSUE-85 the fields are named",
                    "get_issues",
                    on("Generic"),
                    "form > input:nth-of-type(1) [ref=e",
                ),
            ]);
        }
        steps.push((
            "ISSUE-85 close its tab",
            "close_tab",
            json!({"instance_id": i, "tab_id": "$TAB"}),
            "closed_tab_id",
        ));
        steps
    }

    /// ISSUE-80: counters keep their count, an unhandled rejection says
    /// `(in promise)`, and accepting a prompt answers its default (both
    /// browsers).
    fn console_detail_steps(instance: &str) -> Vec<Step> {
        let i = json!(instance);
        let logs = json!({"instance_id": i});
        let log = "console.count('counter80'); console.count('counter80'); Promise.reject(new Error('rej80')); 1";
        let ask = "setTimeout(() => { window.__p80 = prompt('Name?', 'default80'); }); 1";
        vec![
            (
                "ISSUE-80 count twice and reject",
                "evaluate_js",
                json!({"instance_id": i, "expression": log}),
                "",
            ),
            ("ISSUE-80 wait for the messages", "sleep", json!({}), ""),
            (
                "ISSUE-80 the second count",
                "get_console_logs",
                logs.clone(),
                "counter80: 2",
            ),
            (
                "ISSUE-80 the rejection says in promise",
                "get_console_logs",
                logs,
                "Uncaught (in promise) Error: rej80",
            ),
            (
                "ISSUE-80 a prompt with a default",
                "evaluate_js",
                json!({"instance_id": i, "expression": ask}),
                "",
            ),
            ("ISSUE-80 wait for the prompt", "sleep", json!({}), ""),
            (
                "ISSUE-80 accept it without text",
                "handle_dialog",
                json!({"instance_id": i, "accept": true}),
                "Dialog accepted",
            ),
            (
                "ISSUE-80 the page got the default",
                "evaluate_js",
                json!({"instance_id": i, "expression": "window.__p80"}),
                "default80",
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
        let mut steps = vec![
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
        ];
        steps.extend(a11y_state_steps(instance));
        steps.extend(css_steps(instance));
        steps.extend(css_sheet_steps(instance));
        steps
    }

    /// ISSUE-86: a rule of an external stylesheet names that sheet, and its
    /// container query keeps its name (both browsers).
    fn css_sheet_steps(instance: &str) -> Vec<Step> {
        let i = json!(instance);
        let html = "<div id=\"w86\"><link rel=\"stylesheet\" href=\"/e2e86.css\"><div class=\"wrap86\"><p id=\"c86\" class=\"k86\">c86</p></div></div>";
        let add = format!(
            "document.body.insertAdjacentHTML('afterbegin', {}); 1",
            json!(html)
        );
        let styles = json!({"instance_id": i, "selector": "#c86"});
        vec![
            (
                "ISSUE-86 an element styled by an external sheet",
                "evaluate_js",
                json!({"instance_id": i, "expression": add}),
                "",
            ),
            ("ISSUE-86 wait for the sheet", "sleep", json!({}), ""),
            (
                "ISSUE-86 its rule names the sheet",
                "get_css_styles",
                styles.clone(),
                "/e2e86.css\"",
            ),
            (
                "ISSUE-86 the container query keeps its name",
                "get_css_styles",
                styles,
                "@container box86 (min-width: 10px)",
            ),
            (
                "ISSUE-86 remove it",
                "evaluate_js",
                json!({"instance_id": i, "expression": "document.getElementById('w86').remove(); 1"}),
                "",
            ),
        ]
    }

    /// ISSUE-59: an element's matched rules, with the overridden and
    /// `!important` declarations marked, and its computed value (both
    /// browsers).
    fn css_steps(instance: &str) -> Vec<Step> {
        let i = json!(instance);
        let html = "<div id=\"w59\"><style>#c59 { color: red; } .k59 { color: green !important; }</style><p id=\"c59\" class=\"k59\">c59</p></div>";
        let add = format!(
            "document.body.insertAdjacentHTML('afterbegin', {}); 1",
            json!(html)
        );
        let styles = json!({"instance_id": i, "selector": "#c59", "properties": ["color"]});
        let seen = |name: &'static str, expected: &'static str| {
            (name, "get_css_styles", styles.clone(), expected)
        };
        vec![
            (
                "ISSUE-59 a styled element",
                "evaluate_js",
                json!({"instance_id": i, "expression": add}),
                "",
            ),
            seen(
                "ISSUE-59 its rules",
                "Styles of p#c59.k59: 2 matched rule(s)",
            ),
            seen(
                "ISSUE-59 the overridden declaration",
                "\"overridden\": true",
            ),
            seen("ISSUE-59 the important one", "\"important\": true"),
            seen(
                "ISSUE-59 the computed color",
                "\"color\": \"rgb(0, 128, 0)\"",
            ),
            (
                "ISSUE-59 remove it",
                "evaluate_js",
                json!({"instance_id": i, "expression": "document.getElementById('w59').remove(); 1"}),
                "",
            ),
        ]
    }

    /// ISSUE-79: a tree item is named by its own text, grid cells are
    /// `gridcell`, and descriptions, pressed and current states and live
    /// regions show (both browsers run the same outline).
    fn a11y_state_steps(instance: &str) -> Vec<Step> {
        let i = json!(instance);
        let html = r##"<div id="w79"><a href="#d79" aria-current="page">Docs79</a><button aria-describedby="h79" aria-pressed="true">Toggle79</button><span id="h79">Turns it on</span><ul role="tree" aria-label="Files79"><li role="treeitem" aria-expanded="true">src79<ul role="group"><li role="treeitem">main79</li></ul></li></ul><table role="grid" aria-label="Grid79"><tr><th>H</th></tr><tr><td>c79</td></tr></table><div aria-live="polite">said79</div></div>"##;
        let add = format!(
            "document.body.insertAdjacentHTML('afterbegin', {}); 1",
            json!(html)
        );
        let scoped = json!({"instance_id": i, "selector": "#w79"});
        let seen = |name: &'static str, expected: &'static str| {
            (name, "snapshot", scoped.clone(), expected)
        };
        vec![
            (
                "ISSUE-79 widgets with states",
                "evaluate_js",
                json!({"instance_id": i, "expression": add}),
                "",
            ),
            seen(
                "ISSUE-79 a tree item is named by its text",
                "treeitem \"src79\" [expanded=true]",
            ),
            seen("ISSUE-79 a nested tree item", "treeitem \"main79\""),
            seen(
                "ISSUE-79 a description",
                "[description=\"Turns it on\"] [pressed=true]",
            ),
            seen(
                "ISSUE-79 the current link",
                "link \"Docs79\" [current=page]",
            ),
            seen("ISSUE-79 a grid cell", "gridcell \"c79\""),
            seen("ISSUE-79 a live region", "generic [live=polite]"),
            (
                "ISSUE-79 remove the widgets",
                "evaluate_js",
                json!({"instance_id": i, "expression": "document.getElementById('w79').remove(); 1"}),
                "",
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

    /// ISSUE-35: keys reach a field inside an iframe that was never
    /// clicked (the pierce steps clicked into #fr, so the top document's
    /// focus is there, not on #fr2).
    fn frame_focus_steps(instance: &str) -> Vec<Step> {
        let i = json!(instance);
        let value = json!({"instance_id": i, "expression": "document.querySelector('#fr2').contentDocument.querySelector('#fq').value"});
        vec![
            (
                "ISSUE-35 fill a field in another iframe",
                "fill_input",
                json!({"instance_id": i, "selector": "#fr2 >>> #fq", "text": "second-frame"}),
                "Successfully filled",
            ),
            (
                "ISSUE-35 the field holds the text",
                "evaluate_js",
                value.clone(),
                "\"value\": \"second-frame\"",
            ),
            (
                "ISSUE-35 press a key in it",
                "press_key",
                json!({"instance_id": i, "selector": "#fr2 >>> #fq", "key": "z"}),
                "Pressed z",
            ),
            (
                "ISSUE-35 the key reached it",
                "evaluate_js",
                value,
                "\"value\": \"second-framez\"",
            ),
        ]
    }

    /// ISSUE-28: an editor of its own (a focusable textbox that keeps its
    /// own model) is filled through the keys a user presses: Control+A
    /// then the text, Control+End then the appended text; the reply says
    /// the content was not checked.
    fn custom_editor_steps(instance: &str) -> Vec<Step> {
        let i = json!(instance);
        let model = json!({"instance_id": i, "expression": "window.__custom"});
        vec![
            (
                "ISSUE-28 fill a custom editor",
                "fill_input",
                json!({"instance_id": i, "selector": "#custom", "text": "new text"}),
                "not checked",
            ),
            (
                "ISSUE-28 its model holds the text",
                "evaluate_js",
                model.clone(),
                "\"value\": \"new text\"",
            ),
            (
                "ISSUE-28 append to a custom editor",
                "fill_input",
                json!({"instance_id": i, "selector": "#custom", "text": " more", "append": true}),
                "Successfully appended",
            ),
            (
                "ISSUE-28 its model holds both",
                "evaluate_js",
                model,
                "\"value\": \"new text more\"",
            ),
        ]
    }

    /// ISSUE-42: on Chrome the outline reaches a closed shadow root, and
    /// its button is clicked by ref; Firefox cannot see into it.
    fn closed_root_steps(instance: &str, chrome: bool) -> Vec<Step> {
        let i = json!(instance);
        let build = "const h = document.createElement('div'); h.id = 'closed42'; document.body.append(h); const r = h.attachShadow({mode: 'closed'}); r.innerHTML = '<button>closed e2e button</button>'; r.querySelector('button').onclick = () => { window.__closed = 1; }; 1";
        let run = |expression: &str| json!({"instance_id": i, "expression": expression});
        let find = json!({"instance_id": i, "text": "closed e2e button"});
        let mut steps = vec![(
            "ISSUE-42 a closed shadow root",
            "evaluate_js",
            run(build),
            "",
        )];
        if chrome {
            steps.extend([
                (
                    "ISSUE-42 the outline reaches it",
                    "find_in_snapshot",
                    find,
                    "button \"closed e2e button\" [ref=",
                ),
                (
                    "ISSUE-42 click its button by ref",
                    "click_element",
                    json!({"instance_id": i, "selector": "$REF"}),
                    "Successfully clicked",
                ),
                (
                    "ISSUE-42 its handler ran",
                    "evaluate_js",
                    run("window.__closed === 1"),
                    "true",
                ),
            ]);
        } else {
            steps.push((
                "ISSUE-42 Firefox cannot see into it",
                "find_in_snapshot",
                find,
                "No line of the snapshot",
            ));
        }
        steps.push((
            "ISSUE-42 remove it",
            "evaluate_js",
            run("document.getElementById('closed42').remove(); 1"),
            "",
        ));
        steps
    }

    /// ISSUE-41: a click that opens a tab says so (the tab closes itself).
    fn opened_tab_steps(instance: &str) -> Vec<Step> {
        let i = json!(instance);
        let link = "const a = document.createElement('a'); a.id = 'blank41'; a.href = '/self-close.html'; a.target = '_blank'; a.textContent = 'opens a tab'; document.body.prepend(a); 1";
        vec![
            (
                "ISSUE-41 a link to a new tab",
                "evaluate_js",
                json!({"instance_id": i, "expression": link}),
                "",
            ),
            (
                "ISSUE-41 the click says it opened a tab",
                "click_element",
                json!({"instance_id": i, "selector": "#blank41"}),
                "The action opened tab tab-",
            ),
            (
                "ISSUE-68 the opener stays in front",
                "evaluate_js",
                json!({"instance_id": i, "expression": "document.visibilityState"}),
                "\"visible\"",
            ),
            // (it closes itself 2 s after loading, once the click answered)
            ("ISSUE-41 let it close", "sleep", json!({}), ""),
            ("ISSUE-41 let it close, still", "sleep", json!({}), ""),
            (
                "ISSUE-68 the opener is in front once it closed",
                "evaluate_js",
                json!({"instance_id": i, "expression": "document.visibilityState"}),
                "\"visible\"",
            ),
            (
                "ISSUE-41 remove the link",
                "evaluate_js",
                json!({"instance_id": i, "expression": "document.getElementById('blank41').remove(); 1"}),
                "",
            ),
        ]
    }

    /// ISSUE-47: fill_input picks exactly the options of a <select multiple>
    /// named one per line; an unknown one changes nothing.
    fn multiple_select_steps(instance: &str) -> Vec<Step> {
        let i = json!(instance);
        let run = |expression: &str| json!({"instance_id": i, "expression": expression});
        let fill = |text: &str| json!({"instance_id": i, "selector": "#m47", "text": text});
        let build = "document.body.insertAdjacentHTML('beforeend', '<select id=\"m47\" multiple><option>M47 a</option><option selected>M47 b</option><option>M47 c</option></select>'); 1";
        let picked =
            "Array.from(document.getElementById('m47').selectedOptions).map(o => o.text).join('+')";
        vec![
            ("ISSUE-47 a multiple select", "evaluate_js", run(build), ""),
            (
                "ISSUE-47 pick two",
                "fill_input",
                fill("M47 a\nM47 c"),
                "Successfully selected 2 options",
            ),
            (
                "ISSUE-47 exactly those",
                "evaluate_js",
                run(picked),
                "M47 a+M47 c",
            ),
            (
                "ISSUE-47 an unknown one fails",
                "fill_input",
                fill("M47 a\nM47 z"),
                "!the value or label 'M47 z'; nothing was changed",
            ),
            (
                "ISSUE-47 nothing changed",
                "evaluate_js",
                run(picked),
                "M47 a+M47 c",
            ),
            (
                "ISSUE-47 remove it",
                "evaluate_js",
                run("document.getElementById('m47').remove(); 1"),
                "",
            ),
        ]
    }

    /// ISSUE-55: audit_page finds an image without alt text and faint text,
    /// naming each element (the same checks on both browsers).
    fn audit_steps(instance: &str) -> Vec<Step> {
        let i = json!(instance);
        let run = |expression: &str| json!({"instance_id": i, "expression": expression});
        let build = "document.body.insertAdjacentHTML('beforeend', '<div id=\"d55\"><img id=\"i55\" src=\"data:image/gif;base64,R0lGODlhAQABAAAAACw=\" width=\"30\" height=\"30\"><p id=\"p55\" style=\"color:#bbb;background:#fff\">D55 faint</p></div>'); 1";
        let audit = json!({"instance_id": i, "categories": ["accessibility"]});
        vec![
            (
                "ISSUE-55 an image without alt and faint text",
                "evaluate_js",
                run(build),
                "",
            ),
            (
                "ISSUE-55 the image is named",
                "audit_page",
                audit.clone(),
                "\"selector\": \"#i55\"",
            ),
            (
                "ISSUE-55 the faint text is named",
                "audit_page",
                audit,
                "contrast 1.92:1, needs 4.5:1 (#bbbbbb on #ffffff",
            ),
            (
                "ISSUE-55 remove them",
                "evaluate_js",
                run("document.getElementById('d55').remove(); 1"),
                "",
            ),
        ]
    }

    /// ISSUE-48: a draggable element and a drop zone have refs in snapshot.
    fn drag_ref_steps(instance: &str) -> Vec<Step> {
        let i = json!(instance);
        let run = |expression: &str| json!({"instance_id": i, "expression": expression});
        let build = "document.body.insertAdjacentHTML('beforeend', '<div id=\"d48\"><div draggable=\"true\">D48 card</div><div ondragover=\"event.preventDefault()\">D48 zone</div></div>'); 1";
        let outline = json!({"instance_id": i, "selector": "#d48"});
        vec![
            (
                "ISSUE-48 a card and a drop zone",
                "evaluate_js",
                run(build),
                "",
            ),
            (
                "ISSUE-48 the card has a ref",
                "snapshot",
                outline.clone(),
                "clickable \"D48 card\" [draggable] [ref=",
            ),
            (
                "ISSUE-48 the zone has a ref",
                "snapshot",
                outline,
                "clickable \"D48 zone\" [ref=",
            ),
            (
                "ISSUE-48 remove them",
                "evaluate_js",
                run("document.getElementById('d48').remove(); 1"),
                "",
            ),
        ]
    }

    /// ISSUE-49: snapshot marks a field the page rejects [invalid], with
    /// its error message.
    fn invalid_field_steps(instance: &str) -> Vec<Step> {
        let i = json!(instance);
        let run = |expression: &str| json!({"instance_id": i, "expression": expression});
        let build = "const d = document.createElement('div'); d.id = 'd49'; d.innerHTML = '<input id=\"i49\" aria-label=\"I49 zip\" aria-invalid=\"true\" aria-describedby=\"e49\"><span id=\"e49\">I49 digits only</span>'; document.body.append(d); 1";
        vec![
            ("ISSUE-49 an invalid field", "evaluate_js", run(build), ""),
            (
                "ISSUE-49 it is marked with why",
                "snapshot",
                json!({"instance_id": i, "selector": "#d49"}),
                "textbox \"I49 zip\" [invalid] [error=\"I49 digits only\"]",
            ),
            (
                "ISSUE-49 remove it",
                "evaluate_js",
                run("document.getElementById('d49').remove(); 1"),
                "",
            ),
        ]
    }

    /// ISSUE-46: snapshot lists a native <select>'s options under its
    /// combobox, the first ones only on a long list, which the search reaches.
    fn select_option_steps(instance: &str) -> Vec<Step> {
        let i = json!(instance);
        let run = |expression: &str| json!({"instance_id": i, "expression": expression});
        let build = "const s = document.createElement('select'); s.id = 's46'; s.setAttribute('aria-label', 'S46 pick'); for (let n = 0; n < 40; n++) s.add(new Option('S46 option ' + n)); s.options[3].disabled = true; document.body.append(s); 1";
        vec![
            ("ISSUE-46 a long select", "evaluate_js", run(build), ""),
            (
                "ISSUE-46 its options are listed",
                "snapshot",
                json!({"instance_id": i, "selector": "#s46"}),
                "  - option \"S46 option 3\" [disabled]",
            ),
            (
                "ISSUE-46 then how many more",
                "snapshot",
                json!({"instance_id": i, "selector": "#s46"}),
                "… 15 more options",
            ),
            (
                "ISSUE-46 the search reaches the last",
                "find_in_snapshot",
                json!({"instance_id": i, "text": "S46 option 39"}),
                "1 matches",
            ),
            (
                "ISSUE-46 remove it",
                "evaluate_js",
                run("document.getElementById('s46').remove(); 1"),
                "",
            ),
        ]
    }

    /// ISSUE-40: snapshot shows progress bars and meters with their value
    /// and whether a details is open; with a modal dialog open only the
    /// dialog is listed, marked modal.
    fn widget_state_steps(instance: &str) -> Vec<Step> {
        let i = json!(instance);
        let run = |expression: &str| json!({"instance_id": i, "expression": expression});
        let build = "const w = document.createElement('div'); w.id = 'w40'; w.innerHTML = '<progress value=\\'40\\' max=\\'100\\'></progress><details><summary>W40 more</summary><p>w40 hidden</p></details><dialog id=\\'w40d\\'><button>W40 ok</button></dialog>'; document.body.append(w); document.getElementById('w40d').showModal(); 1";
        let snapshot = json!({"instance_id": i});
        vec![
            (
                "ISSUE-40 open a modal dialog over widgets",
                "evaluate_js",
                run(build),
                "",
            ),
            (
                "ISSUE-40 the modal dialog is marked",
                "snapshot",
                snapshot.clone(),
                "dialog [modal]",
            ),
            (
                "ISSUE-40 nothing behind the modal is listed",
                "find_in_snapshot",
                json!({"instance_id": i, "text": "W40 more"}),
                "No line of the snapshot",
            ),
            (
                "ISSUE-40 close the dialog",
                "evaluate_js",
                run("document.getElementById('w40d').close(); 1"),
                "",
            ),
            (
                "ISSUE-40 a progress bar shows its value",
                "snapshot",
                snapshot.clone(),
                ": \"40/100\"",
            ),
            (
                "ISSUE-40 a closed details says so",
                "snapshot",
                snapshot,
                "button \"W40 more\" [expanded=false]",
            ),
            (
                "ISSUE-40 remove the widgets",
                "evaluate_js",
                run("document.getElementById('w40').remove(); 1"),
                "",
            ),
        ]
    }

    /// ISSUE-38: date, time, range and colour inputs get their value; a
    /// range snaps it (noted), a malformed date is refused and keeps the
    /// value it had.
    fn valued_input_steps(instance: &str) -> Vec<Step> {
        let i = json!(instance);
        let fill = |selector: &str, text: &str| json!({"instance_id": i, "selector": selector, "text": text});
        let values = json!({"instance_id": i, "expression": "['vdate', 'vtime', 'vrange', 'vcolor'].map(id => document.getElementById(id).value).join(' ')"});
        vec![
            (
                "ISSUE-38 set a date",
                "fill_input",
                fill("#vdate", "2026-10-31"),
                "Successfully set the value",
            ),
            (
                "ISSUE-38 set a time",
                "fill_input",
                fill("#vtime", "13:45"),
                "Successfully set the value",
            ),
            (
                "ISSUE-38 set a range, snapped",
                "fill_input",
                fill("#vrange", "72"),
                "holds \"70\"",
            ),
            (
                "ISSUE-38 set a colour",
                "fill_input",
                fill("#vcolor", "#FF0000"),
                "Successfully set the value",
            ),
            (
                "ISSUE-38 a malformed date is refused",
                "fill_input",
                fill("#vdate", "31/10/2026"),
                "!it expects YYYY-MM-DD",
            ),
            (
                "ISSUE-38 the inputs hold the values",
                "evaluate_js",
                values,
                "2026-10-31 13:45 70 #ff0000",
            ),
        ]
    }

    /// ISSUE-37: a click waits for an element about to appear and for a
    /// control about to be enabled; one that stays disabled is refused and
    /// its handler never runs.
    fn late_element_steps(instance: &str) -> Vec<Step> {
        let i = json!(instance);
        let click = |selector: &str| json!({"instance_id": i, "selector": selector});
        let read = |expression: &str| json!({"instance_id": i, "expression": expression});
        vec![
            (
                "ISSUE-37 arm the late elements",
                "click_element",
                click("#arm"),
                "Successfully clicked",
            ),
            (
                "ISSUE-37 click an element about to appear",
                "click_element",
                click("#appear"),
                "Successfully clicked",
            ),
            (
                "ISSUE-37 click a control about to be enabled",
                "click_element",
                click("#soon"),
                "Successfully clicked",
            ),
            (
                "ISSUE-37 both handlers ran",
                "evaluate_js",
                read("window.__late"),
                "\"value\": 2",
            ),
            (
                "ISSUE-37 a disabled control is refused",
                "click_element",
                click("#never"),
                "!is disabled; nothing was clicked",
            ),
            (
                "ISSUE-37 its handler did not run",
                "evaluate_js",
                read("window.__never === undefined"),
                "true",
            ),
        ]
    }

    /// ISSUE-29: snapshot shows the value of ARIA comboboxes, spinbuttons
    /// and sliders.
    fn aria_value_steps(instance: &str) -> Vec<Step> {
        let i = json!(instance);
        let line = |regex: &str| json!({"instance_id": i, "regex": regex});
        vec![
            (
                "ISSUE-29 a combobox input shows its value",
                "find_in_snapshot",
                line(r#"combobox "Movie" \[ref=e\d+\]: "The Godfather""#),
                "1 matches",
            ),
            (
                "ISSUE-29 a combobox button shows its text",
                "find_in_snapshot",
                line(r#"combobox "Food" \[ref=e\d+\]: "Blueberry""#),
                "1 matches",
            ),
            (
                "ISSUE-29 a spinbutton shows its aria-valuetext",
                "find_in_snapshot",
                line(r#"spinbutton "Month" \[ref=e\d+\]: "October""#),
                "1 matches",
            ),
            (
                "ISSUE-29 a slider shows its aria-valuenow",
                "find_in_snapshot",
                line(r#"slider "Volume" \[ref=e\d+\]: "30""#),
                "1 matches",
            ),
        ]
    }

    /// ISSUE-33: a capture of one element is its size (the PNG header of
    /// a 123 x 45 px image).
    fn element_shot_steps(instance: &str) -> Vec<Step> {
        let i = json!(instance);
        let mut steps = vec![(
            "ISSUE-33 screenshot of one element",
            "capture_screenshot",
            json!({"instance_id": i, "selector": "#shot"}),
            "iVBORw0KGgoAAAANSUhEUgAAAHsAAAAt",
        )];
        steps.extend(scroll_shot_steps(&i));
        steps
    }

    /// ISSUE-89: element captures put the page's and the boxes' scroll
    /// back, and an element outside the page is refused (both browsers).
    fn scroll_shot_steps(i: &Value) -> Vec<Step> {
        // ISSUE-89: an element far down and one outside the page
        let add = "document.body.insertAdjacentHTML('beforeend', '<div id=\"w89\"><div style=\"height:3000px\"></div><p id=\"low89\">low</p><div id=\"box89\" style=\"height:100px;overflow:auto\"><div style=\"height:2000px\"></div><p id=\"deep89\">deep</p></div><p id=\"off89\" style=\"position:absolute;left:-500px;top:5px;width:20px;height:20px\">off</p></div>'); scrollTo(0, 0); 1";
        vec![
            (
                "ISSUE-89 elements far down and outside the page",
                "evaluate_js",
                json!({"instance_id": i, "expression": add}),
                "",
            ),
            (
                "ISSUE-89 a capture far down",
                "capture_screenshot",
                json!({"instance_id": i, "selector": "#low89"}),
                "iVBORw0KGgo",
            ),
            (
                "ISSUE-89 leaves the page where it was",
                "evaluate_js",
                json!({"instance_id": i, "expression": "scrollY"}),
                "\"value\": 0",
            ),
            (
                "ISSUE-89 a capture inside a scrolled box",
                "capture_screenshot",
                json!({"instance_id": i, "selector": "#deep89"}),
                "iVBORw0KGgo",
            ),
            (
                "ISSUE-89 leaves the box and the page where they were",
                "evaluate_js",
                json!({"instance_id": i, "expression": "document.getElementById('box89').scrollTop + scrollY"}),
                "\"value\": 0",
            ),
            (
                "ISSUE-89 an element outside the page is refused",
                "capture_screenshot",
                json!({"instance_id": i, "selector": "#off89"}),
                "!is outside the page",
            ),
            (
                "ISSUE-89 remove them",
                "evaluate_js",
                json!({"instance_id": i, "expression": "document.getElementById('w89').remove(); 1"}),
                "",
            ),
        ]
    }

    /// ISSUE-32: fill_input with append types after the content: a text
    /// field, a number field (no caret, retyped whole) and the model-driven
    /// rich text editor, which holds "rich-typed" from ISSUE-23.
    fn append_steps(instance: &str) -> Vec<Step> {
        let i = json!(instance);
        let append = |selector: &str, text: &str| json!({"instance_id": i, "selector": selector, "text": text, "append": true});
        let read = |expression: &str| json!({"instance_id": i, "expression": expression});
        vec![
            (
                "ISSUE-32 append to a text field",
                "fill_input",
                append("#app", " more"),
                "Successfully appended",
            ),
            (
                "ISSUE-32 the field holds both",
                "evaluate_js",
                read("document.getElementById('app').value"),
                "\"value\": \"draft more\"",
            ),
            (
                "ISSUE-32 append to a number field",
                "fill_input",
                append("#appn", "3"),
                "Successfully appended",
            ),
            (
                "ISSUE-32 the number is 123",
                "evaluate_js",
                read("document.getElementById('appn').value"),
                "\"value\": \"123\"",
            ),
            (
                "ISSUE-32 append in a rich text editor",
                "fill_input",
                append("#rich", "-more"),
                "Successfully appended",
            ),
            (
                "ISSUE-32 the editor's model holds both",
                "evaluate_js",
                read("window.__rich"),
                "\"value\": \"rich-typed-more\"",
            ),
        ]
    }

    /// ISSUE-26: a button shown only while its row is hovered is clicked.
    fn hover_reveal_steps(instance: &str) -> Vec<Step> {
        let i = json!(instance);
        vec![
            (
                "ISSUE-26 click a control shown on hover",
                "click_element",
                json!({"instance_id": i, "selector": "#more"}),
                "Successfully clicked",
            ),
            (
                "ISSUE-26 its handler ran",
                "evaluate_js",
                json!({"instance_id": i, "expression": "window.__more"}),
                "\"value\": 1",
            ),
        ]
    }

    /// ISSUE-25: a clickable container lists its links and buttons; a
    /// clickable with text only stays one named line.
    fn clickable_steps(instance: &str) -> Vec<Step> {
        let i = json!(instance);
        let scoped = |selector: &str| json!({"instance_id": i, "selector": selector});
        vec![
            (
                "ISSUE-25 a clickable container lists its contents",
                "snapshot",
                scoped("#card"),
                "\n  - link \"card-link\" [href=/card]",
            ),
            (
                "ISSUE-25 a plain clickable stays one named line",
                "snapshot",
                scoped("#plaincard"),
                "- clickable \"plain card text\" [ref=",
            ),
        ]
    }

    /// ISSUE-24: a styled checkbox under its own label's decoration is
    /// clicked; an element under something else is still refused.
    fn label_steps(instance: &str) -> Vec<Step> {
        let i = json!(instance);
        vec![
            (
                "ISSUE-24 click a checkbox covered by its label",
                "click_element",
                json!({"instance_id": i, "selector": "#sw"}),
                "Successfully clicked",
            ),
            (
                "ISSUE-24 the checkbox is checked",
                "evaluate_js",
                json!({"instance_id": i, "expression": "document.getElementById('sw').checked"}),
                "true",
            ),
            (
                "ISSUE-24 an element under another one is refused",
                "click_element",
                json!({"instance_id": i, "selector": "#covered"}),
                "!is covered by 'div'",
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
    /// ISSUE-99: Chrome records a video of the tab while it changes;
    /// Firefox refuses with a capability error.
    fn video_steps(instance: &str, chrome: bool) -> Vec<Step> {
        let i = json!(instance);
        let start = json!({"instance_id": i, "action": "start", "frame_rate": 10});
        if !chrome {
            let refused = "!not available on Firefox";
            return vec![(
                "ISSUE-99 record_video on Firefox",
                "record_video",
                start,
                refused,
            )];
        }
        let path = format!("{}/target/e2e-{instance}.mp4", env!("CARGO_MANIFEST_DIR"));
        let animate = "(() => { let n = 0; window.__v99 = setInterval(() => { document.body.style.background = `hsl(${(n += 30) % 360} 80% 50%)`; }, 50); return 1; })()";
        let stop_animation = "(() => { clearInterval(window.__v99); document.body.style.background = ''; return 1; })()";
        vec![
            (
                "ISSUE-99 start recording",
                "record_video",
                start,
                "Recording the tab",
            ),
            (
                "ISSUE-99 the page changes",
                "evaluate_js",
                json!({"instance_id": i, "expression": animate}),
                "",
            ),
            ("ISSUE-99 let it record", "sleep", json!({}), ""),
            (
                "ISSUE-99 stop writes the video",
                "record_video",
                json!({"instance_id": i, "action": "stop", "path": path}),
                "(MP4) to",
            ),
            (
                "ISSUE-99 the page stops changing",
                "evaluate_js",
                json!({"instance_id": i, "expression": stop_animation}),
                "",
            ),
        ]
    }

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
        let mut steps = vec![
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
        ];
        steps.extend(input_effect_steps(instance));
        steps
    }

    /// ISSUE-81: the widgets of `input_effect_steps`, recording the Enter
    /// keys of the textarea and the wheel over the canvas.
    fn input_widgets_js() -> String {
        let html = r#"<div id="ie81"><textarea id="ta81"></textarea><canvas id="cv81" width="200" height="60"></canvas><input type="file" id="f81"><button id="p81" onclick="history.pushState({}, '', '/pushed81')">push81</button></div>"#;
        format!(
            "document.body.insertAdjacentHTML('afterbegin', {}); window.__url81 = location.href; window.__enter81 = 0; window.__wheel81 = 0; \
             ta81.addEventListener('keydown', e => {{ if (e.key === 'Enter') __enter81++; }}); \
             cv81.addEventListener('wheel', e => {{ __wheel81 += e.deltaY; e.preventDefault(); }}); 1",
            json!(html)
        )
    }

    /// ISSUE-81: a newline in a textarea is the Enter key, `wheel` turns
    /// the mouse wheel over a canvas, and a click that opens a file chooser
    /// or changes the URL without loading a page says so (both browsers).
    fn input_effect_steps(instance: &str) -> Vec<Step> {
        let i = json!(instance);
        let eval = |expression: &str| json!({"instance_id": i, "expression": expression});
        let mut steps = vec![
            (
                "ISSUE-81 input widgets",
                "evaluate_js",
                eval(&input_widgets_js()),
                "",
            ),
            (
                "ISSUE-81 a newline in a textarea",
                "fill_input",
                json!({"instance_id": i, "selector": "#ta81", "text": "l1\nl2"}),
                "Successfully filled",
            ),
            (
                "ISSUE-81 two lines, one Enter key",
                "evaluate_js",
                eval("ta81.value.split('\\n').length + ':' + __enter81"),
                "2:1",
            ),
            (
                "ISSUE-81 the wheel over a canvas",
                "scroll",
                json!({"instance_id": i, "selector": "#cv81", "wheel": true, "y": 120}),
                "Dispatched a wheel event (deltaX 0, deltaY 120)",
            ),
            (
                "ISSUE-81 the canvas got it",
                "evaluate_js",
                eval("__wheel81"),
                "120",
            ),
        ];
        steps.extend(input_note_steps(instance));
        steps
    }

    /// ISSUE-81: a click that opens a file chooser, or changes the URL
    /// without loading a page, says so; the page's URL is put back.
    fn input_note_steps(instance: &str) -> Vec<Step> {
        let i = json!(instance);
        let eval = |expression: &str| json!({"instance_id": i, "expression": expression});
        let on = |selector: &str| json!({"instance_id": i, "selector": selector});
        let back =
            "history.replaceState({}, '', __url81); document.getElementById('ie81').remove(); 1";
        vec![
            (
                "ISSUE-81 a click opening a file chooser",
                "click_element",
                on("#f81"),
                "The action opened a file chooser",
            ),
            (
                "ISSUE-81 a click changing the URL",
                "click_element",
                on("#p81"),
                "The URL changed to ",
            ),
            (
                "ISSUE-81 back to the page's URL",
                "evaluate_js",
                eval(back),
                "",
            ),
        ]
    }

    /// ISSUE-51: a key written with its modifiers (Control+a, as agents
    /// write it) holds them: it selects the field's text, typed over next.
    fn key_combination_steps(instance: &str) -> Vec<Step> {
        let i = json!(instance);
        let value = "document.querySelector('#pk').value";
        vec![
            (
                "ISSUE-51 press Control+a",
                "press_key",
                json!({"instance_id": i, "selector": "#pk", "key": "Control+a"}),
                "Pressed Control+a in '#pk'.",
            ),
            (
                "ISSUE-51 type over the selection",
                "press_key",
                json!({"instance_id": i, "key": "z"}),
                "Pressed z",
            ),
            (
                "ISSUE-51 the field holds it alone",
                "evaluate_js",
                json!({"instance_id": i, "expression": value}),
                "\"value\": \"z\"",
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
    /// ISSUE-61: the CPU performance tier the page reads (Chrome), and a
    /// capability error on Firefox, which has no navigator.cpuPerformance.
    fn cpu_tier_steps(i: &Value, chrome: bool) -> Vec<Step> {
        let read = match chrome {
            true => "\"cpu_performance\": 1",
            false => "!cpu_performance_tier not available on Firefox",
        };
        let low = json!({"instance_id": i, "cpu_performance_tier": "low"});
        vec![("ISSUE-61 a low CPU performance tier", "emulate", low, read)]
    }

    /// ISSUE-92: a phone viewport on a page without a viewport meta tag
    /// lays out at 980 px, and the reply says why (Chrome; Firefox has no
    /// mobile emulation and refuses it).
    fn mobile_layout_steps(i: &Value, chrome: bool) -> Vec<Step> {
        let phone = json!({"instance_id": i, "width": 390, "height": 844, "mobile": true});
        let expected = match chrome {
            true => "lays out at 980 px, wider than 390",
            false => "!mobile not available on Firefox",
        };
        let reset = json!({"instance_id": i, "reset": true});
        vec![
            ("ISSUE-92 a phone viewport", "emulate", phone, expected),
            ("ISSUE-92 reset", "emulate", reset, "Emulation reset"),
        ]
    }

    fn emulate_steps(instance: &str, chrome: bool) -> Vec<Step> {
        let i = json!(instance);
        let dark = if chrome {
            "\"color_scheme\": \"dark\""
        } else {
            "!color_scheme not available on Firefox"
        };
        let throttle = if chrome {
            "\"cpu_throttling\": 4.0"
        } else {
            "!cpu_throttling, network_throttling not available on Firefox"
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
                "ISSUE-52 throttle CPU and network",
                "emulate",
                json!({"instance_id": i, "cpu_throttling": 4.0, "network_throttling": "fast-3g"}),
                throttle,
            ),
        ]
        .into_iter()
        .chain(cpu_tier_steps(&i, chrome))
        .chain(emulate_identity_steps(&i))
        .chain([(
            "ISSUE-9 reset",
            "emulate",
            json!({"instance_id": i, "reset": true}),
            "Emulation reset",
        )])
        .chain(after_reset_steps(&i))
        .chain(mobile_layout_steps(&i, chrome))
        .collect()
    }

    /// ISSUE-43: emulate adds to earlier calls, sets the user agent,
    /// locale, time zone and position, and refuses an unknown time zone.
    fn emulate_identity_steps(i: &Value) -> Vec<Step> {
        vec![
            (
                "ISSUE-43 a later call keeps the earlier ones",
                "emulate",
                json!({"instance_id": i, "device_scale_factor": 3.0}),
                "\"width\": 500",
            ),
            (
                "ISSUE-43 user agent, locale, time zone and position",
                "emulate",
                json!({"instance_id": i, "user_agent": "E2E-Agent/1", "locale": "fr-FR", "timezone": "Asia/Tokyo",
                    "geolocation": {"latitude": 40.4, "longitude": -3.7}}),
                "\"timezone\": \"Asia/Tokyo\"",
            ),
            (
                "ISSUE-43 the page reads the position",
                "evaluate_js",
                json!({"instance_id": i, "expression": "const p = await new Promise(r => navigator.geolocation.getCurrentPosition(r)); p.coords.latitude"}),
                "40.4",
            ),
            (
                "ISSUE-43 an unknown time zone changes nothing",
                "emulate",
                json!({"instance_id": i, "timezone": "Mars/Base"}),
                "!nothing was changed",
            ),
        ]
    }

    /// ISSUE-43: what a reset restores, and evaluate_js with top-level
    /// await and a symbol.
    fn after_reset_steps(i: &Value) -> Vec<Step> {
        vec![
            (
                "ISSUE-43 the reset restores the user agent",
                "evaluate_js",
                json!({"instance_id": i, "expression": "navigator.userAgent.includes('E2E-Agent')"}),
                "false",
            ),
            (
                "ISSUE-43 top-level await",
                "evaluate_js",
                json!({"instance_id": i, "expression": "const r = await fetch('/api'); r.json()"}),
                "\"ok\": true",
            ),
            (
                "ISSUE-43 a symbol is described",
                "evaluate_js",
                json!({"instance_id": i, "expression": "Symbol('e2e')"}),
                "Symbol(e2e)",
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
    /// ISSUE-75: a dedicated worker's request and console message reach
    /// the tab's logs, the message with its own level (Chrome lost the
    /// request and reported `console.log` as `info`).
    fn worker_steps(instance: &str) -> Vec<Step> {
        let i = json!(instance);
        let start = "new Promise(r => { const w = new Worker('/worker75.js'); w.onmessage = () => r('worker done'); })";
        vec![
            (
                "ISSUE-75 run a worker",
                "evaluate_js",
                json!({"instance_id": i, "expression": start}),
                "worker done",
            ),
            (
                "ISSUE-75 the worker's request is logged",
                "get_network_logs",
                json!({"instance_id": i, "url_filter": "from=worker75"}),
                "=> 200",
            ),
            (
                "ISSUE-75 the worker's console.log is a log",
                "get_console_logs",
                json!({"instance_id": i}),
                "[log] worker75-log",
            ),
        ]
    }

    /// ISSUE-78: a redirected request is found by its first URL and says
    /// it was redirected; a request body is kept (Firefox lost it); a CORS
    /// block names its reason (Chrome said only net::ERR_FAILED, Firefox
    /// showed 200 OK).
    fn request_detail_steps(instance: &str) -> Vec<Step> {
        let i = json!(instance);
        let run = "(async () => { const r = await fetch('/hop78'); await fetch('/api?post78', {method: 'POST', body: 'body-78'}); \
            let cors = 'read'; try { await fetch('http://localhost:' + location.port + '/api?cors78'); } catch (e) { cors = 'blocked'; } \
            return r.status + ' ' + cors; })()";
        let shown = json!({"instance_id": i, "url_filter": "post78", "include_details": true, "include_sensitive_headers": true});
        vec![
            (
                "ISSUE-78 a redirect, a POST and a CORS block",
                "evaluate_js",
                json!({"instance_id": i, "expression": run}),
                "200 blocked",
            ),
            (
                "ISSUE-78 the redirect is found by its first URL",
                "get_network_logs",
                json!({"instance_id": i, "url_filter": "/hop78"}),
                "after-hop78 => 200",
            ),
            (
                "ISSUE-78 the summary says it was redirected",
                "get_network_logs",
                json!({"instance_id": i, "url_filter": "/hop78"}),
                "(after 1 redirect from ",
            ),
            (
                "ISSUE-78 the request body is kept",
                "get_network_logs",
                shown,
                "\"request_post_data\": \"body-78\"",
            ),
            (
                "ISSUE-78 the CORS block names its reason",
                "get_network_logs",
                json!({"instance_id": i, "url_filter": "cors78"}),
                "blocked by CORS: ",
            ),
        ]
    }

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

    /// ISSUE-57: metrics after a garbage collection (Chrome collects
    /// first; Firefox reads them and notes it cannot).
    fn memory_steps(instance: &str) -> Vec<Step> {
        vec![(
            "ISSUE-57 metrics after a garbage collection",
            "get_performance_metrics",
            json!({"instance_id": instance, "collect_garbage": true}),
            "Nodes",
        )]
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

    /// ISSUE-30: drag moves an HTML5 draggable onto its drop target and a
    /// pointer-driven knob onto a zone, then the test page comes back.
    fn drag_steps(instance: &str, base: &str) -> Vec<Step> {
        let i = json!(instance);
        let drag = |from: &str, to: &str| json!({"instance_id": i, "from": from, "to": to});
        let read = |expression: &str| json!({"instance_id": i, "expression": expression});
        vec![
            (
                "ISSUE-30 open the drag page",
                "navigate",
                json!({"instance_id": i, "url": format!("{base}/drag.html")}),
                "Navigated to",
            ),
            (
                "ISSUE-30 HTML5 drag and drop",
                "drag",
                drag("#src", "#dst"),
                "Dragged '#src' onto '#dst'",
            ),
            (
                "ISSUE-30 the target got the data",
                "evaluate_js",
                read("window.__html5"),
                "dropped:payload",
            ),
            (
                "ISSUE-30 pointer drag",
                "drag",
                drag("#knob", "#zone"),
                "Dragged '#knob' onto '#zone'",
            ),
            (
                "ISSUE-30 the knob landed in the zone",
                "evaluate_js",
                read("window.__pointer"),
                "\"value\": \"dropped\"",
            ),
            (
                "ISSUE-30 back to the test page",
                "navigate",
                json!({"instance_id": i, "url": format!("{base}/")}),
                "Navigated to",
            ),
        ]
    }

    /// ISSUE-27: snapshot reads a frame of another origin with frame refs,
    /// and the input tools act in it through them.
    fn cross_origin_steps(instance: &str, base: &str) -> Vec<Step> {
        let i = json!(instance);
        let at = |selector: &str| json!({"instance_id": i, "selector": selector});
        vec![
            (
                "ISSUE-27 open a page with a frame of another origin",
                "navigate",
                json!({"instance_id": i, "url": format!("{base}/xo-top.html")}),
                "Navigated to",
            ),
            (
                "ISSUE-27 snapshot reads the frame",
                "snapshot",
                json!({"instance_id": i}),
                "button \"inner button\" [ref=f1e1]",
            ),
            (
                "ISSUE-27 click in the frame",
                "click_element",
                at("f1e1"),
                "Successfully clicked",
            ),
            (
                "ISSUE-27 fill a field in the frame",
                "fill_input",
                json!({"instance_id": i, "selector": "f1 >>> #iq", "text": "xo-typed"}),
                "Successfully filled",
            ),
            (
                "ISSUE-27 press a key in the frame",
                "press_key",
                json!({"instance_id": i, "selector": "f1e2", "key": "!"}),
                "Pressed !",
            ),
        ]
    }

    /// ISSUE-36: the console and requests of a frame of another origin
    /// (out of process on Chrome) are the tab's, response bodies included.
    fn frame_observation_steps(instance: &str) -> Vec<Step> {
        let i = json!(instance);
        let framed = json!({"instance_id": i, "url_filter": "from-frame", "include_details": true});
        vec![
            (
                "ISSUE-36 the frame's console message",
                "get_console_logs",
                json!({"instance_id": i}),
                "xo inner log",
            ),
            (
                "ISSUE-36 the frame's request and its body",
                "get_network_logs",
                framed,
                r#""response_body": "{\"ok\":true}""#,
            ),
        ]
    }

    /// ISSUE-36: the debugger reaches the script of a frame of
    /// another origin: found, a breakpoint by URL binds in it, a click in
    /// the frame pauses there, and evaluate, step and resume act there.
    fn frame_debugger_steps(instance: &str, base: &str) -> Vec<Step> {
        let i = json!(instance);
        let script = format!("{}/xo-inner.js", base.replace("127.0.0.1", "localhost"));
        let bp = json!({"instance_id": i, "url": script, "line_number": 1});
        let id = json!({"instance_id": i});
        vec![
            (
                "ISSUE-36 search the frame's script",
                "search_scripts",
                json!({"instance_id": i, "query": "const doubled"}),
                "const doubled = n * 2",
            ),
            (
                "ISSUE-36 a breakpoint binds in the frame",
                "set_breakpoint",
                bp,
                "\"resolved\": true",
            ),
            (
                "ISSUE-36 a click pauses in the frame",
                "click_element",
                json!({"instance_id": i, "selector": "f1 >>> #ibp"}),
                "paused in the debugger",
            ),
            (
                "ISSUE-36 evaluate in the frame's pause",
                "evaluate_on_call_frame",
                json!({"instance_id": i, "expression": "n * 2"}),
                "42",
            ),
            (
                "ISSUE-36 step in the frame",
                "step_over",
                id.clone(),
                "Stepped",
            ),
            ("ISSUE-36 resume the frame", "resume", id.clone(), "resumed"),
            (
                "ISSUE-36 remove the frame's breakpoint",
                "remove_breakpoint",
                json!({"instance_id": i, "breakpoint_id": "bp-1"}),
                "removed",
            ),
            (
                "ISSUE-36 the handler finished",
                "find_in_snapshot",
                json!({"instance_id": i, "text": "xo 42"}),
                "1 matches",
            ),
        ]
    }

    /// ISSUE-27: what the cross-origin steps did shows in the frame's
    /// outline and in a search; then the test page comes back.
    fn cross_origin_result_steps(instance: &str, base: &str) -> Vec<Step> {
        let i = json!(instance);
        let at = |selector: &str| json!({"instance_id": i, "selector": selector});
        vec![
            (
                "ISSUE-27 the frame holds the text",
                "snapshot",
                at("f1"),
                "textbox \"inner field\" [ref=f1e2]: \"xo-typed!\"",
            ),
            (
                "ISSUE-27 find_in_snapshot searches the frame",
                "find_in_snapshot",
                json!({"instance_id": i, "text": "xo clicked"}),
                "1 matches",
            ),
            (
                "ISSUE-27 a bare frame is no element",
                "click_element",
                at("f1"),
                "!names a frame, not an element",
            ),
            (
                "ISSUE-27 back to the test page",
                "navigate",
                json!({"instance_id": i, "url": format!("{base}/")}),
                "Navigated to",
            ),
        ]
    }

    /// The steps on widgets that need more than a plain click or typing:
    /// ARIA values, late or disabled elements, valued inputs, custom
    /// editors, iframes, appended text.
    fn widget_steps(instance: &str) -> Vec<Step> {
        [
            aria_value_steps(instance),
            widget_state_steps(instance),
            select_option_steps(instance),
            invalid_field_steps(instance),
            drag_ref_steps(instance),
            audit_steps(instance),
            multiple_select_steps(instance),
            opened_tab_steps(instance),
            self_close_steps(instance),
            url_check_steps(instance),
            placeholder_steps(instance),
            late_element_steps(instance),
            valued_input_steps(instance),
            custom_editor_steps(instance),
            frame_focus_steps(instance),
            append_steps(instance),
        ]
        .concat()
    }

    /// ISSUE-39: a URL the browser saves (a CSV, without
    /// Content-Disposition) fails as a download on both browsers.
    fn download_steps(instance: &str, base: &str) -> Vec<Step> {
        let i = json!(instance);
        let to = json!({"instance_id": i, "url": format!("{base}/data.csv")});
        let link = "{ const link = document.createElement('a'); link.id = 'dl77'; link.href = '/data.csv?e2e77'; link.textContent = 'save'; document.body.prepend(link); } 1";
        vec![
            (
                "ISSUE-39 a saved type is a download",
                "navigate",
                to,
                "!the URL is a download",
            ),
            // ISSUE-77: a click that downloads a file says where it went
            (
                "ISSUE-77 a download link",
                "evaluate_js",
                json!({"instance_id": i, "expression": link}),
                "",
            ),
            (
                "ISSUE-77 the click names the saved file",
                "click_element",
                json!({"instance_id": i, "selector": "#dl77"}),
                "The action downloaded data",
            ),
            (
                "ISSUE-77 list_downloads lists it",
                "list_downloads",
                json!({"instance_id": i}),
                "e2e77",
            ),
            (
                "ISSUE-77 it is complete",
                "list_downloads",
                json!({"instance_id": i}),
                "\"state\": \"completed\"",
            ),
        ]
    }

    /// ISSUE-50: a page behind HTTP authentication fails at once saying so
    /// on both browsers (Firefox waited 30 s on its prompt), and the
    /// credentials in the URL log in.
    fn auth_steps(instance: &str, base: &str) -> Vec<Step> {
        let to = |url: String| json!({"instance_id": instance, "url": url});
        let with_credentials = base.replace("http://", "http://user:pass@");
        vec![
            (
                "ISSUE-50 a page behind authentication says so",
                "navigate",
                to(format!("{base}/auth")),
                "!the page asks for HTTP authentication (Basic, realm \"e2e\")",
            ),
            (
                "ISSUE-50 credentials in the URL log in",
                "navigate",
                to(format!("{with_credentials}/auth")),
                "Navigated to",
            ),
        ]
    }

    /// The steps that leave the test page and come back to it.
    fn other_page_steps(instance: &str, base: &str) -> Vec<Step> {
        [
            download_steps(instance, base),
            auth_steps(instance, base),
            drag_steps(instance, base),
            cross_origin_steps(instance, base),
            frame_observation_steps(instance),
            frame_debugger_steps(instance, base),
            cross_origin_result_steps(instance, base),
            beforeunload_steps(instance, base),
            history_steps(instance, base),
        ]
        .concat()
    }

    /// ISSUE-76: a navigate right after accepting a beforeunload lands
    /// (Firefox cancelled it with "Beforeunload prompt was rejected").
    fn beforeunload_steps(instance: &str, base: &str) -> Vec<Step> {
        let i = json!(instance);
        // Armed by a click: browsers ask only after a user gesture.
        let button = "const b = document.createElement('button'); b.id = 'arm76'; b.textContent = 'arm'; b.onclick = () => { window.onbeforeunload = e => { e.preventDefault(); return ''; }; }; document.body.prepend(b); 1";
        vec![
            (
                "ISSUE-76 a button arming beforeunload",
                "evaluate_js",
                json!({"instance_id": i, "expression": button}),
                "",
            ),
            (
                "ISSUE-76 arm it",
                "click_element",
                json!({"instance_id": i, "selector": "#arm76"}),
                "Successfully",
            ),
            (
                "ISSUE-76 navigate stops at the beforeunload",
                "navigate",
                json!({"instance_id": i, "url": format!("{base}/frame.html")}),
                "!beforeunload",
            ),
            (
                "ISSUE-76 accept it",
                "handle_dialog",
                json!({"instance_id": i, "accept": true}),
                "Dialog accepted",
            ),
            (
                "ISSUE-76 navigate right after lands",
                "navigate",
                json!({"instance_id": i, "url": format!("{base}/")}),
                "Navigated to",
            ),
        ]
    }

    /// ISSUE-31: navigate moves back and forward in the tab's history and
    /// ends on the test page; past the last entry it fails without moving.
    fn history_steps(instance: &str, base: &str) -> Vec<Step> {
        let i = json!(instance);
        let step = |history: &str| json!({"instance_id": i, "history": history});
        vec![
            (
                "ISSUE-31 navigate to another page",
                "navigate",
                json!({"instance_id": i, "url": format!("{base}/frame.html")}),
                "Navigated to",
            ),
            (
                "ISSUE-31 history back",
                "navigate",
                step("back"),
                "Went back to",
            ),
            (
                "ISSUE-31 history forward",
                "navigate",
                step("forward"),
                "Went forward to",
            ),
            (
                "ISSUE-31 no next page",
                "navigate",
                step("forward"),
                "!no next page",
            ),
            (
                "ISSUE-31 back to the test page",
                "navigate",
                step("back"),
                "Went back to",
            ),
            (
                "ISSUE-31 the test page is loaded",
                "evaluate_js",
                // (says where it is when it fails: ISSUE-94)
                json!({"instance_id": i, "expression": "(document.getElementById('go') ? '[go] ' : '[no-go] ') + document.readyState + ' ' + location.pathname + ' history ' + history.length"}),
                "[go] complete /",
            ),
        ]
    }

    /// Breakpoint pause, frame evaluation, step, resume, script search.
    fn debugger_steps(instance: &str, base: &str, chrome: bool) -> Vec<Step> {
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
            (
                "ISSUE-62 search_scripts finds inline scripts",
                "search_scripts",
                json!({"instance_id": i, "query": "hello from e2e"}),
                "hello from e2e",
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
        ];
        steps.extend(paused_page_steps(&i));
        steps.extend(click_into_breakpoint_steps(&i));
        steps.extend(async_click_into_breakpoint_steps(&i));
        steps.extend(pause_on_navigate_steps(&i, base));
        steps.extend(loop_breakpoint_steps(&i, base));
        steps.extend(debugger_wording_steps(&i));
        steps.extend(stack_steps(&i, base, chrome));
        steps.extend(exception_pause_steps(&i));
        steps.extend(navigate_while_paused_steps(&i, base));
        steps.extend(conditional_steps(&i));
        steps
    }

    /// ISSUE-87: by the script's file name, a logpoint logs without pausing
    /// (both browsers).
    fn logpoint_steps(i: &Value) -> Vec<Step> {
        let logpoint = json!({"instance_id": i, "url": "app.js", "line_number": 9, "log": "k * 10", "condition": "k === 3"});
        vec![
            (
                "ISSUE-87 a logpoint by file name",
                "set_breakpoint",
                logpoint,
                "Logpoint set",
            ),
            (
                "ISSUE-87 the loop runs without pausing",
                "evaluate_js",
                json!({"instance_id": i, "expression": "appLoop(5)"}),
                "10",
            ),
            (
                "ISSUE-87 the logpoint logged",
                "get_console_logs",
                json!({"instance_id": i}),
                "app.js:9: 30",
            ),
            (
                "ISSUE-87 remove the logpoint",
                "remove_breakpoint",
                json!({"instance_id": i, "breakpoint_id": "bp-7"}),
                "removed",
            ),
        ]
    }

    /// ISSUE-87: a conditional breakpoint pauses on the iteration asked for
    /// (both browsers).
    fn conditional_steps(i: &Value) -> Vec<Step> {
        let on = json!({"instance_id": i});
        let breakpoint =
            json!({"instance_id": i, "url": "app.js", "line_number": 9, "condition": "k === 2"});
        let run = json!({"instance_id": i, "expression": "setTimeout(() => appLoop(5), 50); 1"});
        let mut steps = logpoint_steps(i);
        steps.extend([
            (
                "ISSUE-87 a conditional breakpoint",
                "set_breakpoint",
                breakpoint,
                "Breakpoint set",
            ),
            ("ISSUE-87 run the loop", "evaluate_js", run, ""),
            ("ISSUE-87 wait for the pause", "sleep", json!({}), ""),
            (
                "ISSUE-87 paused on the iteration asked for",
                "evaluate_on_call_frame",
                json!({"instance_id": i, "expression": "k"}),
                "\"value\": 2",
            ),
            ("ISSUE-87 resume", "resume", on, "resumed"),
            (
                "ISSUE-87 remove it",
                "remove_breakpoint",
                json!({"instance_id": i, "breakpoint_id": "bp-8"}),
                "removed",
            ),
        ]);
        steps
    }

    /// ISSUE-83: navigating while paused leaves the new page unpaused (both
    /// browsers): no stale stack, and input tools run again.
    fn navigate_while_paused_steps(i: &Value, base: &str) -> Vec<Step> {
        let bp = json!({"instance_id": i, "url": format!("{base}/app.js"), "line_number": 21});
        let trigger = json!({"instance_id": i, "expression": "setTimeout(() => appBuy(), 50); 1"});
        let on = json!({"instance_id": i});
        vec![
            (
                "ISSUE-83 a breakpoint",
                "set_breakpoint",
                bp,
                "Breakpoint set",
            ),
            ("ISSUE-83 run into it", "evaluate_js", trigger, ""),
            ("ISSUE-83 wait for the pause", "sleep", json!({}), ""),
            (
                "ISSUE-83 paused",
                "call_stack",
                on.clone(),
                "Paused (breakpoint)",
            ),
            (
                "ISSUE-83 navigate while paused",
                "navigate",
                json!({"instance_id": i, "url": format!("{base}/")}),
                "Navigated",
            ),
            (
                "ISSUE-83 no stale pause",
                "call_stack",
                on.clone(),
                "!not paused",
            ),
            (
                "ISSUE-83 input runs again",
                "click_element",
                json!({"instance_id": i, "selector": "#late"}),
                "clicked",
            ),
            (
                "ISSUE-83 remove the breakpoint",
                "remove_breakpoint",
                json!({"instance_id": i, "breakpoint_id": "bp-6"}),
                "removed",
            ),
        ]
    }

    /// ISSUE-66: search_scripts leaves out the server's own snippets (a
    /// snapshot just ran one), and the debugger tools word "not paused" the
    /// same on both browsers.
    /// ISSUE-67: the stack of a pause in nested calls, stepping into and
    /// out of a call, and evaluating in a caller's frame (both browsers);
    /// ISSUE-100: in an enclosing scope (Chrome; Firefox refuses it).
    fn stack_steps(i: &Value, base: &str, chrome: bool) -> Vec<Step> {
        let bp = json!({"instance_id": i, "url": format!("{base}/app.js"), "line_number": 21});
        let trigger = json!({"instance_id": i, "expression": "setTimeout(() => appBuy(), 50); 1"});
        let on = json!({"instance_id": i});
        let caller = json!({"instance_id": i, "expression": "typeof appTotal", "frame_index": 1});
        vec![
            (
                "ISSUE-67 a breakpoint before a call",
                "set_breakpoint",
                bp,
                "Breakpoint set",
            ),
            ("ISSUE-67 run it", "evaluate_js", trigger, ""),
            ("ISSUE-67 wait for the pause", "sleep", json!({}), ""),
            (
                "ISSUE-67 the stack",
                "call_stack",
                on.clone(),
                "\"function_name\": \"appBuy\"",
            ),
            (
                "ISSUE-67 step into the call",
                "step_into",
                on.clone(),
                "Stepped into the call",
            ),
            (
                "ISSUE-67 the callee is on top",
                "call_stack",
                on.clone(),
                "\"function_name\": \"appTotal\"",
            ),
            (
                "ISSUE-67 evaluate in the caller",
                "evaluate_on_call_frame",
                caller,
                "function",
            ),
            scope_step(i, chrome),
            (
                "ISSUE-67 step out to the caller",
                "step_out",
                on.clone(),
                "Stepped out to the caller",
            ),
            ("ISSUE-67 resume", "resume", on, "resumed"),
        ]
    }

    /// ISSUE-100: evaluating in an enclosing scope of the paused frame
    /// (Chrome; Firefox refuses it).
    fn scope_step(i: &Value, chrome: bool) -> Step {
        // `list` is appTotal's parameter: an enclosing scope has none
        let enclosing = json!({"instance_id": i, "expression": "typeof list", "scope": 1});
        let in_scope = match chrome {
            true => "\"value\": \"undefined\"",
            false => "!'scope' is not available on Firefox",
        };
        let label = "ISSUE-100 evaluate in an enclosing scope";
        (label, "evaluate_on_call_frame", enclosing, in_scope)
    }

    /// ISSUE-67: the breakpoint of `stack_steps` removed, then pausing on
    /// uncaught exceptions, the stack saying so, and turning it off (both
    /// browsers).
    fn exception_pause_steps(i: &Value) -> Vec<Step> {
        let mode = |state: &str| json!({"instance_id": i, "state": state});
        let trigger = json!({"instance_id": i, "expression": "setTimeout(() => appBoom(), 50); 1"});
        let on = json!({"instance_id": i});
        vec![
            (
                "ISSUE-67 remove the breakpoint",
                "remove_breakpoint",
                json!({"instance_id": i, "breakpoint_id": "bp-5"}),
                "removed",
            ),
            (
                "ISSUE-67 pause on uncaught exceptions",
                "pause_on_exceptions",
                mode("uncaught"),
                "uncaught exceptions",
            ),
            ("ISSUE-67 throw", "evaluate_js", trigger, ""),
            ("ISSUE-67 wait for the exception", "sleep", json!({}), ""),
            (
                "ISSUE-67 paused on it",
                "call_stack",
                on.clone(),
                "Paused (exception)",
            ),
            ("ISSUE-67 resume after it", "resume", on, "resumed"),
            (
                "ISSUE-67 stop pausing on exceptions",
                "pause_on_exceptions",
                mode("none"),
                "does not pause",
            ),
        ]
    }

    fn debugger_wording_steps(i: &Value) -> Vec<Step> {
        vec![
            (
                "ISSUE-66 run the outline",
                "snapshot",
                json!({"instance_id": i}),
                "[ref=e",
            ),
            (
                "ISSUE-66 search_scripts skips the server's snippets",
                "search_scripts",
                json!({"instance_id": i, "query": "aria-labelledby"}),
                "No matches found.",
            ),
            (
                "ISSUE-66 the debug report is JSON",
                "search_scripts",
                json!({"instance_id": i, "query": "debug"}),
                "\"source_len\":",
            ),
            (
                "ISSUE-66 resume when not paused",
                "resume",
                json!({"instance_id": i}),
                "The debugger was not paused: nothing to resume.",
            ),
            (
                "ISSUE-66 evaluate_on_call_frame when not paused",
                "evaluate_on_call_frame",
                json!({"instance_id": i, "expression": "1"}),
                "!the debugger is not paused",
            ),
        ]
    }

    /// ISSUE-63: a step or resume that stops at the same breakpoint again
    /// (the next iteration of a loop) reports the pause (Firefox answered
    /// `paused: false` after 2 s).
    fn loop_breakpoint_steps(i: &Value, base: &str) -> Vec<Step> {
        let bp = json!({"instance_id": i, "url": format!("{base}/app.js"), "line_number": 9});
        let trigger =
            json!({"instance_id": i, "expression": "setTimeout(() => appLoop(4), 50); 1"});
        vec![
            (
                "ISSUE-63 a breakpoint in a loop",
                "set_breakpoint",
                bp,
                "Breakpoint set",
            ),
            ("ISSUE-63 run the loop", "evaluate_js", trigger, ""),
            ("ISSUE-63 wait for the pause", "sleep", json!({}), ""),
            (
                "ISSUE-63 step_over in the loop stays paused",
                "step_over",
                json!({"instance_id": i}),
                "\"paused\": true",
            ),
            (
                "ISSUE-63 resume stops at the breakpoint again",
                "resume",
                json!({"instance_id": i}),
                "\"paused\": true",
            ),
            (
                "ISSUE-63 remove the breakpoint",
                "remove_breakpoint",
                json!({"instance_id": i, "breakpoint_id": "bp-4"}),
                "removed",
            ),
            (
                "ISSUE-63 resume to the end",
                "resume",
                json!({"instance_id": i}),
                "\"paused\": false",
            ),
        ]
    }

    /// Tools on the paused page answer at once (ISSUE-16, ISSUE-64), then
    /// a step and the resume.
    fn paused_page_steps(i: &Value) -> Vec<Step> {
        vec![
            (
                "ISSUE-16 evaluate_js while paused answers at once",
                "evaluate_js",
                json!({"instance_id": i, "expression": "Promise.resolve(6 * 7)"}),
                "42",
            ),
            (
                "ISSUE-64 snapshot while paused answers at once",
                "snapshot",
                json!({"instance_id": i}),
                "[ref=e",
            ),
            (
                "ISSUE-64 find_in_snapshot while paused",
                "find_in_snapshot",
                json!({"instance_id": i, "text": "hit"}),
                "button \"hit\"",
            ),
            (
                "step_over",
                "step_over",
                json!({"instance_id": i}),
                "Stepped",
            ),
            ("resume", "resume", json!({"instance_id": i}), "resumed"),
        ]
    }

    /// ISSUE-45: a page that pauses in the debugger while it loads makes
    /// navigate fail at once naming the pause (Chrome waited 30 s).
    fn pause_on_navigate_steps(i: &Value, base: &str) -> Vec<Step> {
        let at_load = json!({"instance_id": i, "url": format!("{base}/app.js"), "line_number": 4});
        vec![
            (
                "ISSUE-45 a breakpoint in a script run at load",
                "set_breakpoint",
                at_load,
                "Breakpoint set",
            ),
            (
                "ISSUE-45 navigate stops at the pause",
                "navigate",
                json!({"instance_id": i, "url": format!("{base}/")}),
                "!while running navigate",
            ),
            (
                "ISSUE-45 resume the load",
                "resume",
                json!({"instance_id": i}),
                "resumed",
            ),
            (
                "ISSUE-45 remove the breakpoint",
                "remove_breakpoint",
                json!({"instance_id": i, "breakpoint_id": "bp-3"}),
                "removed",
            ),
        ]
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

    /// ISSUE-65: a click whose handler hits the breakpoint after an `await`
    /// notes the pause too (it waited 2 s and said nothing).
    fn async_click_into_breakpoint_steps(i: &Value) -> Vec<Step> {
        let button = "const b = document.createElement('button'); b.id = 'hit65'; b.textContent = 'hit later'; b.onclick = async () => { await new Promise(r => setTimeout(r, 20)); appTarget(4); }; document.body.prepend(b); 1";
        let run = |expression: &str| json!({"instance_id": i, "expression": expression});
        vec![
            (
                "ISSUE-65 a handler that awaits",
                "evaluate_js",
                run(button),
                "",
            ),
            (
                "ISSUE-65 the click notes the pause after the await",
                "click_element",
                json!({"instance_id": i, "selector": "#hit65"}),
                "The page paused in the debugger at http",
            ),
            (
                "ISSUE-65 the paused frame",
                "evaluate_on_call_frame",
                json!({"instance_id": i, "expression": "x * 7"}),
                "28",
            ),
            (
                "ISSUE-65 resume",
                "resume",
                json!({"instance_id": i}),
                "resumed",
            ),
            (
                "ISSUE-65 remove the button",
                "evaluate_js",
                run("document.getElementById('hit65').remove(); 1"),
                "",
            ),
        ]
    }

    /// ISSUE-22 / ISSUE-53: insights explain the profile: Chrome lists the
    /// render-blocking requests, Firefox notes it cannot, and both report
    /// the DOM size and interaction timing.
    fn insights_steps(browser: &str) -> Vec<Step> {
        let args = json!({"instance_id": "e2e", "duration_ms": 500, "insights": true});
        let expected = if browser == "chrome" {
            "\"render_blocking\": ["
        } else {
            "render_blocking is null: this browser does not say"
        };
        vec![
            (
                "ISSUE-22 profile_page_performance insights",
                "profile_page_performance",
                args.clone(),
                expected,
            ),
            (
                "ISSUE-53 insights include dom_size and third_parties",
                "profile_page_performance",
                args,
                "\"dom_size\": {",
            ),
            (
                "ISSUE-54 a reload measures unused code (Chrome)",
                "profile_page_performance",
                json!({"instance_id": "e2e", "duration_ms": 1000, "action": "reload", "insights": true}),
                if browser == "chrome" {
                    "\"unused_js_bytes\":"
                } else {
                    "unused_code is null: Firefox has no code coverage"
                },
            ),
            (
                "ISSUE-56 save_path writes the trace on Chrome and fails on Firefox",
                "profile_page_performance",
                json!({
                    "instance_id": "e2e",
                    "duration_ms": 500,
                    "insights": true,
                    "save_path": std::env::temp_dir().join("e2e-trace-56.json.gz").display().to_string(),
                }),
                if browser == "chrome" {
                    "\"saved_trace\": {"
                } else {
                    "!save_path is only available on Chrome"
                },
            ),
        ]
    }

    /// ISSUE-71 (Firefox): a subscribed event the server also handles, and
    /// the event packet an RDP request starts, reach get_custom_events.
    fn firefox_custom_event_steps() -> Vec<Step> {
        let i = json!("e2e");
        let custom = |method: &str| json!({"instance_id": i, "filter_method": method});
        let subscribe = json!({"instance_id": i, "method": "session.subscribe", "params": "{\"events\": [\"browsingContext.load\"]}"});
        let evaluate = json!({"instance_id": i, "to": "console", "request_type": "evaluateJSAsync", "params": "{\"text\": \"6*7\"}"});
        vec![
            (
                "ISSUE-71 subscribe to load",
                "send_bidi_command",
                subscribe,
                "subscription",
            ),
            (
                "ISSUE-71 reload",
                "reload",
                json!({"instance_id": i}),
                "Page reloaded",
            ),
            (
                "ISSUE-71 the load is a custom event",
                "get_custom_events",
                custom("browsingContext.load"),
                "\"method\": \"browsingContext.load\"",
            ),
            (
                "ISSUE-71 an async RDP evaluation",
                "send_rdp_request",
                evaluate,
                "resultID",
            ),
            ("ISSUE-71 let its result come", "sleep", json!({}), ""),
            (
                "ISSUE-71 its evaluationResult is a custom event",
                "get_custom_events",
                custom("rdp.evaluationResult"),
                "\"method\": \"rdp.evaluationResult\"",
            ),
        ]
    }

    /// Firefox's profile steps: a reload, a cold reload bypassing the cache
    /// and a plain reload right after (ISSUE-58).
    fn firefox_profile_steps() -> Vec<Step> {
        let i = json!("e2e");
        vec![
            (
                "FPF-02 profile_page_performance",
                "profile_page_performance",
                json!({"instance_id": i, "duration_ms": 500, "action": "reload"}),
                "first_contentful_paint_ms",
            ),
            (
                "ISSUE-58 Firefox reload bypasses the cache",
                "profile_page_performance",
                json!({"instance_id": i, "duration_ms": 500, "action": "reload", "disable_cache": true}),
                "first_contentful_paint_ms",
            ),
            (
                "ISSUE-58 Firefox reload right after",
                "reload",
                json!({"instance_id": i}),
                "Page reloaded.",
            ),
        ]
    }

    /// Steps on instances of their own: the last tab closed (ISSUE-101),
    /// the page crashed (ISSUE-102), a Lighthouse-style report.
    fn own_instance_steps(browser: &str, base: &str) -> Vec<Step> {
        [
            last_tab_steps(browser, base),
            crash_steps(browser, base),
            lighthouse_steps(browser, base),
        ]
        .concat()
    }

    /// lighthouse_report scores a cold mobile load (performance from the
    /// trace on Chrome; none on Firefox, with a note), narrows to desktop
    /// SEO, and puts the tab's viewport back.
    fn lighthouse_steps(browser: &str, base: &str) -> Vec<Step> {
        let i = json!("lh");
        let open = json!({"browser": browser, "label": "lh", "headless": true});
        let url = format!("{base}/");
        let mobile = json!({"instance_id": i, "url": url});
        let desktop = json!({"instance_id": i, "device": "desktop", "categories": ["seo"]});
        let performance = match browser {
            "chrome" => "\"speed_index_ms\": {\n      \"points_lost\"",
            _ => "performance has no score",
        };
        let width = json!({"instance_id": i, "expression": "innerWidth > 800"});
        vec![
            ("LH an instance", "open_instance", open, "\"lh\""),
            (
                "LH a mobile report and its performance",
                "lighthouse_report",
                mobile,
                performance,
            ),
            (
                "LH desktop SEO of the page",
                "lighthouse_report",
                desktop,
                "(desktop, ",
            ),
            ("LH the viewport is put back", "evaluate_js", width, "true"),
            (
                "LH close the instance",
                "close_instance",
                json!({"instance_id": i}),
                "\"stopped\":true",
            ),
        ]
    }

    /// ISSUE-101: once an instance's last tab is closed, a tool says to
    /// open one, and open_tab does (it failed on Chrome).
    fn last_tab_steps(browser: &str, base: &str) -> Vec<Step> {
        let i = json!("last101");
        let open = json!({"browser": browser, "label": "last101", "headless": true});
        let url = format!("{base}/");
        vec![
            ("ISSUE-101 an instance", "open_instance", open, "last101"),
            (
                "ISSUE-101 its page",
                "navigate",
                json!({"instance_id": i, "url": url}),
                "Navigated to",
            ),
            (
                "ISSUE-101 close its last tab",
                "close_tab",
                json!({"instance_id": i}),
                "closed_tab_id",
            ),
            (
                "ISSUE-101 a tool says to open one",
                "evaluate_js",
                json!({"instance_id": i, "expression": "1"}),
                "!open one with open_tab",
            ),
            (
                "ISSUE-101 open_tab opens one",
                "open_tab",
                json!({"instance_id": i, "url": url}),
                "tab_id",
            ),
            (
                "ISSUE-101 it answers",
                "evaluate_js",
                json!({"instance_id": i, "expression": "1 + 1"}),
                "2",
            ),
            (
                "ISSUE-101 close the instance",
                "close_instance",
                json!({"instance_id": i}),
                "\"stopped\":true",
            ),
        ]
    }

    /// ISSUE-102 (Chrome): a tool on a crashed page fails at once saying
    /// so, and reload brings it back (Firefox lets no page crash on demand).
    fn crash_steps(browser: &str, base: &str) -> Vec<Step> {
        if browser != "chrome" {
            return Vec::new();
        }
        let i = json!("crash102");
        let open = json!({"browser": browser, "label": "crash102", "headless": true});
        let crash = json!({"instance_id": i, "method": "Page.crash", "params": "{}"});
        let eval = |expression: &str| json!({"instance_id": i, "expression": expression});
        vec![
            ("ISSUE-102 an instance", "open_instance", open, "crash102"),
            (
                "ISSUE-102 its page",
                "navigate",
                json!({"instance_id": i, "url": format!("{base}/")}),
                "Navigated to",
            ),
            ("ISSUE-102 crash it", "send_cdp_command", crash, "!crashed"),
            (
                "ISSUE-102 a tool says so at once",
                "evaluate_js",
                eval("1"),
                "!call reload, or navigate",
            ),
            (
                "ISSUE-102 reload brings it back",
                "reload",
                json!({"instance_id": i}),
                "Page reloaded",
            ),
            ("ISSUE-102 it answers", "evaluate_js", eval("1 + 1"), "2"),
            (
                "ISSUE-102 close the instance",
                "close_instance",
                json!({"instance_id": i}),
                "\"stopped\":true",
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
            let mut steps = firefox_profile_steps();
            steps.extend(firefox_custom_event_steps());
            steps.extend(insights_steps(browser));
            steps.push((
                "pause_on_load",
                "pause_on_load",
                json!({"instance_id": i}),
                "!is not available on",
            ));
            steps.push(close);
            return steps;
        }
        // ISSUE-92: nothing loads while tracing: the page's own timings
        let mut steps = vec![(
            "profile_page_performance",
            "profile_page_performance",
            profile,
            "nothing loaded while tracing",
        )];
        steps.extend(insights_steps(browser));
        steps.extend(pause_on_load_steps(&i));
        steps.extend([
            (
                "resume after load",
                "resume",
                json!({"instance_id": i}),
                "resumed",
            ),
            close,
        ]);
        steps
    }

    /// ISSUE-84: pause_on_load answers once the page paused, before its
    /// scripts ran, and the pause is named (Chrome only).
    fn pause_on_load_steps(i: &Value) -> Vec<Step> {
        vec![
            (
                "ISSUE-84 pause_on_load answers the pause",
                "pause_on_load",
                json!({"instance_id": i}),
                "paused before its first script runs",
            ),
            (
                "ISSUE-84 call_stack at once names it",
                "call_stack",
                json!({"instance_id": i}),
                "browser-control-mcp://pause_on_load",
            ),
            (
                "ISSUE-84 the page's scripts have not run",
                "evaluate_js",
                json!({"instance_id": i, "expression": "typeof appTarget"}),
                "undefined",
            ),
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
        mcp.list_tools(59).await;
        if browser == "chrome" {
            mcp.own_default_chrome().await;
        }
        let all = [
            page_steps(browser, &base),
            tab_steps("e2e", &base),
            foreground_steps("e2e"),
            runtime_steps("e2e"),
            dialog_and_wait_steps("e2e"),
            pierce_steps("e2e"),
            snapshot_steps("e2e"),
            table_steps("e2e"),
            settle_steps("e2e"),
            rich_editor_steps("e2e"),
            label_steps("e2e"),
            clickable_steps("e2e"),
            hover_reveal_steps("e2e"),
            element_shot_steps("e2e"),
            widget_steps("e2e"),
            form_steps("e2e"),
            secret_steps("e2e"),
            upload_steps("e2e"),
            pdf_steps("e2e"),
            video_steps("e2e", browser == "chrome"),
            key_and_pointer_steps("e2e"),
            key_combination_steps("e2e"),
            storage_steps("e2e"),
            emulate_steps("e2e", browser == "chrome"),
            closed_root_steps("e2e", browser == "chrome"),
            observation_steps("e2e"),
            memory_steps("e2e"),
            network_steps("e2e"),
            worker_steps("e2e"),
            request_detail_steps("e2e"),
            websocket_steps("e2e", &base),
            other_page_steps("e2e", &base),
            form_issue_steps("e2e", &base, browser == "chrome"),
            debugger_steps("e2e", &base, browser == "chrome"),
            own_instance_steps(browser, &base),
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

    /// The attach suite's steps on the default instance (`chrome`: the
    /// attached browser is Chrome).
    fn attach_steps(base: &str, chrome: bool) -> Vec<Step> {
        [
            tab_steps("default", base),
            runtime_steps("default"),
            dialog_and_wait_steps("default"),
            pierce_steps("default"),
            snapshot_steps("default"),
            table_steps("default"),
            settle_steps("default"),
            rich_editor_steps("default"),
            label_steps("default"),
            clickable_steps("default"),
            hover_reveal_steps("default"),
            element_shot_steps("default"),
            widget_steps("default"),
            form_steps("default"),
            upload_steps("default"),
            pdf_steps("default"),
            video_steps("default", chrome),
            key_and_pointer_steps("default"),
            key_combination_steps("default"),
            storage_steps("default"),
            emulate_steps("default", chrome),
            closed_root_steps("default", chrome),
            network_steps("default"),
            worker_steps("default"),
            request_detail_steps("default"),
            other_page_steps("default", base),
            form_issue_steps("default", base, chrome),
            debugger_steps("default", base, chrome),
        ]
        .concat()
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
        mcp.attached_steps(attach_steps(&base, variable.contains("CHROME")))
            .await;
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
