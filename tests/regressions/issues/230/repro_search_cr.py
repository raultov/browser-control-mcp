"""search_scripts gives line 0 for a match in a script whose lines end with a
lone CR; the debugger pauses there only at line 2."""
import json, re, sys, threading, time
from http.server import ThreadingHTTPServer, BaseHTTPRequestHandler

sys.path.insert(0, "tests/compare")
from mcp_client import Server

CR_JS = "\r".join(['var s = "x y";', "function needleCR() {", "  return 22;", "}",
                   "setInterval(needleCR, 200);", ""]).encode()


class H(BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path.startswith("/cr.js"):
            body, kind = CR_JS, "text/javascript"
        else:
            body, kind = b"<script src=/cr.js></script>", "text/html"
        self.send_response(200)
        self.send_header("Content-Type", kind)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *a):
        pass


httpd = ThreadingHTTPServer(("127.0.0.1", 0), H)
threading.Thread(target=httpd.serve_forever, daemon=True).start()


def paused_at(s, line):
    bp = s.call("set_breakpoint", {"url": "cr.js", "line_number": line})
    bid = re.search(r'"breakpoint_id":\s*"([^"]+)"', bp.text).group(1)
    where = None
    t0 = time.time()
    while time.time() - t0 < 3 and where is None:
        cs = s.call("call_stack", {})
        if "function_name" in cs.text:
            frame = json.loads(cs.text[cs.text.find("{"):cs.text.rfind("}") + 1])["frames"][0]
            where = (frame["function_name"], frame["line_number"])
        time.sleep(0.2)
    s.call("remove_breakpoint", {"breakpoint_id": bid})
    s.call("resume", {})
    return where


bad = []
for browser in ("chrome", "firefox"):
    s = Server(["target/release/browser-control-mcp", "--headless", "--tool-groups", "debug", "--browser", browser])
    try:
        s.call("navigate", {"url": f"http://127.0.0.1:{httpd.server_address[1]}/"})
        time.sleep(1)
        found = json.loads(s.call("search_scripts", {"query": "return 22"}).text)
        line = found[0]["lineNumber"]
        at_found = paused_at(s, line)
        at_two = paused_at(s, 2)
    finally:
        s.close()
    print(f"{browser}: search_scripts line {line}; breakpoint there pauses at {at_found}; at line 2 pauses at {at_two}")
    if at_two and at_two[1] != line:
        bad.append(browser)
print("SEARCH LINE WRONG on: " + ", ".join(bad) if bad else "OK")
sys.exit(1 if bad else 0)
