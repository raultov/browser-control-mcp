"""get_console_logs on Chrome: a message logged by a script a dedicated worker
imported (importScripts) is listed twice. Exits 1 while the bug is present."""
import json, os, subprocess, sys, threading, time
from http.server import ThreadingHTTPServer, BaseHTTPRequestHandler

PAGES = {
    '/main.html': ('text/html', "<!doctype html><title>m</title><script>window.w = new Worker('/worker.js')</script>"),
    '/worker.js': ('application/javascript', "importScripts('/lib.js'); onmessage = () => qaLog();"),
    '/lib.js': ('application/javascript', "self.qaLog = () => console.log('qa-from-lib');"),
}


class H(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def do_GET(self):
        kind, body = PAGES.get(self.path, ('text/plain', 'nf'))
        body = body.encode()
        self.send_response(200 if self.path in PAGES else 404)
        self.send_header('Content-Type', kind)
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)


srv = ThreadingHTTPServer(('127.0.0.1', 0), H)
threading.Thread(target=srv.serve_forever, daemon=True).start()
base = f'http://127.0.0.1:{srv.server_address[1]}'

proc = subprocess.Popen(['target/release/browser-control-mcp', '--headless', '--browser', 'chrome'],
                        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True)
n = 0


def rpc(method, params):
    global n
    n += 1
    proc.stdin.write(json.dumps({'jsonrpc': '2.0', 'id': n, 'method': method, 'params': params}) + '\n')
    proc.stdin.flush()
    while True:
        msg = json.loads(proc.stdout.readline())
        if msg.get('id') == n:
            return msg


def call(name, args):
    res = rpc('tools/call', {'name': name, 'arguments': args})['result']
    return ''.join(b.get('text', '') for b in res['content'] if b['type'] == 'text')


rpc('initialize', {'protocolVersion': '2025-11-25', 'capabilities': {}, 'clientInfo': {'name': 'repro', 'version': '1'}})
proc.stdin.write(json.dumps({'jsonrpc': '2.0', 'method': 'notifications/initialized'}) + '\n')
proc.stdin.flush()
call('navigate', {'url': base + '/main.html'})
time.sleep(1)
call('evaluate_js', {'expression': 'w.postMessage(1); 1'})
time.sleep(1.5)
text = call('get_console_logs', {})
proc.stdin.close()
proc.wait(timeout=20)
print(text)
lines = [l for l in text.splitlines() if 'qa-from-lib' in l]
if len(lines) != 1:
    print(f'BUG: one console.log listed {len(lines)} times')
    sys.exit(1)
print('OK: listed once')
