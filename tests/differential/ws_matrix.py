"""WebSocket closes in get_network_logs: every owner of a socket against every
way it ends, on one browser. Each cell opens one socket (a URL of its own),
ends it, and expects exactly one `closed` or `failed` line for it, as the
server and the page saw it end (eleven reports, #153-#178, were cells of
this table nobody had tried).

Owners: the tab's document, a same-origin iframe, a cross-origin iframe, a
cross-origin iframe inside a same-origin one, a dedicated worker. Causes:
the page's `close()`, the server's close frame, the TCP connection dropped
without one, a navigation within the owner's origin and to another one, the
iframe removed, the worker terminated.

Left out on purpose: a socket whose server never answers the close frame
(`/mute` of ws_server.py) stays CLOSING in the page, so it is not listed
closed until the connection ends; on Firefox that holds for a terminated
worker's socket too (Chrome drops the worker's connection at once).

Usage (from the repository root):
    python3 -I tests/differential/ws_matrix.py [--browser chrome|firefox]
        [--bin target/debug/browser-control-mcp] [--only owner/cause]
Exits 1 when a cell has no close line, or more than one.
"""

import argparse
import json
import os
import re
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(os.path.dirname(HERE))
sys.path.insert(0, os.path.join(ROOT, "tests", "compare"))
sys.path.insert(0, HERE)

from mcp_client import Server  # noqa: E402
from siteserver import Site  # noqa: E402
import ws_server  # noqa: E402

OWNERS = {
    "document": ["close", "server-close", "tcp-cut", "nav-same-origin", "nav-other-origin"],
    "iframe-same-origin": ["close", "server-close", "tcp-cut", "nav-same-origin", "nav-other-origin", "removed"],
    "iframe-cross-origin": ["close", "server-close", "tcp-cut", "nav-same-origin", "nav-other-origin", "removed"],
    "iframe-nested": ["close", "server-close", "removed"],
    "worker": ["close", "server-close", "tcp-cut", "terminated"],
}

# How a command reaches the socket's owner, from the top document.
POST = {
    "document": "window.sock{call}",
    "iframe-same-origin": "document.getElementById('o').contentWindow.postMessage({msg}, '*')",
    "iframe-cross-origin": "document.getElementById('o').contentWindow.postMessage({msg}, '*')",
    "iframe-nested": "document.getElementById('o').contentWindow.frames[0].postMessage({msg}, '*')",
    "worker": "window.wk.postMessage({msg})",
}

WAIT_OPEN = """
await new Promise((resolve, reject) => {
  const done = () => resolve('open');
  addEventListener('message', e => { if (String(e.data).startsWith('open')) done(); });
  if (window.wk) wk.onmessage = done;
  setTimeout(() => reject(new Error('the socket did not open')), 10000);
})"""


class Cell:
    def __init__(self, server, site, port, owner, cause):
        self.server, self.site, self.owner, self.cause = server, site, owner, cause
        self.url = "ws://127.0.0.1:%d/echo?cell=%s-%s" % (port, owner, cause)

    def js(self, expression):
        reply = self.server.call("evaluate_js", {"expression": expression})
        if reply.is_error:
            raise RuntimeError("evaluate_js: " + reply.text.strip()[:200])
        return reply.text

    def owner_page(self, origin):
        base = self.site.url if origin == "same" else self.site.other_origin
        return base("/wsowner.html?u=" + _quote(self.url))

    def open(self):
        self.server.call("navigate", {"url": self.site.url("/basic.html")})
        if self.owner == "document":
            setup = "window.sock = new WebSocket(%s); sock.onopen = () => { sock.send('hi'); postMessage('open', '*') };" % json.dumps(self.url)
        elif self.owner == "worker":
            setup = "window.wk = new Worker('/wsworker.js?u=' + encodeURIComponent(%s));" % json.dumps(self.url)
        else:
            src = {
                "iframe-same-origin": self.owner_page("same"),
                "iframe-cross-origin": self.owner_page("other"),
                "iframe-nested": self.site.url("/wsowner.html?child=" + _quote(self.owner_page("other"))),
            }[self.owner]
            setup = "const f = document.createElement('iframe'); f.id = 'o'; f.src = %s; document.body.append(f);" % json.dumps(src)
        self.js(setup + WAIT_OPEN)
        time.sleep(0.5)

    def end(self):
        post = POST[self.owner]
        message = {"close": "close", "server-close": "send:bye", "tcp-cut": "send:cut"}.get(self.cause)
        if message is not None:
            if self.owner == "document":
                call = ".close()" if message == "close" else ".send(%s)" % json.dumps(message[5:])
                self.js(post.format(call=call) + "; 1")
            else:
                self.js(post.format(msg=json.dumps(message)) + "; 1")
        elif self.owner == "document":
            target = self.site.url("/second.html") if self.cause == "nav-same-origin" else self.site.other_origin("/basic.html")
            self.server.call("navigate", {"url": target})
        elif self.cause == "terminated":
            self.js("wk.terminate(); 1")
        elif self.cause == "removed":
            self.js("document.getElementById('o').remove(); 1")
        else:
            same = self.site.url if self.owner == "iframe-same-origin" else self.site.other_origin
            other = self.site.other_origin if self.owner == "iframe-same-origin" else self.site.url
            target = (same if self.cause == "nav-same-origin" else other)("/basic.html")
            self.js("document.getElementById('o').src = %s; 1" % json.dumps(target))

    def closes(self):
        """The close lines of the cell's socket, waiting at most 8 s for one."""
        pattern = re.compile(r"^\S+ (closed|failed) %s(:|$)" % re.escape(self.url), re.M)
        lines = []
        for _ in range(16):
            logs = self.server.call("get_network_logs", {"type_filter": "websocket"}).text
            lines = [m.group(0) + logs[m.end():].split("\n", 1)[0] for m in pattern.finditer(logs)]
            if lines:
                time.sleep(1)
                logs = self.server.call("get_network_logs", {"type_filter": "websocket"}).text
                return [m.group(0) + logs[m.end():].split("\n", 1)[0] for m in pattern.finditer(logs)], logs
            time.sleep(0.5)
        return lines, logs


def _quote(text):
    from urllib.parse import quote
    return quote(text, safe="")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--browser", default="chrome", choices=["chrome", "firefox"])
    parser.add_argument("--bin", default=os.path.join(ROOT, "target", "debug", "browser-control-mcp"))
    parser.add_argument("--only", help="owner/cause")
    parser.add_argument("--visible", action="store_true")
    args = parser.parse_args()

    command = [args.bin, "--browser", args.browser] + ([] if args.visible else ["--headless"])
    site = Site()
    port = ws_server.start()
    server = Server(command, timeout=120)
    failures = 0
    try:
        for owner, causes in OWNERS.items():
            for cause in causes:
                if args.only and args.only != "%s/%s" % (owner, cause):
                    continue
                cell = Cell(server, site, port, owner, cause)
                try:
                    cell.open()
                    cell.end()
                    lines, logs = cell.closes()
                    verdict = "PASS" if len(lines) == 1 else "FAIL"
                    detail = lines[0].split(" ", 1)[1] if len(lines) == 1 else "%d close lines" % len(lines)
                except Exception as error:  # a cell that cannot run is a failure
                    verdict, detail = "FAIL", str(error)
                failures += verdict == "FAIL"
                print("%s %-20s %-17s %s" % (verdict, owner, cause, detail.replace(cell.url, "<url>")))
    finally:
        server.close()
        site.stop()
    print("WebSocket matrix (%s): %d failure(s)" % (args.browser, failures))
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
