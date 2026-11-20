"""A tiny WebSocket server on 127.0.0.1 for the repros, run in a thread.
/close...: text 'hello-'+path, then close 4001 'qa\\nbye'. /bad...: a frame
with reserved opcode 3. Other paths echo text frames as 'echo:'+data."""
import base64, hashlib, socket, struct, threading

GUID = '258EAFA5-E914-47DA-95CA-C5AB0DC85B11'


def _exact(c, n):
    b = b''
    while len(b) < n:
        d = c.recv(n - len(b))
        if not d:
            raise EOFError
        b += d
    return b


def _read(c):
    h = _exact(c, 2)
    op, ln = h[0] & 0x0F, h[1] & 0x7F
    if ln == 126:
        ln = struct.unpack('>H', _exact(c, 2))[0]
    elif ln == 127:
        ln = struct.unpack('>Q', _exact(c, 8))[0]
    mask = _exact(c, 4) if h[1] & 0x80 else b'\0\0\0\0'
    data = bytearray(_exact(c, ln))
    for i in range(ln):
        data[i] ^= mask[i % 4]
    return op, bytes(data)


def _frame(op, data):
    return bytes([0x80 | op, len(data)]) + data


def _handle(c):
    try:
        req = b''
        while b'\r\n\r\n' not in req:
            d = c.recv(4096)
            if not d:
                return
            req += d
        lines = req.decode('latin1').split('\r\n')
        path = lines[0].split(' ')[1]
        key = next(l.split(':', 1)[1].strip() for l in lines if l.lower().startswith('sec-websocket-key:'))
        acc = base64.b64encode(hashlib.sha1((key + GUID).encode()).digest()).decode()
        c.sendall(('HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n'
                   'Sec-WebSocket-Accept: %s\r\n\r\n' % acc).encode())
        if path.startswith('/close'):
            c.sendall(_frame(1, ('hello-' + path).encode()))
            c.sendall(_frame(8, struct.pack('>H', 4001) + b'qa\nbye'))
            try:
                _read(c)
            except Exception:
                pass
        elif path.startswith('/bad'):
            c.sendall(b'\x83\x01x')
            while c.recv(4096):
                pass
        else:
            while True:
                op, d = _read(c)
                if op == 1:
                    c.sendall(_frame(1, b'echo:' + d))
                elif op == 8:
                    c.sendall(_frame(8, d[:2]))
                    return
    except Exception:
        pass
    finally:
        c.close()


def start():
    """Starts the server; returns its port."""
    s = socket.socket()
    s.bind(('127.0.0.1', 0))
    s.listen(64)

    def serve():
        while True:
            c, _ = s.accept()
            threading.Thread(target=_handle, args=(c,), daemon=True).start()
    threading.Thread(target=serve, daemon=True).start()
    return s.getsockname()[1]
