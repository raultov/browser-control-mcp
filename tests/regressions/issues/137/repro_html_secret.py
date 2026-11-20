import os, sys, tempfile, shutil, threading
sys.path.insert(0, os.path.join(os.getcwd(), 'tests/compare'))
from mcp_client import Server
from http.server import ThreadingHTTPServer, BaseHTTPRequestHandler

class H(BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def do_GET(self):
        body = b'<title>h</title><p id=s>ab&amp;c&lt;d&gt;e8841</p>'
        self.send_response(200); self.send_header('Content-Type', 'text/html')
        self.send_header('Content-Length', str(len(body))); self.end_headers(); self.wfile.write(body)

srv = ThreadingHTTPServer(('127.0.0.1', 0), H)
threading.Thread(target=srv.serve_forever, daemon=True).start()
B = 'http://127.0.0.1:%d' % srv.server_port
d = tempfile.mkdtemp(prefix='repro-secrets-', dir=os.getcwd())
sec = os.path.join(d, 's.env')
open(sec, 'w').write("QA_H='ab&c<d>e8841'\n")
s = Server(['target/release/browser-control-mcp', '--headless', '--secrets', sec], timeout=120)
out = []
try:
    s.call('navigate', {'url': B + '/'})
    out.append(s.call('evaluate_js', {'expression': "document.getElementById('s').textContent"}))
    out.append(s.call('inspect_dom', {}))
    out.append(s.call('evaluate_js', {'expression': "document.getElementById('s').innerHTML"}))
finally:
    s.close(); shutil.rmtree(d, ignore_errors=True)
for i, r in enumerate(out):
    print(i, r.name, r.is_error, r.short(300))
if '8841' in out[0].text:
    print('MASKING_INACTIVE'); sys.exit(2)
leak = [i for i, r in enumerate(out) if '8841' in r.text]
print('LEAKS', leak)
sys.exit(1 if leak else 0)
