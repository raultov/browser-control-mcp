"""emulate geolocation, then navigate to another origin in the same tab:
Chrome still reads the position; Firefox's page never gets an answer."""
import json, sys, threading
from http.server import ThreadingHTTPServer, BaseHTTPRequestHandler

sys.path.insert(0, "tests/compare")
from mcp_client import Server


class H(BaseHTTPRequestHandler):
    def do_GET(self):
        body = b"<p>x</p>"
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

GEO = ("await new Promise(r => navigator.geolocation.getCurrentPosition("
       "p => r([p.coords.latitude, p.coords.longitude]), e => r('err:'+e.code), {timeout: 3000}))")
seen = {}
for browser in ("chrome", "firefox"):
    s = Server(["target/release/browser-control-mcp", "--headless", "--tool-groups", "emulation",
                "--browser", browser])
    try:
        s.call("navigate", {"url": f"http://127.0.0.1:{port}/a.html"})
        s.call("emulate", {"geolocation": {"latitude": 10, "longitude": 20}})
        s.call("navigate", {"url": f"http://localhost:{port}/b.html"})
        r = s.call("evaluate_js", {"expression": GEO}, timeout=20)
        try:
            seen[browser] = json.loads(r.text)["value"]
        except ValueError:
            seen[browser] = r.text.strip()[:80]
    finally:
        s.close()
    print(browser, "position on the second origin:", seen[browser])
if seen["chrome"] != seen["firefox"]:
    print("BROWSERS DIFFER")
    sys.exit(1)
print("SAME")
