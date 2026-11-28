"""evaluate_js user_gesture: true on Chrome (--headless): the page has user
activation, but requestFullscreen() and clipboard.writeText() are refused."""
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
threading.Thread(target=httpd.serve_forever, daemon=True).start()

CALLS = {
    "activation": "navigator.userActivation.isActive",
    "fullscreen": "await document.documentElement.requestFullscreen().then(()=>'ok', e=>e.name+': '+e.message)",
    "clipboard": "await navigator.clipboard.writeText('qa-clip').then(()=>'ok', e=>e.name+': '+e.message)",
}
s = Server(["target/release/browser-control-mcp", "--headless"])
out = {}
try:
    s.call("navigate", {"url": f"http://127.0.0.1:{httpd.server_address[1]}/"})
    for name, code in CALLS.items():
        r = s.call("evaluate_js", {"expression": code, "user_gesture": True}, timeout=20)
        out[name] = json.loads(r.text)["value"] if not r.is_error else r.text.strip()
        print(f"{name}: {out[name]!r}")
finally:
    s.close()
refused = [k for k in ("fullscreen", "clipboard") if out.get(k) != "ok"]
if out.get("activation") is True and refused:
    print("REFUSED WITH user_gesture: " + ", ".join(refused))
    sys.exit(1)
print("OK")
