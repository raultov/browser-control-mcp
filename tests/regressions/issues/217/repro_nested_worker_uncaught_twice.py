import os, sys, time, threading
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
sys.path.insert(0, os.path.join(os.getcwd(), "tests", "compare"))
from mcp_client import Server

FILES = {
    "/p.html": ("text/html", "<script>new Worker('outer.js')</script>"),
    "/outer.js": ("text/javascript", "new Worker('/inner.js')"),
    "/inner.js": ("text/javascript", "throw new Error('boom')"),
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
browser = sys.argv[1] if len(sys.argv) > 1 else "chrome"
s = Server(["target/release/browser-control-mcp", "--headless", "--browser", browser])
try:
    s.call("navigate", {"url": f"http://127.0.0.1:{srv.server_address[1]}/p.html"})
    time.sleep(2)
    text = s.call("get_console_logs").text
finally:
    s.close()
lines = [l for l in text.splitlines() if "Uncaught" in l]
print(f"{browser}: {len(lines)} 'Uncaught' line(s) for one error thrown: {lines}")
sys.exit(1 if len(lines) != 1 else 0)
