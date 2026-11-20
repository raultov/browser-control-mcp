"""WebSocket closes in get_network_logs: every owner of a socket against every
way it ends, on one browser. Each cell opens one socket (a URL of its own),
ends it, and expects exactly one `closed` or `failed` line for it, as the
server and the page saw it end (eleven reports, #153-#178, were cells of
this table nobody had tried).

Owners: the tab's document, a same-origin iframe, a cross-origin iframe, a
cross-origin iframe inside a same-origin one, a dedicated worker, a popup.
Causes: the page's `close()`, the server's close frame, the TCP connection
dropped without one, a navigation within the owner's origin and to another
one, a reload, the iframe removed, the worker terminated; and sockets that
never open: the handshake refused (403), unanswered, or accepted then
dropped. Two sockets to one URL, one of them closed, must give one line.

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
from urllib.parse import quote

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(os.path.dirname(HERE))
sys.path.insert(0, os.path.join(ROOT, "tests", "compare"))
sys.path.insert(0, HERE)

from mcp_client import Server  # noqa: E402
from siteserver import Site  # noqa: E402
import ws_server  # noqa: E402

NEVER_OPEN = ["refused", "unanswered", "accepted-then-dropped"]
FRAME_CAUSES = ["close", "server-close", "tcp-cut", "nav-same-origin", "nav-other-origin", "removed"]

OWNERS = {
    "document": ["close", "server-close", "tcp-cut", "nav-same-origin", "nav-other-origin", "reload",
                 "twin-closed"] + NEVER_OPEN,
    "iframe-same-origin": FRAME_CAUSES + NEVER_OPEN,
    "iframe-cross-origin": FRAME_CAUSES + NEVER_OPEN,
    "iframe-nested": ["close", "server-close", "removed"],
    "worker": ["close", "server-close", "tcp-cut", "terminated"] + NEVER_OPEN,
    "popup": ["close", "server-close", "tcp-cut", "nav-same-origin", "nav-other-origin"],
}

# The server's path for each cause (see ws_server.py); echo for the others.
PATHS = {"refused": "deny", "unanswered": "drop", "accepted-then-dropped": "acc-close"}

# How a command reaches the socket's owner, from the top document.
POST = {
    "document": "window.sock{call}",
    "iframe-same-origin": "document.getElementById('o').contentWindow.postMessage({msg}, '*')",
    "iframe-cross-origin": "document.getElementById('o').contentWindow.postMessage({msg}, '*')",
    "iframe-nested": "document.getElementById('o').contentWindow.frames[0].postMessage({msg}, '*')",
    "worker": "window.wk.postMessage({msg})",
    "popup": "window.pop.postMessage({msg}, '*')",
}

# Waits for the owner's message `word` (open, closed), from a frame, a popup
# or the worker.
WAIT = """
await new Promise((resolve, reject) => {
  const done = e => { if (String(e.data).startsWith(%(word)s)) resolve(%(word)s); };
  addEventListener('message', done);
  if (window.wk) wk.addEventListener('message', done);
  setTimeout(() => reject(new Error('the socket did not get ' + %(word)s)), 10000);
})"""


class Cell:
    def __init__(self, server, site, port, owner, cause):
        self.server, self.site, self.owner, self.cause = server, site, owner, cause
        self.url = "ws://127.0.0.1:%d/%s?cell=%s-%s" % (port, PATHS.get(cause, "echo"), owner, cause)
        self.tab = None

    def js(self, expression, gesture=False):
        reply = self.server.call("evaluate_js", {"expression": expression, "user_gesture": gesture})
        if reply.is_error:
            raise RuntimeError("evaluate_js: " + reply.text.strip()[:200])
        return reply.text

    def owner_page(self, origin):
        base = self.site.url if origin == "same" else self.site.other_origin
        return base("/wsowner.html?u=" + quote(self.url, safe=""))

    def setup(self):
        """The script that makes the owner open the socket."""
        if self.owner == "document":
            twin = "window.sock2 = new WebSocket(%s);" % json.dumps(self.url) if self.cause == "twin-closed" else ""
            return ("window.sock = new WebSocket(%s); %s"
                    "sock.onopen = () => { sock.send('hi'); postMessage('open', '*') };"
                    "sock.onclose = () => postMessage('closed', '*');" % (json.dumps(self.url), twin))
        if self.owner == "worker":
            return "window.wk = new Worker('/wsworker.js?u=' + encodeURIComponent(%s));" % json.dumps(self.url)
        if self.owner == "popup":
            return "window.pop = window.open(%s);" % json.dumps(self.owner_page("same"))
        src = {
            "iframe-same-origin": self.owner_page("same"),
            "iframe-cross-origin": self.owner_page("other"),
            "iframe-nested": self.site.url("/wsowner.html?child=" + quote(self.owner_page("other"), safe="")),
        }[self.owner]
        return "const f = document.createElement('iframe'); f.id = 'o'; f.src = %s; document.body.append(f);" % json.dumps(src)

    def open(self):
        self.server.call("navigate", {"url": self.site.url("/basic.html")})
        word = json.dumps("closed" if self.cause in NEVER_OPEN else "open")
        # A popup needs a user's gesture past the popup blocker.
        self.js(self.setup() + WAIT % {"word": word}, gesture=self.owner == "popup")
        if self.owner == "popup":
            tabs = json.loads(self.server.call("list_tabs", {}).text)["tabs"]
            self.tab = next(t["tab_id"] for t in tabs if "wsowner" in t["url"])
        time.sleep(0.5)

    def end(self):
        message = {"close": "close", "server-close": "send:bye", "tcp-cut": "send:cut"}.get(self.cause)
        if self.cause in NEVER_OPEN:
            return
        if message is not None:
            self.command(message)
        elif self.cause == "twin-closed":
            self.js("sock.close(); 1")
        elif self.cause == "reload":
            self.server.call("reload", {})
        elif self.owner in ("document", "popup"):
            target = self.site.url("/second.html") if self.cause == "nav-same-origin" else self.site.other_origin("/basic.html")
            self.server.call("navigate", {"url": target, **({"tab_id": self.tab} if self.tab else {})})
        elif self.cause == "terminated":
            self.js("wk.terminate(); 1")
        elif self.cause == "removed":
            self.js("document.getElementById('o').remove(); 1")
        else:
            same = self.site.url if self.owner == "iframe-same-origin" else self.site.other_origin
            other = self.site.other_origin if self.owner == "iframe-same-origin" else self.site.url
            target = (same if self.cause == "nav-same-origin" else other)("/basic.html")
            self.js("document.getElementById('o').src = %s; 1" % json.dumps(target))

    def command(self, message):
        post = POST[self.owner]
        if self.owner == "document":
            call = ".close()" if message == "close" else ".send(%s)" % json.dumps(message[5:])
            self.js(post.format(call=call) + "; 1")
        else:
            self.js(post.format(msg=json.dumps(message)) + "; 1")

    def logs(self):
        args = {"type_filter": "websocket", **({"tab_id": self.tab} if self.tab else {})}
        return self.server.call("get_network_logs", args).text

    def closes(self):
        """The close lines of the cell's socket, once one came (at most 8 s)
        and another second passed for a second one."""
        pattern = re.compile(r"^\S+ (closed|failed) %s(:.*)?$" % re.escape(self.url), re.M)
        for _ in range(16):
            if pattern.search(self.logs()):
                time.sleep(1)
                break
            time.sleep(0.5)
        return [m.group(0) for m in pattern.finditer(self.logs())]

    def close_popup(self):
        if self.tab:
            self.server.call("close_tab", {"tab_id": self.tab})


def run_cell(server, site, port, owner, cause):
    cell = Cell(server, site, port, owner, cause)
    try:
        cell.open()
        cell.end()
        lines = cell.closes()
        verdict = "PASS" if len(lines) == 1 else "FAIL"
        detail = lines[0].split(" ", 1)[1] if len(lines) == 1 else "%d close lines" % len(lines)
    except Exception as error:  # a cell that cannot run is a failure
        verdict, detail = "FAIL", str(error)
    finally:
        cell.close_popup()
    return verdict, detail.replace(cell.url, "<url>")


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
    failures = cells = 0
    try:
        for owner, causes in OWNERS.items():
            for cause in causes:
                if args.only and args.only != "%s/%s" % (owner, cause):
                    continue
                verdict, detail = run_cell(server, site, port, owner, cause)
                cells += 1
                failures += verdict == "FAIL"
                print("%s %-20s %-22s %s" % (verdict, owner, cause, detail), flush=True)
    finally:
        server.close()
        site.stop()
    print("WebSocket matrix (%s): %d cells, %d failure(s)" % (args.browser, cells, failures))
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
