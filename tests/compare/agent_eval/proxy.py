"""The harness's side of an agent run: the server under test, behind a
proxy that the agent CLI reaches through `relay.py`.

The agent CLI starts `relay.py <port>` as its MCP server; the relay pipes
its stdin and stdout to the proxy's TCP port. The proxy forwards every
message to the server and back, and logs one JSON line per tool call (tool
name, error flag, reply size, time; never the arguments, and an error
reply's first 300 characters only).

The proxy and the server belong to the harness, not to the CLI: CLIs signal
and kill their MCP servers when they exit (Claude Code sends SIGINT, then
SIGTERM, then kills), which would take the browser with them. Here only the
relay dies; the harness then reads the page through `call` (requests of its
own, which the agent never sees) and stops the server with `stop`.
"""

import json
import os
import signal
import socket
import subprocess
import threading
import time


class Proxy:
    def __init__(self, command, log_path, env=None):
        self.server = subprocess.Popen(
            command, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
            bufsize=0, start_new_session=True, env=env)
        self.log_file = open(log_path, "a", buffering=1)
        self.lock = threading.Lock()
        self.pending = {}
        self.own = {}
        self.own_n = 0
        self.client = None
        self.clients = 0
        self.listener = socket.socket()
        self.listener.bind(("127.0.0.1", 0))
        self.listener.listen()
        self.port = self.listener.getsockname()[1]
        threading.Thread(target=self.accept, daemon=True).start()
        threading.Thread(target=self.pump_server, daemon=True).start()

    def log(self, **entry):
        entry["t"] = round(time.time(), 3)
        with self.lock:
            if not self.log_file.closed:
                self.log_file.write(json.dumps(entry) + "\n")

    def to_server(self, data):
        with self.lock:
            self.server.stdin.write(data)
            self.server.stdin.flush()

    def to_client(self, data):
        client = self.client
        if client is None:
            return
        try:
            client.sendall(data)
        except OSError:
            self.client = None

    # -- client -> server ----------------------------------------------------

    def accept(self):
        while True:
            try:
                connection, _ = self.listener.accept()
            except OSError:
                return
            self.clients += 1
            if self.clients > 1:
                self.log(event="another client connected", clients=self.clients)
            self.client = connection
            threading.Thread(target=self.pump_client, args=(connection,), daemon=True).start()

    def pump_client(self, connection):
        with connection.makefile("rb") as stream:
            for line in stream:
                try:
                    msg = json.loads(line)
                except ValueError:
                    msg = None
                if isinstance(msg, dict) and "id" in msg and "method" in msg:
                    name = (msg.get("params") or {}).get("name") if msg["method"] == "tools/call" else None
                    self.pending[msg["id"]] = (msg["method"], name, time.time())
                try:
                    self.to_server(line)
                except OSError:
                    break
        self.log(event="client gone")
        if self.client is connection:
            self.client = None

    # -- server -> client ----------------------------------------------------

    def pump_server(self):
        for line in self.server.stdout:
            try:
                msg = json.loads(line)
            except ValueError:
                msg = None
            if isinstance(msg, dict) and "id" in msg and "method" not in msg:
                slot = self.own.pop(msg["id"], None)
                if slot is not None:
                    slot["reply"] = msg
                    slot["event"].set()
                    continue
                self.note_reply(msg)
            self.to_client(line)
        self.log(event="server exited")

    def note_reply(self, msg):
        method, name, start = self.pending.pop(msg["id"], (None, None, time.time()))
        result = msg.get("result") or {}
        if method == "initialize":
            self.log(event="initialize", instructions_chars=len(result.get("instructions") or ""))
        elif method == "tools/list":
            tools = result.get("tools") or []
            self.log(event="tools/list", tools=len(tools), chars=len(json.dumps(tools)))
        elif method == "tools/call":
            content = result.get("content") or []
            text = " ".join(b.get("text", "") for b in content if b.get("type") == "text")
            entry = {"event": "call", "tool": name, "error": bool(result.get("isError")) or "error" in msg,
                     "chars": len(text), "images": sum(1 for b in content if b.get("type") == "image"),
                     "ms": int((time.time() - start) * 1000)}
            if entry["error"]:
                entry["message"] = (text or json.dumps(msg.get("error")))[:300]
            self.log(**entry)

    # -- the harness ---------------------------------------------------------

    def call(self, tool, arguments=None, timeout=60):
        """A tool call of the harness: (is_error, text). Not logged."""
        with self.lock:
            self.own_n += 1
            rid = f"harness-{self.own_n}"
        slot = {"event": threading.Event(), "reply": None}
        self.own[rid] = slot
        request = {"jsonrpc": "2.0", "id": rid, "method": "tools/call",
                   "params": {"name": tool, "arguments": arguments or {}}}
        try:
            self.to_server((json.dumps(request) + "\n").encode())
        except OSError:
            return True, "the server is gone"
        if not slot["event"].wait(timeout):
            self.own.pop(rid, None)
            return True, f"no reply after {timeout} s"
        reply = slot["reply"]
        result = reply.get("result") or {}
        text = " ".join(b.get("text", "") for b in result.get("content") or [] if b.get("type") == "text")
        return bool(result.get("isError")) or "error" in reply, text or json.dumps(reply.get("error"))

    def stop(self):
        try:
            self.listener.close()
        except OSError:
            pass
        client = self.client
        if client is not None:
            try:
                client.close()
            except OSError:
                pass
        try:
            self.server.stdin.close()
            self.server.wait(timeout=15)
        except Exception:
            pass
        try:
            os.killpg(self.server.pid, signal.SIGKILL)
        except (ProcessLookupError, PermissionError):
            pass
        with self.lock:
            self.log_file.close()
