import os, sys, threading
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
sys.path.insert(0, os.path.join(os.getcwd(), "tests", "compare"))
from mcp_client import Server

PAGE = b"<button id=b onclick=\"throw 'str-boom'\">Throw string</button>"


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
s = Server(["target/release/browser-control-mcp", "--headless"])
try:
    s.call("navigate", {"url": f"http://127.0.0.1:{srv.server_address[1]}/"})
    click = s.call("click_element", {"selector": "#b"}).text
    logs = s.call("get_console_logs").text
finally:
    s.close()
print("click_element:", click.strip().replace("\n", " | "))
print("get_console_logs:", logs.strip().replace("\n", " | "))
missing = "str-boom" not in click and "str-boom" not in logs
print("thrown value missing from both replies:", missing)
sys.exit(1 if missing else 0)
