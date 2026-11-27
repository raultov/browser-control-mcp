import json, sys, threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
sys.path.insert(0, 'tests/compare')
from mcp_client import Server


class H(BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path == '/echo':
            body = json.dumps({'cookie': self.headers.get('Cookie')}).encode()
        else:
            body = b'<p>page</p>'
        self.send_response(200)
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *a):
        pass


srv = ThreadingHTTPServer(('127.0.0.1', 0), H)
threading.Thread(target=srv.serve_forever, daemon=True).start()
page = f'http://127.0.0.1:{srv.server_address[1]}/page.html'
s = Server(['target/release/browser-control-mcp', '--headless', '--tool-groups', 'storage'])
s.call('navigate', {'url': page})
r = s.call('cookies', {'action': 'set', 'name': 'sec', 'value': 'x', 'secure': True})
print('set:', r.text.strip())
e = s.call('evaluate_js', {'expression': "await (await fetch('/echo')).text()"})
sent = json.loads(json.loads(e.text)['value'])['cookie']
g = s.call('cookies', {'action': 'get'})
s.close()
srv.shutdown()
print('Cookie header the page sends:', sent)
print('cookies get:', g.text.split('\n')[0])
if sent and 'sec=' in sent and '"sec"' not in g.text:
    print('NOT LISTED: the Secure cookie is sent to the page but cookies get omits it')
    sys.exit(1)
sys.exit(0)
