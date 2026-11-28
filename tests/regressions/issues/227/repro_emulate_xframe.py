"""emulate user_agent/timezone/locale: does a cross-site iframe see them?
Chrome: no; Firefox: yes. Exits 1 when the browsers differ."""
import json, sys, threading, time
from http.server import ThreadingHTTPServer, BaseHTTPRequestHandler

sys.path.insert(0, "tests/compare")
from mcp_client import Server

FRAME = (b"<script>onmessage = e => e.ports[0].postMessage({ua: navigator.userAgent,"
         b" tz: Intl.DateTimeFormat().resolvedOptions().timeZone,"
         b" loc: Intl.DateTimeFormat().resolvedOptions().locale})</script>")


class H(BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path.startswith("/frame"):
            body = FRAME
        else:
            body = f"<iframe id=cross src=http://localhost:{port}/frame.html></iframe>".encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/html")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *a):
        pass


httpd = ThreadingHTTPServer(("127.0.0.1", 0), H)
port = httpd.server_address[1]
threading.Thread(target=httpd.serve_forever, daemon=True).start()

ASK = ("const c=new MessageChannel(); const p=new Promise(r=>c.port1.onmessage=e=>r(e.data));"
       " cross.contentWindow.postMessage(0,'*',[c.port2]); await p")
seen = {}
for browser in ("chrome", "firefox"):
    s = Server(["target/release/browser-control-mcp", "--headless", "--tool-groups", "emulation",
                "--browser", browser])
    try:
        s.call("navigate", {"url": f"http://127.0.0.1:{port}/index.html"})
        time.sleep(1)
        s.call("emulate", {"user_agent": "QA-UA/1.0", "timezone": "Asia/Tokyo", "locale": "fr-FR"})
        v = json.loads(s.call("evaluate_js", {"expression": ASK}, timeout=30).text)["value"]
    finally:
        s.close()
    seen[browser] = {"ua": v["ua"] == "QA-UA/1.0", "tz": v["tz"] == "Asia/Tokyo", "loc": v["loc"].startswith("fr")}
    print(browser, "cross-site iframe sees:", v)
diff = [k for k in seen["chrome"] if seen["chrome"][k] != seen["firefox"][k]]
print("emulated in the cross-site iframe:", seen)
if diff:
    print("BROWSERS DIFFER on: " + ", ".join(diff))
    sys.exit(1)
print("SAME")
