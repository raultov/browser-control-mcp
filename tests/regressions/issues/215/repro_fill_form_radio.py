import os, sys, json, threading
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
sys.path.insert(0, os.path.join(os.getcwd(), "tests", "compare"))
from mcp_client import Server

PAGE = b"<input type=radio name=r id=r1 checked><input type=radio name=r id=r2>"


class H(SimpleHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200)
        self.send_header("Content-Type", "text/html")
        self.send_header("Content-Length", str(len(PAGE)))
        self.end_headers()
        self.wfile.write(PAGE)

    def log_message(self, *a):
        pass


srv = ThreadingHTTPServer(("127.0.0.1", 0), H)
threading.Thread(target=srv.serve_forever, daemon=True).start()
browser = sys.argv[1] if len(sys.argv) > 1 else "chrome"
s = Server(["target/release/browser-control-mcp", "--headless", "--browser", browser])
try:
    s.call("navigate", {"url": f"http://127.0.0.1:{srv.server_address[1]}/form.html"})
    r = s.call("fill_form", {"fields": [{"selector": "#r1", "value": "false"}]})
    outcome = json.loads(r.text[r.text.index("{"):r.text.rindex("}") + 1])["fields"][0]["outcome"]
    page = json.loads(s.call("evaluate_js", {"expression": "r1.checked"}).text)["value"]
    print(f"fill_form outcome={outcome} r1.checked={page}")
    sys.exit(1 if outcome == "unchecked" and page is True else 0)
finally:
    s.close()
