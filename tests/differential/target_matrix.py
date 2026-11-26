"""Every per-tab feature in every kind of target a tab has, on one browser.

A tab is not one document: its frames (of its origin and of another one,
which Chrome runs in a process of its own), its dedicated workers and the
popups it opens are each a target of their own, which each browser reports
apart. A feature built and tested on the document misses the others: the
issues about network logs and mocked routes were mostly such cells
(#75, #141, #163, #167, #172, #173, #178, #185, #186, #199, #200), each
found one at a time. This runs the table at once.

Owners: the document, a same-origin iframe, a cross-origin iframe, a
cross-origin iframe inside a same-origin one, a dedicated worker, a worker
that worker started (#217), a worker of a cross-origin iframe, a popup (its
own tab).
Features: a console message, an uncaught error with its stack, each also
from a script the owner imported (a <script src>, importScripts: #207), a
thrown value that is not an error, listed with the value (#219), the same
error thrown 30 times at once, each listed once and oldest first (#218), a
message the owner logs while it loads, right after the tab moved to another
site (#206), a request in the network log, a redirect hop with its status
and Location, a mocked request answered by a route of the tab, and one a
route fails (#212). Each must be listed exactly once: a cell found by
presence alone missed the duplicates (#207). WebSockets have their own
table (ws_matrix.py).

Usage (from the repository root):
    python3 -I tests/differential/target_matrix.py [--browser chrome|firefox]
        [--bin target/debug/browser-control-mcp] [--only owner/feature]
Exits 1 when a cell fails.
"""

import argparse
import json
import os
import sys
import time
from urllib.parse import quote

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(os.path.dirname(HERE))
sys.path.insert(0, os.path.join(ROOT, "tests", "compare"))

from mcp_client import Server  # noqa: E402
from siteserver import Site  # noqa: E402

OWNERS = ["document", "iframe-same-origin", "iframe-cross-origin", "iframe-nested",
          "worker", "worker-nested", "worker-in-cross-iframe", "popup"]
FEATURES = ["console", "console-imported", "uncaught", "uncaught-imported", "thrown-value", "burst",
            "load", "request", "redirect", "mock", "abort"]

# How many times the `burst` cell throws its error.
BURST = 30

# The page's worker parameter of the owners that are one.
WORKERS = {"worker": "?worker=1", "worker-nested": "?worker=nested"}

# Waits for the owner's answer to the command of `token`.
WAIT = """
await new Promise((resolve, reject) => {
  const done = e => { const t = String(e.data); if (t.startsWith('done:%(token)s:')) resolve(t.slice(%(skip)d)); };
  addEventListener('message', done);
  setTimeout(() => reject(new Error('no answer from the owner')), 10000);
})"""


class Cell:
    def __init__(self, server, site, owner, feature, n):
        self.server, self.site, self.owner, self.feature = server, site, owner, feature
        self.token = "t%d%s" % (n, feature[:3])
        self.tab = None

    def call(self, tool, args):
        if self.tab:
            args = {**args, "tab_id": self.tab}
        return self.server.call(tool, args)

    def js(self, expression, gesture=False):
        reply = self.server.call("evaluate_js", {"expression": expression, "user_gesture": gesture})
        if reply.is_error:
            raise RuntimeError("evaluate_js: " + reply.text.strip()[:200])
        return json.loads(reply.text[reply.text.index("{"):]).get("value")

    def owner_page(self, other_origin, query=""):
        base = self.site.other_origin if other_origin else self.site.url
        return base("/probe.html" + query)

    def own_query(self, query=""):
        """`query` with the owner's `onload` token, for the `load` cell."""
        if self.feature != "load":
            return query
        return query + ("&" if query else "?") + "onload=" + self.token

    def open(self):
        """Loads the owner; the expression that posts a command to it. For
        `load`, the tab first shows another site: the owner's page is the
        first of its site."""
        if self.feature == "load":
            self.server.call("navigate", {"url": self.site.other_origin("/basic.html")})
        top = "/probe.html" + (self.own_query(WORKERS[self.owner]) if self.owner in WORKERS else
                               self.own_query() if self.owner == "document" else "")
        self.server.call("navigate", {"url": self.site.url(top)})
        frame = {
            "iframe-same-origin": self.owner_page(False, self.own_query()),
            "iframe-cross-origin": self.owner_page(True, self.own_query()),
            "iframe-nested": self.owner_page(False, "?child=" + quote(self.owner_page(True, self.own_query()), safe="")),
            "worker-in-cross-iframe": self.owner_page(True, self.own_query("?worker=1")),
        }.get(self.owner)
        if frame:
            self.js("await new Promise(r => { const f = document.createElement('iframe'); f.id = 'o'; f.onload = r;"
                    " f.src = %s; document.body.append(f); }); await new Promise(r => setTimeout(r, 500)); 1"
                    % json.dumps(frame))
            return "document.getElementById('o').contentWindow"
        if self.owner == "popup":
            self.js("window.pop = window.open(%s); await new Promise(r => setTimeout(r, 1000)); 1"
                    % json.dumps(self.owner_page(False, self.own_query())), gesture=True)
            listed = self.server.call("list_tabs", {}).text
            listed = json.loads(listed[listed.index("{"):])
            self.tab = next(t["tab_id"] for t in listed["tabs"] if t["tab_id"] != listed["active_tab_id"])
            return "window.pop"
        time.sleep(0.5 if self.owner in WORKERS else 0)
        return "window"

    def command(self, target, command):
        prefix = "done:%s:" % self.token
        return self.js("%s.postMessage(%s, '*'); %s" % (target, json.dumps(command), WAIT % {
            "token": self.token, "skip": len(prefix)}))

    def until(self, tool, args, check, seconds=6):
        text = ""
        for _ in range(int(seconds / 0.5)):
            text = self.call(tool, args).text
            if check(text):
                return True, text
            time.sleep(0.5)
        return False, text

    def once(self, tool, args, check, seconds=6):
        """`until` the reply holds what `check` wants, then once more a
        moment later: a copy coming late from another feed shows then."""
        ok, text = self.until(tool, args, check, seconds)
        if not ok:
            return ok, text
        time.sleep(1)
        text = self.call(tool, args).text
        return check(text), text

    def logged(self, words, stacked=False, level=None):
        """A check that the console lists one message with `words` (with a
        stack, at `level`), read from its details: a duplicate is no
        repeat of the same line."""
        def check(text):
            if "{" not in text:
                return False
            found = [m for m in json.loads(text[text.index("{"):]).get("messages", [])
                     if words in m.get("text", "")]
            return (len(found) == 1 and (not stacked or "\n    at " in found[0]["text"])
                    and (level is None or found[0].get("level") == level))
        return check

    def burst(self, words):
        """A check that the console lists `BURST` messages with `words`,
        none before an older one (by their own timestamps)."""
        def check(text):
            if "{" not in text:
                return False
            stamps = [m["timestamp"] for m in json.loads(text[text.index("{"):]).get("messages", [])
                      if words in m.get("text", "")]
            return len(stamps) == BURST and stamps == sorted(stamps)
        return check

    def requested(self, token, status=None, failed=False):
        """A check that the network log lists one request with `token`
        (answered `status`, or failed)."""
        def check(text):
            if "{" not in text:
                return False
            found = [r for r in json.loads(text[text.index("{"):]).get("requests", {}).values()
                     if token in r.get("url", "")]
            if len(found) != 1:
                return False
            return bool(found[0].get("error")) if failed else found[0].get("response_status") == status
        return check

    def run(self):
        target = self.open()
        token = self.token
        details = {"include_details": True}
        if self.feature in ("console", "console-imported"):
            imported = self.feature == "console-imported"
            self.command(target, ("ilog:" if imported else "log:") + token)
            words = ("probe-lib-log " if imported else "probe-log ") + token
            return self.once("get_console_logs", details, self.logged(words, level="log"))
        if self.feature == "load":
            return self.once("get_console_logs", details, self.logged("probe-onload " + token))
        if self.feature in ("uncaught", "uncaught-imported"):
            imported = self.feature == "uncaught-imported"
            self.command(target, ("ithrow:" if imported else "throw:") + token)
            words = ("probe-lib-throw " if imported else "probe-throw ") + token
            return self.once("get_console_logs", details, self.logged(words, stacked=True))
        if self.feature == "thrown-value":
            self.command(target, "throwvalue:" + token)
            return self.once("get_console_logs", details, self.logged("probe-value " + token, level="error"))
        if self.feature == "burst":
            self.command(target, "burst:%s:%d" % (token, BURST))
            return self.once("get_console_logs", {"limit": 1000, **details}, self.burst("probe-burst " + token))
        if self.feature == "request":
            self.command(target, "fetch:%s:/api/echo?probe=%s" % (token, token))
            return self.once("get_network_logs", {"url_filter": token, **details}, self.requested(token, 200))
        if self.feature == "redirect":
            to = quote("/api/echo?probe=%s" % token, safe="")
            self.command(target, "fetch:%s:/redirect?to=%s" % (token, to))

            def hop(text):
                if "{" not in text:
                    return False
                requests = json.loads(text[text.index("{"):]).get("requests", {})
                hops = [h for r in requests.values() for h in r.get("redirects", [])]
                return any(h.get("status") == 302 and h.get("location") for h in hops)
            return self.until("get_network_logs", {"url_filter": token, "include_details": True}, hop)
        if self.feature == "mock":
            self.call("mock_route", {"url_pattern": "*mocked=%s*" % token, "body": "MOCKED"})
            answer = self.command(target, "fetch:%s:/api/echo?mocked=%s" % (token, token))
            if answer != "200 MOCKED":
                return False, answer
            return self.once("get_network_logs", {"url_filter": token, **details}, self.requested(token, 200))
        if self.feature == "abort":
            self.call("mock_route", {"url_pattern": "*aborted=%s*" % token, "abort": True})
            answer = self.command(target, "fetch:%s:/api/echo?aborted=%s" % (token, token))
            if not answer.startswith("failed TypeError"):
                return False, answer
            return self.once("get_network_logs", {"url_filter": token, **details},
                             self.requested(token, failed=True))
        raise ValueError(self.feature)

    def close(self):
        if self.feature in ("mock", "abort"):
            self.call("remove_route", {})
        if self.tab:
            self.server.call("close_tab", {"tab_id": self.tab})


def run_cell(server, site, owner, feature, n):
    cell = Cell(server, site, owner, feature, n)
    try:
        ok, text = cell.run()
        detail = "" if ok else text.strip().replace("\n", " | ")[:220]
    except Exception as error:  # a cell that cannot run is a failure
        ok, detail = False, str(error)[:220]
    finally:
        try:
            cell.close()
        except Exception:
            pass
    return ("PASS" if ok else "FAIL"), detail


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--browser", default="chrome", choices=["chrome", "firefox"])
    parser.add_argument("--bin", default=os.path.join(ROOT, "target", "debug", "browser-control-mcp"))
    parser.add_argument("--only", help="owner/feature")
    parser.add_argument("--visible", action="store_true")
    args = parser.parse_args()

    command = [args.bin, "--browser", args.browser, "--tool-groups", "network"]
    command += [] if args.visible else ["--headless"]
    site = Site()
    server = Server(command, timeout=120)
    failures = cells = 0
    try:
        for owner in OWNERS:
            for feature in FEATURES:
                if args.only and args.only != "%s/%s" % (owner, feature):
                    continue
                cells += 1
                verdict, detail = run_cell(server, site, owner, feature, cells)
                failures += verdict == "FAIL"
                print("%s %-24s %-9s %s" % (verdict, owner, feature, detail), flush=True)
    finally:
        server.close()
        site.stop()
    print("Target matrix (%s): %d cells, %d failure(s)" % (args.browser, cells, failures))
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
