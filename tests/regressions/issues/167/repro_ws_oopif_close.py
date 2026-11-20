"""Chrome: the WebSocket of a cross-origin iframe is never listed as closed
when the iframe is removed (a same-origin iframe's socket is)."""
import sys, os, re, time, socket, threading, functools, tempfile, base64, hashlib, struct, urllib.parse
from http.server import ThreadingHTTPServer, SimpleHTTPRequestHandler
sys.path.insert(0, 'tests/compare')
from mcp_client import Server


def ws_conn(c):
    try:
        data = b''
        while b'\r\n\r\n' not in data:
            x = c.recv(4096)
            if not x:
                return c.close()
            data += x
        key = re.search(rb'(?i)sec-websocket-key:\s*(\S+)', data).group(1)
        acc = base64.b64encode(hashlib.sha1(key + b'258EAFA5-E914-47DA-95CA-C5AB0DC85B11').digest())
        c.sendall(b'HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n'
                  b'Sec-WebSocket-Accept: ' + acc + b'\r\n\r\n')
        while True:
            h = c.recv(2)
            if len(h) < 2:
                return c.close()
            op, ln = h[0] & 15, h[1] & 127
            if ln == 126:
                ln = struct.unpack('>H', c.recv(2))[0]
            mask = c.recv(4)
            p = bytes(b ^ mask[i % 4] for i, b in enumerate(c.recv(ln)))
            if op == 1:
                out = b'echo:' + p
                c.sendall(b'\x81' + bytes([len(out)]) + out)
            elif op == 8:
                c.sendall(b'\x88\x00')
                return c.close()
    except Exception:
        c.close()


ws = socket.socket()
ws.bind(('127.0.0.1', 0))
ws.listen(16)
W = ws.getsockname()[1]
threading.Thread(target=lambda: [threading.Thread(target=ws_conn, args=(ws.accept()[0],), daemon=True).start() for _ in iter(int, 1)], daemon=True).start()

d = tempfile.mkdtemp()
open(os.path.join(d, 'sock.html'), 'w').write(
    "<!doctype html><title>sock</title><p>sock</p><script>const ws=new WebSocket(new URLSearchParams(location.search).get('u'));"
    " ws.onopen=()=>ws.send('hi-'+location.hostname);</script>")
open(os.path.join(d, 'top.html'), 'w').write('<!doctype html><title>top</title><p>top</p>')


class H(SimpleHTTPRequestHandler):
    def log_message(self, *a):
        pass


httpd = ThreadingHTTPServer(('127.0.0.1', 0), functools.partial(H, directory=d))
threading.Thread(target=httpd.serve_forever, daemon=True).start()
P = httpd.server_address[1]

s = Server(['target/release/browser-control-mcp', '--headless'], timeout=90)
s.call('navigate', {'url': f'http://127.0.0.1:{P}/top.html'})
result = {}
for host, k in [('127.0.0.1', 'same'), ('localhost', 'cross')]:
    u = urllib.parse.quote(f'ws://127.0.0.1:{W}/echo?k={k}', safe='')
    s.call('evaluate_js', {'expression': f"{{ const f=document.createElement('iframe'); f.id='f_{k}'; f.src='http://{host}:{P}/sock.html?u={u}'; document.body.append(f); }} 1"})
    time.sleep(3)
    s.call('evaluate_js', {'expression': f"document.getElementById('f_{k}').remove(); 1"})
    time.sleep(3)
L = s.call('get_network_logs', {'type_filter': 'websocket'})
s.close()
print(L.text)
for k in ['same', 'cross']:
    rec = f'received ws://127.0.0.1:{W}/echo?k={k}' in L.text
    clo = bool(re.search(rf'^\S+ closed ws://127\.0\.0\.1:{W}/echo\?k={k}(:|$)', L.text, re.M))
    print(f'{k}: received={rec} closed={clo}')
    result[k] = (rec, clo)
bug = result['cross'][0] and not result['cross'][1]
print('CROSS_ORIGIN_SOCKET_NOT_CLOSED' if bug else 'OK')
sys.exit(1 if bug else 0)
