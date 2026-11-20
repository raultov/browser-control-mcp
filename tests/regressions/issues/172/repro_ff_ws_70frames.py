"""Firefox: 70 same-origin iframes each open a WebSocket; after all are
removed, only 64 sockets are listed closed (SocketOwners bound)."""
import sys, re, time, socket, threading, base64, hashlib
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
ws.listen(128)
W = ws.getsockname()[1]
threading.Thread(target=lambda: [threading.Thread(target=conn, args=(ws.accept()[0],), daemon=True).start() for _ in iter(int, 1)], daemon=True).start()

site = Site()
s = Server(['target/release/browser-control-mcp', '--headless', '--browser', 'firefox'], timeout=120)
s.call('navigate', {'url': site.url('/basic.html')})
r = s.call('evaluate_js', {'expression': f"window.fr=[]; await Promise.all(Array.from({{length:70}},(_,i)=>new Promise(ok=>{{const f=document.createElement('iframe'); f.width=20; f.height=20; f.src='/second.html?f='+i; f.onload=()=>{{const w=new f.contentWindow.WebSocket('ws://127.0.0.1:{W}/echo?k='+i); w.onopen=()=>w.send('m'+i); w.onmessage=()=>ok(1);}}; fr.push(f); document.body.append(f);}}))); 'all'"}, timeout=90)
print('open all:', r.short(100))
time.sleep(2)
s.call('evaluate_js', {'expression': "fr.forEach(f=>f.remove()); 1"})
time.sleep(5)
L = s.call('get_network_logs', {'type_filter': 'websocket'})
s.close(); site.stop()
print(L.text.splitlines()[0] if L.text else '')
not_closed = [k for k in range(70) if not re.search(rf'^\S+ closed ws://127\.0\.0\.1:{W}/echo\?k={k}(:|$)', L.text, re.M)]
print('sockets never listed closed:', not_closed)
print('FRAME_SOCKETS_NOT_CLOSED' if '"all"' in r.text and not_closed else 'OK')
sys.exit(1 if '"all"' in r.text and not_closed else 0)
