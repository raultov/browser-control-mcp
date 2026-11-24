import os, sys, threading, time
sys.path.insert(0, os.path.join(os.getcwd(), 'tests/compare'))
from mcp_client import Server
from http.server import ThreadingHTTPServer, BaseHTTPRequestHandler
PAGE = b"<!doctype html><title>c</title>\n<script>\nfunction boom() { throw new Error('qa-uncaught') }\nsetTimeout(boom, 0);\n</script>\n"
class H(BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def do_GET(self):
        self.send_response(200); self.send_header('Content-Type', 'text/html')
        self.send_header('Content-Length', str(len(PAGE))); self.end_headers(); self.wfile.write(PAGE)
httpd = ThreadingHTTPServer(('127.0.0.1', 0), H)
threading.Thread(target=httpd.serve_forever, daemon=True).start()
base = 'http://127.0.0.1:%d' % httpd.server_address[1]
bad = []
for browser in ('chrome', 'firefox'):
    s = Server(['target/release/browser-control-mcp', '--headless', '--browser', browser], timeout=60)
    try:
        s.call('navigate', {'url': base + '/index.html'})
        time.sleep(1)
        lines = s.call('get_console_logs', {}).text.splitlines()
    finally:
        s.close()
    print(browser, lines)
    at = [i for i, l in enumerate(lines) if 'qa-uncaught' in l]
    stacked = bool(at) and at[0] + 1 < len(lines) and lines[at[0] + 1].startswith('    ')
    if not stacked:
        bad.append(browser)
if bad:
    print('BUG: no indented stack line after the uncaught error on', bad)
    sys.exit(1)
print('stack lines on both browsers')
