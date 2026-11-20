import sys, os, json, tempfile, threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
sys.path.insert(0, 'tests/compare')
from mcp_client import Server

BODY = b'{"php":"ab\\/cd&ef7731","go":"ab/cd\\u0026ef7731"}'

class H(BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def do_GET(self):
        body = BODY if self.path == '/qa.json' else b'<!doctype html><title>t</title><p>x</p>'
        self.send_response(200)
        self.send_header('Content-Type', 'application/json' if self.path == '/qa.json' else 'text/html')
        self.send_header('Content-Length', str(len(body)))
        self.end_headers(); self.wfile.write(body)

httpd = ThreadingHTTPServer(('127.0.0.1', 0), H)
threading.Thread(target=httpd.serve_forever, daemon=True).start()
base = f'http://127.0.0.1:{httpd.server_address[1]}'
E = os.path.join(tempfile.mkdtemp(), 'secrets.env')
open(E, 'w').write('QA_KEY=ab/cd&ef7731\n')
s = Server(['target/release/browser-control-mcp', '--headless', '--secrets', E], timeout=90)
s.call('navigate', {'url': base + '/p.html'})
r = s.call('evaluate_js', {'expression': "fetch('/qa.json').then(r=>r.text())"})
s.close()
print(r.text)
leak = 'ef7731' in r.text
print('LEAK: secret QA_KEY visible in escaped JSON form' if leak else 'masked')
sys.exit(1 if leak else 0)
