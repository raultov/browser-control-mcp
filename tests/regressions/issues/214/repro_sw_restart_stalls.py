import os, sys, json, time, threading
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
sys.path.insert(0, os.path.join(os.getcwd(), "tests", "compare"))
from mcp_client import Server

SW = """self.addEventListener('install', e => self.skipWaiting());
self.addEventListener('activate', e => e.waitUntil(clients.claim()));
self.addEventListener('fetch', e => {
  if (new URL(e.request.url).pathname === '/via-sw.txt') e.respondWith(new Response('from-sw'));
});"""
FILES = {
    "/sw.html": ("text/html", "<script>navigator.serviceWorker.register('/sw.js')</script>"),
    "/sw.js": ("text/javascript", SW),
}


class H(SimpleHTTPRequestHandler):
    def do_GET(self):
        ctype, body = FILES.get(self.path, ("text/plain", ""))
        data = body.encode()
        self.send_response(200 if self.path in FILES else 404)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def log_message(self, *a):
        pass


srv = ThreadingHTTPServer(("127.0.0.1", 0), H)
threading.Thread(target=srv.serve_forever, daemon=True).start()
FETCH = ("await Promise.race([fetch('/via-sw.txt').then(r => r.text()),"
         " new Promise(r => setTimeout(() => r('TIMEOUT'), 5000))])")
s = Server(["target/release/browser-control-mcp", "--headless", "--tool-groups", "raw"])


def ev(expr):
    return json.loads(s.call("evaluate_js", {"expression": expr}).text).get("value")


try:
    s.call("navigate", {"url": f"http://127.0.0.1:{srv.server_address[1]}/sw.html"})
    ev("await navigator.serviceWorker.ready; 1")
    s.call("reload", {})
    time.sleep(0.5)
    before = ev(FETCH)
    s.call("send_cdp_command", {"method": "ServiceWorker.enable"})
    s.call("send_cdp_command", {"method": "ServiceWorker.stopAllWorkers"})
    time.sleep(0.5)
    after = ev(FETCH)
finally:
    s.close()
print(f"before the worker restart: {before}; after: {after}")
sys.exit(1 if before == "from-sw" and after == "TIMEOUT" else 0)
