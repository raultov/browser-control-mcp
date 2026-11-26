import os, sys, json, time, threading
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
sys.path.insert(0, os.path.join(os.getcwd(), "tests", "compare"))
from mcp_client import Server

# 100 setTimeout callbacks 3 ms apart, each throwing Error('burst').
LOOP = "for(let k=0;k<100;k++)setTimeout(()=>{throw new Error('burst')},k*3);"
WHERE = sys.argv[1] if len(sys.argv) > 1 else "page"
FILES = {
    "/p.html": ("text/html", "<script>new Worker('w.js')</script>" if WHERE == "worker"
                else "<script>" + LOOP + "</script>"),
    "/w.js": ("text/javascript", LOOP),
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
s = Server(["target/release/browser-control-mcp", "--headless", "--browser", "firefox"])
try:
    s.call("navigate", {"url": f"http://127.0.0.1:{srv.server_address[1]}/p.html"})
    time.sleep(4)
    r = s.call("get_console_logs", {"include_details": True, "limit": 1000})
finally:
    s.close()
details = json.loads(r.text[r.text.index("{"):])
stamps = [m["timestamp"] for m in details["messages"] if "Error: burst" in m["text"]]
backwards = sum(1 for a, b in zip(stamps, stamps[1:]) if b < a)
print(f"{WHERE}: {len(stamps)} 'burst' errors listed (100 thrown), {backwards} listed before an older one")
sys.exit(1 if len(stamps) > 100 or backwards else 0)
