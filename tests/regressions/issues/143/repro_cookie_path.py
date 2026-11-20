import os, sys, json, threading
sys.path.insert(0, os.path.join(os.getcwd(), 'tests/compare'))
from mcp_client import Server
from http.server import ThreadingHTTPServer, BaseHTTPRequestHandler
class H(BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'
    def log_message(self, *a): pass
    def do_GET(self):
        if self.path.split('?')[0].endswith('/echo-cookies'):
            body, ctype = (self.headers.get('Cookie') or '').encode(), 'text/plain'
        else:
            body, ctype = b'<!doctype html><title>c</title><p>cookie page</p>', 'text/html'
        self.send_response(200); self.send_header('Content-Type', ctype)
        self.send_header('Cache-Control', 'no-store'); self.send_header('Content-Length', str(len(body)))
        self.end_headers(); self.wfile.write(body)
httpd = ThreadingHTTPServer(('127.0.0.1', 0), H); port = httpd.server_address[1]
threading.Thread(target=httpd.serve_forever, daemon=True).start()
def listed(r):
    return sorted(c['name'] for c in json.loads(r.text[r.text.index('{'):])['cookies'] if c['name'].startswith('qa_'))
def sent(r):
    return sorted(n for n in ('qa_path', 'qa_root') if n + '=' in r.text)
bad = 0
def check(browser, label, ok, detail):
    global bad
    bad += not ok
    print(browser, 'OK ' if ok else 'BAD', label, '->', detail)
for browser in ('firefox',):
    s = Server(['target/release/browser-control-mcp', '--headless', '--browser', browser, '--tool-groups', 'storage'], timeout=120)
    try:
        base = f'http://127.0.0.1:{port}'
        s.call('navigate', {'url': base + '/app/page'})
        s.call('evaluate_js', {'expression': "document.cookie='qa_path=1; path=/app'; document.cookie='qa_root=1; path=/'; document.cookie"})
        s.call('navigate', {'url': base + '/application/page'})
        a_sent = sent(s.call('evaluate_js', {'expression': "(await fetch('/application/echo-cookies')).text()"}))
        check(browser, 'server receives only qa_root', a_sent == ['qa_root'], a_sent)
        g = s.call('cookies', {'action': 'get'})
        a_listed = listed(g)
        check(browser, 'get lists only qa_root', (not g.is_error) and a_listed == ['qa_root'], a_listed)
        d = s.call('cookies', {'action': 'delete', 'name': 'qa_path'})
        check(browser, 'delete qa_path matches nothing', 'No cookie matched' in d.text, d.short(200))
        s.call('navigate', {'url': base + '/app/page'})
        c = s.call('evaluate_js', {'expression': 'document.cookie'})
        check(browser, 'qa_path survives at /app/page', 'qa_path=1' in c.text, c.short(200))
    finally:
        s.close()
httpd.shutdown()
print('BAD_COUNT', bad)
sys.exit(1 if bad else 0)
