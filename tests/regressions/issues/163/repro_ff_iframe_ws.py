"""Firefox: WebSocket traffic of a same-origin iframe in get_network_logs."""
import base64, hashlib, socket, struct, sys, threading, time
sys.path.insert(0, 'tests/compare')
from mcp_client import Server
from siteserver import Site

GUID = '258EAFA5-E914-47DA-95CA-C5AB0DC85B11'


def recv_exact(c, n):
    b = b''
    while len(b) < n:
        d = c.recv(n - len(b))
        if not d:
            raise EOFError
        b += d
    return b


def serve(c):
    try:
        req = b''
        while b'\r\n\r\n' not in req:
            req += c.recv(4096)
        key = [l.split(b':', 1)[1].strip() for l in req.split(b'\r\n') if l.lower().startswith(b'sec-websocket-key')][0]
        acc = base64.b64encode(hashlib.sha1(key + GUID.encode()).digest())
        c.sendall(b'HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: ' + acc + b'\r\n\r\n')
        while True:
            b0, b1 = recv_exact(c, 2)
            n = b1 & 0x7F
            if n == 126:
                n = struct.unpack('!H', recv_exact(c, 2))[0]
            mask = recv_exact(c, 4)
            data = bytes(x ^ mask[i % 4] for i, x in enumerate(recv_exact(c, n)))
            if b0 & 0x0F == 1:
                p = b'echo:' + data
                c.sendall(bytes([0x81, len(p)]) + p)
            elif b0 & 0x0F == 8:
                c.sendall(bytes([0x88, 2]) + data[:2])
                return
    except Exception:
        pass
    finally:
        c.close()


ls = socket.socket(); ls.bind(('127.0.0.1', 0)); ls.listen(16)
port = ls.getsockname()[1]
threading.Thread(target=lambda: [threading.Thread(target=serve, args=(ls.accept()[0],), daemon=True).start() for _ in iter(int, 1)], daemon=True).start()

browser = sys.argv[1] if len(sys.argv) > 1 else 'firefox'
site = Site()
s = Server(['target/release/browser-control-mcp', '--headless'] + (['--browser', 'firefox'] if browser == 'firefox' else []), timeout=60)
E = lambda e: print(s.call('evaluate_js', {'expression': e}).short(80))
try:
    s.call('navigate', {'url': site.url('/basic.html')})
    E("new Promise(ok=>{const w=new WebSocket('ws://127.0.0.1:%d/echo?main=1');w.onopen=()=>w.send('hi');w.onmessage=e=>ok(e.data)})" % port)
    E("new Promise(ok=>{const f=document.createElement('iframe');f.id='f1';f.src='/second.html';f.onload=()=>ok(1);document.body.append(f)})")
    E("new Promise(ok=>{const W=document.getElementById('f1').contentWindow.WebSocket;const w=new W('ws://127.0.0.1:%d/echo?frame=1');w.onopen=()=>w.send('hi');w.onmessage=e=>ok(e.data)})" % port)
    time.sleep(1.5)
    R = s.call('get_network_logs', {'type_filter': 'websocket'}).text
    print('BEFORE TEARDOWN:\n' + R)
    E("document.getElementById('f1').remove(); 1")
    time.sleep(2)
    R2 = s.call('get_network_logs', {'type_filter': 'websocket'}).text
    print('AFTER REMOVE:\n' + R2)
finally:
    s.close(); site.stop()
print('IFRAME_SOCKET_LISTED' if 'echo?frame=1' in R else 'IFRAME_SOCKET_MISSING')
print('IFRAME_CLOSE_LISTED' if 'closed ws://127.0.0.1:%d/echo?frame=1' % port in R2 else 'IFRAME_CLOSE_MISSING')
