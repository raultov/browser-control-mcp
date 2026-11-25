"""wait_for on Chrome: closing a popup (a tab the page opened) does not end a
wait_for on it; the wait runs to its timeout. Exits 1 while the bug is present."""
import json, select, subprocess, sys, threading, time
from http.server import ThreadingHTTPServer, BaseHTTPRequestHandler


class H(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def do_GET(self):
        body = b'<title>P</title><p>page</p>'
        self.send_response(200)
        self.send_header('Content-Type', 'text/html')
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)


srv = ThreadingHTTPServer(('127.0.0.1', 0), H)
threading.Thread(target=srv.serve_forever, daemon=True).start()
base = f'http://127.0.0.1:{srv.server_address[1]}'
browser = sys.argv[1] if len(sys.argv) > 1 else 'chrome'
proc = subprocess.Popen(['target/release/browser-control-mcp', '--headless', '--browser', browser],
                        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True)
n = 0


def send(rid, method, params):
    proc.stdin.write(json.dumps({'jsonrpc': '2.0', 'id': rid, 'method': method, 'params': params}) + '\n')
    proc.stdin.flush()


def wait(ids, limit):
    got = {}
    end = time.time() + limit
    while set(ids) - set(got) and time.time() < end:
        if select.select([proc.stdout], [], [], 0.1)[0]:
            msg = json.loads(proc.stdout.readline())
            if msg.get('id') in ids:
                res = msg.get('result', {})
                got[msg['id']] = (time.time(), ''.join(b.get('text', '') for b in res.get('content', []) if b['type'] == 'text'))
    return got


def call(name, args):
    global n
    n += 1
    send(n, 'tools/call', {'name': name, 'arguments': args})
    return wait([n], 60)[n][1]


n += 1
send(n, 'initialize', {'protocolVersion': '2025-11-25', 'capabilities': {}, 'clientInfo': {'name': 'repro', 'version': '1'}})
wait([n], 60)
proc.stdin.write(json.dumps({'jsonrpc': '2.0', 'method': 'notifications/initialized'}) + '\n')
proc.stdin.flush()
call('navigate', {'url': base + '/a.html'})
call('evaluate_js', {'expression': "window.open('/popup.html'); 1", 'user_gesture': True})
popup = None
for _ in range(30):
    tabs = json.loads(call('list_tabs', {}))['tabs']
    popup = next((t['tab_id'] for t in tabs if t['url'].endswith('/popup.html')), None)
    if popup:
        break
    time.sleep(0.3)
print('popup:', popup)
send(1000, 'tools/call', {'name': 'wait_for', 'arguments': {'tab_id': popup, 'text': 'never-there', 'timeout_ms': 20000}})
time.sleep(1)
t0 = time.time()
send(1001, 'tools/call', {'name': 'close_tab', 'arguments': {'tab_id': popup}})
got = wait([1000, 1001], 40)
proc.stdin.close()
proc.wait(timeout=30)
print('close_tab:', got[1001][1])
took = got[1000][0] - t0
print(f'wait_for answered {took:.1f} s after close_tab: {got[1000][1]}')
if took > 3:
    print('BUG: wait_for did not end when its tab closed')
    sys.exit(1)
print('OK')
