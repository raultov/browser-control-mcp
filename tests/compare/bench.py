#!/usr/bin/env python3
"""Measures the README's comparison table ("Measured"): the context cost of
each server's tool list, snapshots of real pages, a login by steps and with
one form call, the latency of a click, an evaluation and a navigation, and
how each server handles a very long page, an unknown host, the console and
the network log. Chrome, headless; tokens as `mcp_client` counts them.

    python3 -I tests/compare/bench.py                     # bc, pw, cd
    python3 -I tests/compare/bench.py --servers bc --runs 5

Not a gate: it starts real browsers, fetches the other servers with npx and
reads live pages (Hacker News, Wikipedia, MDN), whose size changes.
"""

import argparse
import base64
import json
import os
import shutil
import statistics
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

from adapters import BrowserControl, DevTools, Playwright, T  # noqa: E402
from run import DEVTOOLS, PLAYWRIGHT, free_port  # noqa: E402
from siteserver import Site  # noqa: E402

REPO = os.path.dirname(os.path.dirname(HERE))

REAL_PAGES = [
    ("Hacker News", "https://news.ycombinator.com/"),
    ("Wikipedia \"Web browser\"", "https://en.wikipedia.org/wiki/Web_browser"),
    ("MDN `<input>`", "https://developer.mozilla.org/en-US/docs/Web/HTML/Reference/Elements/input"),
]


def commands(bin_path, output):
    """name -> (adapter class, default command, command with every opt-in tool)."""
    npx = shutil.which("npx") or "npx"
    bc = [bin_path, "--headless", "--port", str(free_port())]
    pw = [npx, "-y", PLAYWRIGHT, "--headless", "--isolated", "--browser", "chrome",
          "--output-dir", os.path.join(output, "playwright")]
    cd = [npx, "-y", DEVTOOLS, "--headless", "--isolated", "--usageStatistics=false", "--performanceCrux=false"]
    return {
        "bc": (BrowserControl, bc, bc + ["--tool-groups", "all"]),
        "pw": (Playwright, pw, pw + ["--caps", "network,storage,devtools,vision,pdf,testing,config"]),
        "cd": (DevTools, cd, None),
    }


def tool_tokens(adapter_class, command):
    """Tokens of the tool list a server puts in the model's context."""
    adapter = adapter_class(command, dict(os.environ))
    try:
        return len(json.dumps(adapter.server.tools())) // 4
    finally:
        adapter.close()


def counted(adapter, steps):
    """Runs `steps(adapter)` and returns (calls, tokens, ms) of what it called."""
    adapter.calls = []
    start = time.time()
    steps(adapter)
    ms = int((time.time() - start) * 1000)
    return len(adapter.calls), sum(c.tokens for c in adapter.calls), ms


def login_steps(site):
    def run(a):
        a.goto(site.url("/login.html"))
        snap = a.snap().text if a.snapshot_pattern else None
        find = lambda role, name, css: a.find(T(role, name, css), snap)
        a.fill(find("textbox", "Username", "#user"), "grace")
        a.fill(find("textbox", "Password", "#pass"), "s3cret")
        a.select(find("combobox", "Role", "#role"), "Admin", "admin")
        a.click(find("checkbox", "Remember me", "#remember"))
        a.click(find("button", "Sign in", "button[type=submit]"))
        a.wait_text("Welcome, grace")
    return run


def login_form(site):
    def run(a):
        a.goto(site.url("/login.html"))
        snap = a.snap().text if a.snapshot_pattern else None
        find = lambda role, name, css: a.find(T(role, name, css), snap)
        a.form([(find("textbox", "Username", "#user"), "grace", "textbox"),
                (find("textbox", "Password", "#pass"), "s3cret", "textbox"),
                (find("combobox", "Role", "#role"), "Admin", "combobox"),
                (find("checkbox", "Remember me", "#remember"), "true", "checkbox")])
        a.click(find("button", "Sign in", "button[type=submit]"))
        a.wait_text("Welcome, grace")
    return run


def check_login(a):
    seen = a.value("[document.getElementById('out').textContent, document.getElementById('role').value, document.getElementById('remember').checked]")
    return seen == ["Welcome, grace", "admin", True], seen


def median_ms(adapter, action, runs):
    times = []
    for _ in range(runs):
        adapter.calls = []
        action()
        times.append(adapter.calls[-1].ms)
    return int(statistics.median(times))


def raw_call(adapter, tool, args):
    """A tool call read whole: (is_error, text, image bytes)."""
    msg = adapter.server.request("tools/call", {"name": tool, "arguments": args}, 120)
    result = msg.get("result", {})
    text = " ".join(b.get("text", "") for b in result.get("content", []) if b.get("type") == "text")
    images = [len(base64.b64decode(b.get("data", ""))) for b in result.get("content", []) if b.get("type") == "image"]
    error = bool(result.get("isError")) or "error" in msg
    return error, text.strip(), images


def full_shot(adapter, name, site):
    adapter.goto(site.url("/long.html"))
    if name == "bc":
        return raw_call(adapter, "capture_screenshot", {"full_page": True})
    if name == "pw":
        return raw_call(adapter, "browser_take_screenshot", {"type": "png", "fullPage": True})
    return raw_call(adapter, "take_screenshot", {"pageId": adapter.page, "fullPage": True})


def unknown_host(adapter, name):
    url = "http://nonexistent-host.invalid/"
    if name == "bc":
        return raw_call(adapter, "navigate", {"url": url})
    if name == "pw":
        return raw_call(adapter, "browser_navigate", {"url": url})
    return raw_call(adapter, "navigate_page", {"pageId": adapter.page, "type": "url", "url": url})


def measure(name, adapter, site, runs):
    """Every row for one server, as a dict."""
    out = {}
    for label, url in REAL_PAGES:
        adapter.goto(url)
        time.sleep(1)
        out[f"snapshot {label}"] = adapter.snap().tokens
    for key, steps in (("login steps", login_steps(site)), ("login form", login_form(site))):
        calls, tokens, ms = counted(adapter, steps)
        ok, seen = check_login(adapter)
        out[key] = {"calls": calls, "tokens": tokens, "ms": ms, "ok": ok, "seen": seen}
    adapter.goto(site.url("/basic.html"))
    button = adapter.find(T("button", "Count", "#count"))
    out["click ms"] = median_ms(adapter, lambda: adapter.click(button), runs)
    out["eval ms"] = median_ms(adapter, lambda: adapter.eval_raw("1 + 1"), runs)
    second = site.url("/second.html")
    out["navigate ms"] = median_ms(adapter, lambda: adapter.goto(second), runs)
    adapter.calls = []
    adapter.goto(site.url("/console.html"))
    time.sleep(0.5)
    adapter.console()
    out["console tokens"] = sum(c.tokens for c in adapter.calls)
    out["console text"] = adapter.calls[-1].text[:600]
    adapter.calls = []
    adapter.goto(site.url("/requests.html"))
    time.sleep(0.5)
    adapter.network()
    out["network tokens"] = sum(c.tokens for c in adapter.calls)
    out["network text"] = adapter.calls[-1].text[:600]
    # Last: a failed navigation leaves an error page behind.
    error, text, images = full_shot(adapter, name, site)
    out["full shot"] = {"error": error, "text": text[:300], "image_bytes": images}
    error, text, _ = unknown_host(adapter, name)
    out["unknown host"] = {"error": error, "text": text[:200]}
    return out


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--servers", default="bc,pw,cd")
    parser.add_argument("--runs", type=int, default=10, help="calls per latency median")
    parser.add_argument("--bc-bin", default=os.path.join(REPO, "target", "release", "browser-control-mcp"))
    args = parser.parse_args()
    output = tempfile.mkdtemp(prefix="bench-")
    site = Site()
    results = {}
    try:
        for name in args.servers.split(","):
            adapter_class, default, every = commands(args.bc_bin, output)[name]
            row = {"tools default": tool_tokens(adapter_class, default)}
            if every:
                row["tools all"] = tool_tokens(adapter_class, every)
            adapter = adapter_class(every or default, dict(os.environ))
            adapter.server.timeout = 120
            try:
                row["version"] = adapter.server.info.get("version")
                row.update(measure(name, adapter, site, args.runs))
            finally:
                adapter.close()
            results[name] = row
            print(name, json.dumps(row, indent=1, ensure_ascii=False), flush=True)
    finally:
        site.stop()
    path = os.path.join(output, "bench.json")
    with open(path, "w") as f:
        json.dump(results, f, indent=1, ensure_ascii=False)
    print("saved", path)


if __name__ == "__main__":
    main()
