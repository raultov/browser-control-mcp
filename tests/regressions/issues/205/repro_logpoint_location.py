import os, sys, json, threading, time
sys.path.insert(0, os.path.join(os.getcwd(), 'tests/compare'))
from mcp_client import Server
from http.server import ThreadingHTTPServer, BaseHTTPRequestHandler
PAGES = {
    '/index.html': ('text/html', '<!doctype html><title>lp</title><script src="/app2.js"></script>'),
    '/app2.js': ('text/javascript', 'function tick(i) {\n  const sq = i * i;\n  return sq;\n}\nfunction loop() { for (let i = 0; i < 3; i++) tick(i); }\n'),
}
class H(BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def do_GET(self):
        ctype, body = PAGES.get(self.path, ('text/plain', ''))
        body = body.encode()
        self.send_response(200); self.send_header('Content-Type', ctype)
        self.send_header('Content-Length', str(len(body))); self.end_headers(); self.wfile.write(body)
httpd = ThreadingHTTPServer(('127.0.0.1', 0), H)
threading.Thread(target=httpd.serve_forever, daemon=True).start()
base = 'http://127.0.0.1:%d' % httpd.server_address[1]
lines = {}
for browser in ('chrome', 'firefox'):
    s = Server(['target/release/browser-control-mcp', '--headless', '--browser', browser, '--tool-groups', 'debug'], timeout=60)
    try:
        s.call('navigate', {'url': base + '/index.html'})
        s.call('set_breakpoint', {'url': 'app2.js', 'line_number': 1, 'log': '"i=" + i'})
        s.call('evaluate_js', {'expression': 'loop(); 7'})
        found = []
        for _ in range(15):
            found = [l.replace(base, 'BASE') for l in s.call('get_console_logs', {}).text.splitlines() if 'logpoint ' in l]
            if len(found) >= 3:
                break
            time.sleep(0.2)
        lines[browser] = found
        print(browser, found)
    finally:
        s.close()
if lines.get('chrome') != lines.get('firefox'):
    print('BUG: the logpoint console lines differ between Chrome and Firefox')
    sys.exit(1)
print('same on both browsers')
