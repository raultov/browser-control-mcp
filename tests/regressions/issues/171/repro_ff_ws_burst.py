"""Firefox: with many WebSockets refused or dropped at the handshake next to
live ones, a refusal's close is put on a live socket (listed closed, 1006)
and the refused URL goes missing."""
import sys, re, time, socket, threading, base64, hashlib, struct
sys.path.insert(0, 'tests/compare')
from mcp_client import Server
from siteserver import Site


def conn(c):
    try:
        data = b''
        while b'\r\n\r\n' not in data:
            x = c.recv(4096)
            if not x:
                return c.close()
            data += x
        path = data.split(b' ')[1]
        if path.startswith(b'/deny'):
            c.sendall(b'HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n')
            return c.close()
        if path.startswith(b'/drop'):
            return c.close()
        key = re.search(rb'(?i)sec-websocket-key:\s*(\S+)', data).group(1)
        acc = base64.b64encode(hashlib.sha1(key + b'258EAFA5-E914-47DA-95CA-C5AB0DC85B11').digest())
        c.sendall(b'HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n'
                  b'Sec-WebSocket-Accept: ' + acc + b'\r\n\r\n')
        while True:
            h = c.recv(2)
            if len(h) < 2:
                return c.close()
            op, ln = h[0] & 15, h[1] & 127
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
ws.listen(64)
W = ws.getsockname()[1]
threading.Thread(target=lambda: [threading.Thread(target=conn, args=(ws.accept()[0],), daemon=True).start() for _ in iter(int, 1)], daemon=True).start()

site = Site()
s = Server(['target/release/browser-control-mcp', '--headless', '--browser', 'firefox'], timeout=120)
s.call('navigate', {'url': site.url('/basic.html')})
s.call('evaluate_js', {'expression': f"window.socks=[]; for(let k=1;k<=10;k++) socks.push(new WebSocket('ws://127.0.0.1:{W}/deny?n='+k)); for(let k=1;k<=5;k++) socks.push(new WebSocket('ws://127.0.0.1:{W}/drop?n='+k)); for(let k=1;k<=3;k++){{const w=new WebSocket('ws://127.0.0.1:{W}/echo?live='+k); w.onopen=()=>w.send('live'+k); socks.push(w);}} 1"})
states = ''
t0 = time.time()
while time.time() - t0 < 120:
    time.sleep(2)
    states = re.search(r'"value": "([^"]*)"', s.call('evaluate_js', {'expression': "socks.map(w=>w.readyState).join(',')"}).text).group(1)
    if '0' not in states.split(','):
        break
time.sleep(2)
L = s.call('get_network_logs', {'type_filter': 'websocket'})
s.close(); site.stop()
print(L.text)
print('page readyStates:', states)
live_closed = [l for l in L.text.splitlines() if re.match(rf'^\S+ closed ws://127\.0\.0\.1:{W}/echo\?live=\d', l)]
missing = [f'{kind}?n={k}' for kind, n in (('deny', 10), ('drop', 5)) for k in range(1, n + 1)
           if not re.search(rf'^\S+ (closed|failed) ws://127\.0\.0\.1:{W}/{kind}\?n={k}(:|$)', L.text, re.M)]
print('live sockets listed closed:', live_closed)
print('refused/dropped sockets missing:', missing)
bug = states.endswith('1,1,1') and bool(live_closed or missing)
print('MISATTRIBUTED_CLOSE' if bug else 'OK')
sys.exit(1 if bug else 0)
