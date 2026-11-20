"""A WebSocket whose handshake gets HTTP 403: is it listed in get_network_logs?"""
import socket, sys, threading, time
sys.path.insert(0, 'tests/compare')
from mcp_client import Server
from siteserver import Site


def serve(c):
    try:
        req = b''
        while b'\r\n\r\n' not in req:
            req += c.recv(4096)
        c.sendall(b'HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n')
        time.sleep(0.5)
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
try:
    s.call('navigate', {'url': site.url('/basic.html')})
    r = s.call('evaluate_js', {'expression': "new Promise(ok=>{const w=new WebSocket('ws://127.0.0.1:%d/deny?n=1');w.onclose=e=>ok('closed '+e.code);setTimeout(()=>ok('timeout'),5000)})" % port})
    print('page:', r.short(100))
    time.sleep(1.5)
    R = s.call('get_network_logs', {'type_filter': 'websocket'}).text
    print('websocket logs:\n' + R)
    print('all logs:\n' + s.call('get_network_logs', {}).text)
finally:
    s.close(); site.stop()
listed = any(l.split(' ', 2)[1:2] in (['closed'], ['failed']) and '/deny?n=1' in l for l in R.splitlines())
print('DENIED_SOCKET_LISTED' if listed else 'DENIED_SOCKET_MISSING')
