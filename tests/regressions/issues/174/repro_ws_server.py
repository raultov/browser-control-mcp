"""Minimal WebSocket server for tests. /deny -> 403, /drop -> close w/o answer,
/acc-close -> 101 then close, everything else echoes."""
import base64, hashlib, socket, struct, threading

LOG = []
GUID = '258EAFA5-E914-47DA-95CA-C5AB0DC85B11'


def _recv_exact(c, n):
    b = b''
    while len(b) < n:
        d = c.recv(n - len(b))
        if not d:
            raise EOFError
        b += d
    return b


def _handle(c):
    path = '?'
    try:
        head = b''
        while b'\r\n\r\n' not in head:
            d = c.recv(4096)
            if not d:
                return
            head += d
        lines = head.decode('latin1').split('\r\n')
        path = lines[0].split(' ')[1]
        key = ''
        for l in lines[1:]:
            if l.lower().startswith('sec-websocket-key:'):
                key = l.split(':', 1)[1].strip()
        LOG.append(('open', path))
        if path.startswith('/deny'):
            c.sendall(b'HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n')
            return
        if path.startswith('/drop'):
            return
        acc = base64.b64encode(hashlib.sha1((key + GUID).encode()).digest()).decode()
        c.sendall(('HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n'
                   'Sec-WebSocket-Accept: %s\r\n\r\n' % acc).encode())
        if path.startswith('/acc-close'):
            return
        while True:
            h = _recv_exact(c, 2)
            op = h[0] & 0x0F
            ln = h[1] & 0x7F
            if ln == 126:
                ln = struct.unpack('>H', _recv_exact(c, 2))[0]
            elif ln == 127:
                ln = struct.unpack('>Q', _recv_exact(c, 8))[0]
            mask = _recv_exact(c, 4) if h[1] & 0x80 else b'\0\0\0\0'
            data = bytes(b ^ mask[i % 4] for i, b in enumerate(_recv_exact(c, ln)))
            if op == 8:
                c.sendall(bytes([0x88, len(data[:125])]) + data[:125])
                LOG.append(('closeframe', path))
                return
            if op == 1:
                if len(data) < 126:
                    c.sendall(bytes([0x81, len(data)]) + data)
                else:
                    c.sendall(bytes([0x81, 126]) + struct.pack('>H', len(data)) + data)
            elif op == 9:
                c.sendall(bytes([0x8A, len(data)]) + data)
    except Exception:
        pass
    finally:
        LOG.append(('tcpclose', path))
        try:
            c.close()
        except Exception:
            pass


def start():
    s = socket.socket()
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.bind(('127.0.0.1', 0))
    s.listen(256)

    def loop():
        while True:
            c, _ = s.accept()
            threading.Thread(target=_handle, args=(c,), daemon=True).start()
    threading.Thread(target=loop, daemon=True).start()
    return s.getsockname()[1]
