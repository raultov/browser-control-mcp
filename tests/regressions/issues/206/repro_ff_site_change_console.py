"""get_console_logs on Firefox: after navigate moves the tab to another site,
the new page's own console messages (logged while it loads) are cleared with
the old site's. Exits 1 while the bug is present."""
import json, subprocess, sys, threading, time
from http.server import ThreadingHTTPServer, BaseHTTPRequestHandler


class H(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def do_GET(self):
        name = self.path.strip('/').split('.')[0]
        body = f"<!doctype html><title>{name}</title><script>console.log('on-{name}')</script>".encode()
        self.send_response(200)
        self.send_header('Content-Type', 'text/html')
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)


srv = ThreadingHTTPServer(('127.0.0.1', 0), H)
threading.Thread(target=srv.serve_forever, daemon=True).start()
port = srv.server_address[1]
browser = sys.argv[1] if len(sys.argv) > 1 else 'firefox'
proc = subprocess.Popen(['target/release/browser-control-mcp', '--headless', '--browser', browser],
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
call('navigate', {'url': f'http://127.0.0.1:{port}/a.html'})
time.sleep(0.5)
print(call('navigate', {'url': f'http://localhost:{port}/c.html'}).splitlines()[0])
time.sleep(2)
text = call('get_console_logs', {})
proc.stdin.close()
proc.wait(timeout=30)
print(text)
if 'on-c' not in text:
    print(f'BUG: {browser}: the new page\'s console.log(\'on-c\') is missing after the site change')
    sys.exit(1)
print('OK')
