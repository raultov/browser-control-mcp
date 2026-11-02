"""A minimal MCP client over stdio (JSON lines), standard library only.

Every call is timed and its reply measured in tokens with one rule for all
servers (text: characters / 4; images: Anthropic's width * height / 750 after
the API's downscale), so the numbers compare servers fairly even if they are
approximate in absolute terms.
"""

import base64
import json
import os
import select
import signal
import struct
import subprocess
import time


def image_dims(data):
    """(width, height) of a PNG, JPEG or WebP image, or None."""
    if data[:8] == b"\x89PNG\r\n\x1a\n":
        return struct.unpack(">II", data[16:24])
    if data[:2] == b"\xff\xd8":
        i = 2
        while i + 9 < len(data):
            if data[i] != 0xFF:
                i += 1
                continue
            marker = data[i + 1]
            if marker in (0xC0, 0xC1, 0xC2):
                h, w = struct.unpack(">HH", data[i + 5:i + 9])
                return w, h
            i += 2 + struct.unpack(">H", data[i + 2:i + 4])[0]
    if data[:4] == b"RIFF" and data[8:12] == b"WEBP" and data[12:16] == b"VP8 ":
        w, h = struct.unpack("<HH", data[26:30])
        return w & 0x3FFF, h & 0x3FFF
    return None


def image_tokens(w, h):
    scale = min(1.0, 1568 / max(w, h), (1_150_000 / (w * h)) ** 0.5)
    return int(w * scale * h * scale / 750)


class Result:
    """One tool call: its text, images, whether it failed, time and tokens."""

    def __init__(self, name, args, msg, ms):
        self.name, self.args, self.ms = name, args, ms
        res = msg.get("result", {})
        self.protocol_error = msg.get("error")
        self.is_error = bool(res.get("isError")) or self.protocol_error is not None
        self.text = ""
        self.images = []
        for block in res.get("content", []):
            kind = block.get("type")
            if kind == "text":
                self.text += block.get("text", "") + "\n"
            elif kind == "image":
                raw = base64.b64decode(block.get("data", ""))
                self.images.append(image_dims(raw))
        if self.protocol_error:
            self.text = json.dumps(self.protocol_error)
        self.structured = res.get("structuredContent")

    @property
    def tokens(self):
        images = sum(image_tokens(*d) for d in self.images if d)
        extra = len(json.dumps(self.structured)) // 4 if self.structured else 0
        return len(self.text) // 4 + images + extra

    def short(self, width=160):
        return self.text.strip().replace("\n", " | ")[:width]


class Server:
    """An MCP server process, initialized; its own process group, so closing
    it also ends the browsers it started."""

    def __init__(self, command, env=None, timeout=120, roots=()):
        self.timeout = timeout
        self.roots = [{"uri": "file://" + os.path.abspath(root), "name": os.path.basename(root)} for root in roots]
        self.proc = subprocess.Popen(
            command, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
            text=True, bufsize=1, env=env, start_new_session=True)
        self.n = 0
        init = self.request("initialize", {
            "protocolVersion": "2025-11-25", "capabilities": {"roots": {}},
            "clientInfo": {"name": "browser-mcp-compare", "version": "1"}})
        if "result" not in init:
            raise RuntimeError(f"initialize failed: {init}")
        self.info = init["result"].get("serverInfo", {})
        self.notify("notifications/initialized")

    def notify(self, method, params=None):
        msg = {"jsonrpc": "2.0", "method": method}
        if params is not None:
            msg["params"] = params
        self.proc.stdin.write(json.dumps(msg) + "\n")
        self.proc.stdin.flush()

    def request(self, method, params, timeout=None):
        self.n += 1
        rid = self.n
        self.proc.stdin.write(json.dumps({"jsonrpc": "2.0", "id": rid, "method": method, "params": params}) + "\n")
        self.proc.stdin.flush()
        deadline = time.time() + (timeout or self.timeout)
        while time.time() < deadline:
            ready, _, _ = select.select([self.proc.stdout], [], [], max(0.1, deadline - time.time()))
            if not ready:
                break
            line = self.proc.stdout.readline()
            if not line:
                return {"error": {"message": "the server exited"}}
            try:
                msg = json.loads(line)
            except ValueError:
                continue
            if msg.get("id") == rid and ("result" in msg or "error" in msg):
                return msg
            if "method" in msg and "id" in msg:
                # A request of the server (roots/list, elicitation): answered empty.
                result = {"roots": self.roots} if msg["method"] == "roots/list" else {}
                self.proc.stdin.write(json.dumps({"jsonrpc": "2.0", "id": msg["id"], "result": result}) + "\n")
                self.proc.stdin.flush()
        return {"error": {"message": f"no reply after {timeout or self.timeout} s"}}

    def tools(self):
        return self.request("tools/list", {}).get("result", {}).get("tools", [])

    def call(self, name, args=None, timeout=None):
        start = time.time()
        msg = self.request("tools/call", {"name": name, "arguments": args or {}}, timeout)
        return Result(name, args or {}, msg, int((time.time() - start) * 1000))

    def close(self):
        try:
            self.proc.stdin.close()
            self.proc.wait(timeout=15)
        except Exception:
            pass
        try:
            os.killpg(self.proc.pid, signal.SIGKILL)
        except (ProcessLookupError, PermissionError):
            pass
