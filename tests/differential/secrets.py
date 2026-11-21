"""--secrets end to end on one browser: the page writes a secret through the
browser's own encoders (JSON, URL, `escape()`, HTML serialization) into
every place a tool reads (an evaluation's value or error, the console, the
DOM, the snapshot, a request's URL, headers and bodies, storage, the tab's
URL), and no reply may show it, nor any piece of it.

The secret holds the characters the encoders change around two plain
fragments, `Qx9` and `Zk7731`, which every encoding here leaves as they
are: a reply showing either shows the secret, encoded or not.

Usage (from the repository root):
    python3 -I tests/differential/secrets.py [--browser chrome|firefox]
        [--bin target/debug/browser-control-mcp]
Exits 1 when a reply shows a fragment, or lacks the token where the secret
was.
"""

import argparse
import os
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(os.path.dirname(HERE))
sys.path.insert(0, os.path.join(ROOT, "tests", "compare"))

from mcp_client import Server  # noqa: E402
from siteserver import Site  # noqa: E402

SECRET = "Qx9\"w&<\u00e9> Zk7731/+%'\\"
FRAGMENTS = ["Qx9", "Zk7731"]
TOKEN = "<secret:PW>"

# The page's value, without writing it in the expression (the expression is
# not a reply, but keep the check honest): built from its pieces.
V = "['Qx', '9\"w&<\\u00e9> Zk', '7731/+%\\'\\\\'].join('')"

# Expressions whose value carries the secret, each through an encoder.
EVALUATIONS = {
    "as is": V,
    "JSON.stringify": "JSON.stringify({p: %s})" % V,
    "JSON x2": "JSON.stringify(JSON.stringify({p: %s}))" % V,
    "JSON x3": "JSON.stringify(JSON.stringify(JSON.stringify({p: %s})))" % V,
    "JSON x4": "JSON.stringify(JSON.stringify(JSON.stringify(JSON.stringify({p: %s}))))" % V,
    "encodeURIComponent": "encodeURIComponent(%s)" % V,
    "encodeURI": "encodeURI(%s)" % V,
    "escape()": "escape(%s)" % V,
    "URLSearchParams": "new URLSearchParams({p: %s, q: 'x y'}).toString()" % V,
    "URL of JSON": "encodeURIComponent(JSON.stringify({p: %s}))" % V,
    "URL twice": "encodeURIComponent(encodeURIComponent(%s))" % V,
    "new URL search": "new URL('http://a/?p=' + encodeURIComponent(%s)).href" % V,
    "innerHTML text": "(d => { d.textContent = %s; return d.innerHTML })(document.createElement('div'))" % V,
    "outerHTML attribute": "(d => { d.setAttribute('t', %s); return d.outerHTML })(document.createElement('div'))" % V,
    "attribute of JSON": "(d => { d.setAttribute('t', JSON.stringify({p: %s})); return d.outerHTML })(document.createElement('div'))" % V,
    "attribute of JSON x2": "(d => { d.setAttribute('t', JSON.stringify(JSON.stringify({p: %s}))); return d.outerHTML })(document.createElement('div'))" % V,
    "object": "({p: %s, list: [%s]})" % (V, V),
    "Map preview": "new Map([['p', %s]])" % V,
    "Error object": "new Error(%s)" % V,
    "function source": "new Function('return ' + JSON.stringify(%s))" % V,
}

SETUP = """
const v = %s;
const d = document.createElement('div');
d.id = 'qa-secret'; d.textContent = v; d.setAttribute('data-v', v);
d.setAttribute('data-json', JSON.stringify({p: v}));
d.setAttribute('aria-label', v);
document.body.append(d);
const input = document.createElement('input'); input.id = 'qa-input'; input.value = v; document.body.append(input);
document.title = v;
localStorage.setItem('qa', v); sessionStorage.setItem('qa', JSON.stringify({p: v}));
document.cookie = 'qa=' + encodeURIComponent(v) + '; path=/';
console.log(v); console.log({p: v}); console.warn(JSON.stringify({p: v})); console.error(encodeURIComponent(v));
await fetch('/api/echo?net=1&p=' + encodeURIComponent(v), {headers: {'X-Api-Key': v}});
await fetch('/api/echo?net=2', {method: 'POST', headers: {'Content-Type': 'application/x-www-form-urlencoded'},
  body: new URLSearchParams({p: v}).toString()});
'ok'
""" % V

# Tool calls that read what the page wrote: (name, tool, arguments, whether
# the token must be in the reply).
READS = [
    ("thrown error", "evaluate_js", {"expression": "throw new Error(%s)" % V}, True),
    ("console", "get_console_logs", {}, True),
    ("console details", "get_console_logs", {"include_details": True}, True),
    # A window wide enough for the whole element: only whole values are masked.
    ("inspect_dom", "inspect_dom", {"query": "qa-secret", "after": 2000}, True),
    ("snapshot", "snapshot", {}, True),
    ("find_in_snapshot", "find_in_snapshot", {"text": "qa"}, False),
    ("network list", "get_network_logs", {"url_filter": "net="}, True),
    ("network details", "get_network_logs", {"url_filter": "net=", "include_details": True}, True),
    ("network every value", "get_network_logs", {"url_filter": "net=", "include_details": True,
                                                  "include_sensitive_headers": True}, True),
    ("localStorage", "web_storage", {"action": "get", "kind": "local", "show_values": True}, True),
    ("sessionStorage", "web_storage", {"action": "get", "kind": "session", "show_values": True}, True),
    ("cookies", "cookies", {"action": "get", "show_values": True}, True),
]


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--browser", default="chrome", choices=["chrome", "firefox"])
    parser.add_argument("--bin", default=os.path.join(ROOT, "target", "debug", "browser-control-mcp"))
    parser.add_argument("--visible", action="store_true")
    args = parser.parse_args()

    secrets = tempfile.NamedTemporaryFile("w", suffix=".env", delete=False)
    secrets.write('PW="%s"\n' % SECRET)
    secrets.close()
    command = [args.bin, "--browser", args.browser, "--secrets", secrets.name, "--tool-groups", "storage"]
    command += [] if args.visible else ["--headless"]
    site = Site()
    server = Server(command, timeout=120)
    wrong, checked = [], 0
    try:
        server.call("navigate", {"url": site.url("/basic.html")})
        reply = server.call("evaluate_js", {"expression": SETUP})
        if reply.is_error:
            raise RuntimeError("setup: " + reply.text[:300])
        time.sleep(1)
        calls = [(name, "evaluate_js", {"expression": e}, True) for name, e in EVALUATIONS.items()] + READS
        nav = site.url("/basic.html?p=") + "Qx9%22w%26%3C%C3%A9%3E%20Zk7731%2F%2B%25'%5C"
        calls.append(("navigate (URL)", "navigate", {"url": nav}, True))
        calls.append(("list_tabs (URL)", "list_tabs", {}, True))
        for name, tool, arguments, token in calls:
            checked += 1
            text = server.call(tool, arguments).text
            shown = [f for f in FRAGMENTS if f in text]
            if shown:
                line = next(l for l in text.splitlines() if any(f in l for f in shown))
                wrong.append("%s: shows %s in %s" % (name, shown, line.strip()[:160]))
            elif token and TOKEN not in text:
                wrong.append("%s: no %s in %s" % (name, TOKEN, " ".join(text.split())[:160]))
    finally:
        server.close()
        site.stop()
        os.unlink(secrets.name)
    for line in wrong:
        print("  " + line)
    print("secret masking (%s): %d replies checked, %d wrong" % (args.browser, checked, len(wrong)))
    return 1 if wrong else 0


if __name__ == "__main__":
    sys.exit(main())
