import json, sys, threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
sys.path.insert(0, 'tests/compare')
from mcp_client import Server

PAGE = b"""<!doctype html><html><body>
<p>htmlOnlyNeedle in plain text</p>
<script>var a = 'inlineNeedle';</script>
<script>var b = 2;</script>
</body></html>
"""


class H(BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200)
        self.send_header('Content-Type', 'text/html')
        self.send_header('Content-Length', str(len(PAGE)))
        self.end_headers()
        self.wfile.write(PAGE)

    def log_message(self, *a):
        pass


srv = ThreadingHTTPServer(('127.0.0.1', 0), H)
threading.Thread(target=srv.serve_forever, daemon=True).start()
s = Server(['target/release/browser-control-mcp', '--headless', '--browser', 'firefox', '--tool-groups', 'debug'])
s.call('navigate', {'url': f'http://127.0.0.1:{srv.server_address[1]}/page.html'})
html = s.call('search_scripts', {'query': 'htmlOnlyNeedle'}).text
inline = s.call('search_scripts', {'query': 'inlineNeedle'}).text
s.close()
srv.shutdown()
print('htmlOnlyNeedle ->', html)
print('inlineNeedle ->', inline)


def count(text):
    try:
        return len(json.loads(text))
    except ValueError:
        return 0


bad = False
if count(html):
    print(f'WRONG: text outside any script matched {count(html)} time(s)')
    bad = True
if count(inline) != 1:
    print(f'WRONG: one match in one inline script reported {count(inline)} time(s)')
    bad = True
sys.exit(1 if bad else 0)
