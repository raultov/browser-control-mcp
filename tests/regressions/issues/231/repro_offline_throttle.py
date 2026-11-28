"""set_offline {offline:false} drops emulate's network_throttling (Chrome),
and emulate {network_throttling} brings an offline tab back online."""
import json, os, sys, threading
from http.server import ThreadingHTTPServer, SimpleHTTPRequestHandler

sys.path.insert(0, "tests/compare")
from mcp_client import Server


class H(SimpleHTTPRequestHandler):
    def do_GET(self):
        body = b"<p>x</p>" if self.path.startswith("/index") else b"ok"
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

PROBE = ("const t=performance.now(); const ok = await fetch('/tiny?'+Math.random(),{cache:'no-store'})"
         ".then(()=>true,()=>false); ({ok, ms: Math.round(performance.now()-t), online: navigator.onLine})")
s = Server(["target/release/browser-control-mcp", "--headless", "--tool-groups", "all"])
bad = []
try:
    s.call("navigate", {"url": f"http://127.0.0.1:{port}/index.html"})

    def probe():
        return json.loads(s.call("evaluate_js", {"expression": PROBE}, timeout=30).text)["value"]

    s.call("emulate", {"network_throttling": "slow-3g"})
    a1 = probe()
    s.call("set_offline", {"offline": True})
    s.call("set_offline", {"offline": False})
    a3 = probe()
    rep = s.call("emulate", {"locale": "fr-FR"}).text
    print("throttled:", a1, "after offline on/off:", a3)
    if a1["ms"] >= 1500 and a3["ms"] < 500:
        bad.append("THROTTLING_LOST")
        if '"network_throttling": "slow-3g"' in rep:
            bad.append("EMULATE_STILL_REPORTS_SLOW3G")
    s.call("emulate", {"reset": True})
    s.call("set_offline", {"offline": True})
    s.call("emulate", {"network_throttling": "4g"})
    b3 = probe()
    print("offline then emulate 4g:", b3)
    if b3["ok"] and b3["online"]:
        bad.append("OFFLINE_LOST")
finally:
    s.close()
print("BUG " + " ".join(bad) if bad else "OK")
sys.exit(1 if bad else 0)
