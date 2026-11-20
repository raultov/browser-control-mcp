import sys, threading
sys.path.insert(0, 'tests/compare')
from http.server import ThreadingHTTPServer, BaseHTTPRequestHandler
from mcp_client import Server

class H(BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'
    def do_GET(self):
        b = b'{"ok":1}' if self.path.startswith('/api/h') else b'<!doctype html><title>h</title>'
        self.send_response(200)
        self.send_header('Content-Type', 'application/json' if self.path.startswith('/api') else 'text/html')
        self.send_header('Content-Length', str(len(b)))
        self.end_headers(); self.wfile.write(b)
    def log_message(self, *a): pass

httpd = ThreadingHTTPServer(('127.0.0.1', 0), H)
threading.Thread(target=httpd.serve_forever, daemon=True).start()
base = 'http://127.0.0.1:%d' % httpd.server_address[1]
s = Server(['target/release/browser-control-mcp', '--headless', '--browser', 'chrome'], timeout=120)
leaks = []
try:
    s.call('navigate', {'url': base + '/'})
    r = s.call('evaluate_js', {'expression': "fetch('/api/h',{headers:{'X-Authorization':'Bearer QAHDR7xauth33','X-Forwarded-Authorization':'Bearer QAHDR8fwd44','Authentication':'QAHDR9authn55','X-Auth-Token':'QAHDRAtok66'}}).then(r=>r.status)"})
    print('fetch', r.short(80))
    a = s.call('get_network_logs', {'include_details': True, 'url_filter': '/api/h'})
    for line in a.text.splitlines():
        if 'Auth' in line:
            print(line.strip())
    for v in ('QAHDR7xauth33', 'QAHDR8fwd44', 'QAHDR9authn55', 'QAHDRAtok66'):
        if v in a.text:
            leaks.append(v)
finally:
    s.close()
print('LEAKED', leaks)
sys.exit(1 if leaks else 0)
