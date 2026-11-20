"""A WebSocket server for the differential checks, on 127.0.0.1 and a free
port, one thread per connection.

Paths: `/deny` answers 403, `/drop` closes without answering, `/acc-close`
accepts then closes the connection, `/mute` never answers a close frame;
any other path echoes text frames. On every path that echoes, the text
`bye` makes the server close cleanly (a close frame 1000, then the TCP
connection) and `cut` makes it drop the TCP connection without a close
frame. `LOG` records `(event, path)`: open, closeframe (answered),
closeframe-ignored, serverclose, cut, tcpclose.
"""

import base64
import hashlib
import socket
import struct
import threading

LOG = []
GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"


def _recv_exact(c, n):
    data = b""
    while len(data) < n:
        chunk = c.recv(n - len(data))
        if not chunk:
            raise EOFError
        data += chunk
    return data


def _frame(opcode, payload):
    if len(payload) < 126:
        return bytes([0x80 | opcode, len(payload)]) + payload
    return bytes([0x80 | opcode, 126]) + struct.pack(">H", len(payload)) + payload


def _read_frame(c):
    head = _recv_exact(c, 2)
    opcode = head[0] & 0x0F
    length = head[1] & 0x7F
    if length == 126:
        length = struct.unpack(">H", _recv_exact(c, 2))[0]
    elif length == 127:
        length = struct.unpack(">Q", _recv_exact(c, 8))[0]
    mask = _recv_exact(c, 4) if head[1] & 0x80 else b"\0\0\0\0"
    data = bytes(b ^ mask[i % 4] for i, b in enumerate(_recv_exact(c, length)))
    return opcode, data


def _handshake(c):
    head = b""
    while b"\r\n\r\n" not in head:
        chunk = c.recv(4096)
        if not chunk:
            return None, None
        head += chunk
    lines = head.decode("latin1").split("\r\n")
    path = lines[0].split(" ")[1]
    key = ""
    for line in lines[1:]:
        if line.lower().startswith("sec-websocket-key:"):
            key = line.split(":", 1)[1].strip()
    return path, key


def _serve(c, path):
    while True:
        opcode, data = _read_frame(c)
        if opcode == 8:
            if path.startswith("/mute"):
                LOG.append(("closeframe-ignored", path))
                continue
            c.sendall(_frame(8, data[:125]))
            LOG.append(("closeframe", path))
            return
        if opcode == 1 and data == b"bye":
            c.sendall(_frame(8, struct.pack(">H", 1000) + b"bye"))
            LOG.append(("serverclose", path))
            return
        if opcode == 1 and data == b"cut":
            LOG.append(("cut", path))
            c.shutdown(socket.SHUT_RDWR)
            return
        if opcode == 1:
            c.sendall(_frame(1, data))
        elif opcode == 9:
            c.sendall(_frame(10, data))


def _handle(c):
    path = "?"
    try:
        path, key = _handshake(c)
        if path is None:
            return
        LOG.append(("open", path))
        if path.startswith("/deny"):
            c.sendall(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            return
        if path.startswith("/drop"):
            return
        accept = base64.b64encode(hashlib.sha1((key + GUID).encode()).digest()).decode()
        c.sendall(("HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n"
                   "Sec-WebSocket-Accept: %s\r\n\r\n" % accept).encode())
        if path.startswith("/acc-close"):
            return
        _serve(c, path)
    except Exception:
        pass
    finally:
        LOG.append(("tcpclose", path))
        try:
            c.close()
        except Exception:
            pass


def start():
    """Starts the server; answers its port."""
    s = socket.socket()
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.bind(("127.0.0.1", 0))
    s.listen(256)

    def accept_loop():
        while True:
            c, _ = s.accept()
            threading.Thread(target=_handle, args=(c,), daemon=True).start()

    threading.Thread(target=accept_loop, daemon=True).start()
    return s.getsockname()[1]
