import os, sys, json, threading
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
sys.path.insert(0, os.path.join(os.getcwd(), "tests", "compare"))
from mcp_client import Server

PAGE = (b"<textarea id=ta></textarea><script>window.keys=[];"
        b"ta.addEventListener('keydown',e=>keys.push(e.key))</script>")


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
url = f"http://127.0.0.1:{srv.server_address[1]}/"
keys = {}
for browser in ("chrome", "firefox"):
    s = Server(["target/release/browser-control-mcp", "--headless", "--browser", browser])
    try:
        s.call("navigate", {"url": url})
        s.call("fill_input", {"selector": "#ta", "text": "a\tb"})
        r = s.call("evaluate_js", {"expression": "JSON.stringify({value: ta.value, keys})"})
        keys[browser] = json.loads(json.loads(r.text)["value"])
    finally:
        s.close()
    print(browser, json.dumps(keys[browser]))
same = keys["chrome"]["keys"] == keys["firefox"]["keys"]
print("keydown keys identical:", same)
sys.exit(0 if same else 1)
