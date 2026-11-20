import sys, os, tempfile, threading
sys.path.insert(0, 'tests/compare')
from http.server import ThreadingHTTPServer, BaseHTTPRequestHandler
from mcp_client import Server

PAGE = '<!doctype html><title>j</title><div id="c" data-config=\'{"token":"Qa\\"Pw&amp;rd7731"}\'>cfg</div>'.encode()

class H(BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200)
        self.send_header('Content-Type', 'text/html')
        self.send_header('Content-Length', str(len(PAGE)))
        self.end_headers(); self.wfile.write(PAGE)
    def log_message(self, *a): pass

httpd = ThreadingHTTPServer(('127.0.0.1', 0), H)
threading.Thread(target=httpd.serve_forever, daemon=True).start()
base = 'http://127.0.0.1:%d' % httpd.server_address[1]
env = os.path.join(tempfile.mkdtemp(), 'secrets.env')
open(env, 'w', encoding='utf-8').write("QA_J='Qa\"Pw&rd7731'\n")
s = Server(['target/release/browser-control-mcp', '--headless', '--browser', 'chrome', '--secrets', env], timeout=120)
leaks = []
try:
    s.call('navigate', {'url': base + '/'})
    for name, tool, args in (('dataset', 'evaluate_js', {'expression': "document.getElementById('c').dataset.config"}),
                             ('outerHTML', 'evaluate_js', {'expression': "document.getElementById('c').outerHTML"}),
                             ('inspect_dom', 'inspect_dom', {})):
        r = s.call(tool, args)
        leaked = '7731' in r.text
        print(name, 'LEAK' if leaked else 'masked', r.short(200))
        if leaked:
            leaks.append(name)
finally:
    s.close()
print('LEAKED', leaks)
sys.exit(1 if leaks else 0)
