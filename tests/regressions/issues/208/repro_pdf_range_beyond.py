"""save_pdf with page_ranges beyond the document: Chrome fails ('Page range
exceeds page count'), Firefox answers success with a blank page.
Exits 1 while the two browsers disagree."""
import json, os, subprocess, sys, threading
from http.server import ThreadingHTTPServer, BaseHTTPRequestHandler

PAGE = b"<!doctype html><title>P</title><style>section{break-after:page}section:last-child{break-after:auto}</style><section>One</section><section>Two</section>"


class H(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def do_GET(self):
        self.send_response(200)
        self.send_header('Content-Type', 'text/html')
        self.send_header('Content-Length', str(len(PAGE)))
        self.end_headers()
        self.wfile.write(PAGE)


srv = ThreadingHTTPServer(('127.0.0.1', 0), H)
threading.Thread(target=srv.serve_forever, daemon=True).start()
url = f'http://127.0.0.1:{srv.server_address[1]}/p.html'


def run(browser):
    proc = subprocess.Popen(['target/release/browser-control-mcp', '--headless', '--tool-groups', 'files', '--browser', browser],
                            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True)
    n = [0]

    def rpc(method, params):
        n[0] += 1
        proc.stdin.write(json.dumps({'jsonrpc': '2.0', 'id': n[0], 'method': method, 'params': params}) + '\n')
        proc.stdin.flush()
        while True:
            msg = json.loads(proc.stdout.readline())
            if msg.get('id') == n[0]:
                return msg

    rpc('initialize', {'protocolVersion': '2025-11-25', 'capabilities': {}, 'clientInfo': {'name': 'repro', 'version': '1'}})
    proc.stdin.write(json.dumps({'jsonrpc': '2.0', 'method': 'notifications/initialized'}) + '\n')
    proc.stdin.flush()
    rpc('tools/call', {'name': 'navigate', 'arguments': {'url': url}})
    path = os.path.abspath(f'repro_range_{browser}.pdf')
    res = rpc('tools/call', {'name': 'save_pdf', 'arguments': {'path': path, 'page_ranges': '9'}})['result']
    proc.stdin.close()
    proc.wait(timeout=30)
    if os.path.exists(path):
        os.remove(path)
    text = ''.join(b.get('text', '') for b in res['content'] if b['type'] == 'text')
    return bool(res.get('isError')), text.splitlines()[0] if text else ''


out = {b: run(b) for b in ('chrome', 'firefox')}
for b, (err, first) in out.items():
    print(f'{b}: isError={err}: {first}')
if out['chrome'][0] != out['firefox'][0]:
    print("BUG: save_pdf page_ranges '9' of a 2-page document: Chrome and Firefox disagree")
    sys.exit(1)
print('OK: same outcome')
