import os, sys, threading
sys.path.insert(0, os.path.join(os.getcwd(), 'tests/compare'))
from mcp_client import Server
from http.server import ThreadingHTTPServer, BaseHTTPRequestHandler
class H(BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'
    def log_message(self, *a): pass
    def do_GET(self):
        body = b'{"ok":1}' if self.path.startswith('/api/h') else b'<!doctype html><title>h</title><p>hdr</p>'
        self.send_response(200); self.send_header('Content-Type', 'application/json' if self.path.startswith('/api/h') else 'text/html')
        self.send_header('Cache-Control', 'no-store'); self.send_header('Content-Length', str(len(body)))
        self.end_headers(); self.wfile.write(body)
httpd = ThreadingHTTPServer(('127.0.0.1', 0), H); port = httpd.server_address[1]
threading.Thread(target=httpd.serve_forever, daemon=True).start()
VALUES = {'X-Auth-Key': 'QAHDR1authkey77', 'Ocp-Apim-Subscription-Key': 'QAHDR2apim88', 'X-API_KEY': 'QAHDR3under99',
          'Authorization': 'Bearer QAHDR0bearer11', 'X-Api-Key': 'QAHDR9apikey22'}
expr = "(await fetch('/api/h', {headers: " + repr(VALUES).replace("'", '"') + "})).status"
bad = 0
for browser in ('chrome',):
    s = Server(['target/release/browser-control-mcp', '--headless', '--browser', browser], timeout=120)
    try:
        s.call('navigate', {'url': f'http://127.0.0.1:{port}/hdr.html'})
        print(browser, 'fetch', s.call('evaluate_js', {'expression': expr}).short(100))
        r = s.call('get_network_logs', {'include_details': True, 'url_filter': '/api/h'})
        logged = '/api/h' in r.text and '<redacted>' in r.text
        print(browser, 'OK ' if logged else 'BAD', 'request logged with <redacted> values'); bad += not logged
        for name, value in VALUES.items():
            secret = value.split()[-1]
            ok = secret not in r.text
            print(browser, 'OK ' if ok else 'BAD', name, 'hidden'); bad += not ok
    finally:
        s.close()
print('BAD_COUNT', bad)
sys.exit(1 if bad else 0)
