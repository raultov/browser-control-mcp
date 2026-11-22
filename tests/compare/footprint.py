#!/usr/bin/env python3
"""Measures the README's "Advantages of a native Rust binary": how long each
server takes to answer MCP `initialize`, how much memory its own processes
use (the browsers they start are left out), and how long the first
navigation takes (it launches Chrome). Chrome, headless; median of the runs.

    make build && python3 -I tests/compare/footprint.py              # bc, pw, cd
    python3 -I tests/compare/footprint.py --servers bc --runs 10

`pw` and `cd` are started with `npx`, as their READMEs show (npm stays
running as their parent: its memory is reported apart), and then with
`node` on the package npx installed, without npm. Not a gate: it starts
real browsers and fetches the other servers with npx. Linux only (/proc).
"""

import argparse
import json
import os
import shutil
import statistics
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

from adapters import BrowserControl, DevTools, Playwright  # noqa: E402
from run import free_port  # noqa: E402
from siteserver import Site  # noqa: E402

REPO = os.path.dirname(os.path.dirname(HERE))
PLAYWRIGHT = "@playwright/mcp@0.0.86"
DEVTOOLS = "chrome-devtools-mcp@1.13.0"

# Browser processes: left out of a server's memory.
BROWSERS = ("chrome", "chromium", "headless_shell", "crashpad", "firefox")


def npx_bin(spec, name):
    """The `name` executable of `spec` in npx's cache (run once with npx first)."""
    root = os.path.expanduser("~/.npm/_npx")
    for entry in sorted(os.listdir(root)) if os.path.isdir(root) else []:
        try:
            with open(os.path.join(root, entry, "package.json")) as f:
                packages = json.load(f).get("_npx", {}).get("packages", [])
        except (OSError, ValueError):
            continue
        path = os.path.join(root, entry, "node_modules", ".bin", name)
        if spec in packages and os.path.exists(path):
            return path
    return None


def commands(bin_path, output):
    """name -> (adapter class, command, npx spec and bin to run with node)."""
    npx = shutil.which("npx") or "npx"
    pw = ["--headless", "--isolated", "--browser", "chrome", "--output-dir", output]
    cd = ["--headless", "--isolated", "--usageStatistics=false", "--performanceCrux=false"]
    return {
        "bc": (BrowserControl, [bin_path, "--headless", "--port", str(free_port())], None),
        "pw": (Playwright, [npx, "-y", PLAYWRIGHT] + pw, (PLAYWRIGHT, "playwright-mcp", pw)),
        "cd": (DevTools, [npx, "-y", DEVTOOLS] + cd, (DEVTOOLS, "chrome-devtools-mcp", cd)),
    }


def session_processes(sid):
    """(command line, RSS in KB) of the processes of session `sid`."""
    found = []
    for pid in filter(str.isdigit, os.listdir("/proc")):
        try:
            with open(f"/proc/{pid}/stat") as f:
                stat = f.read()
            if int(stat[stat.rindex(")") + 2:].split()[3]) != sid:
                continue
            with open(f"/proc/{pid}/cmdline", "rb") as f:
                cmd = f.read().replace(b"\0", b" ").decode(errors="replace").strip()
            with open(f"/proc/{pid}/status") as f:
                rss = next(int(line.split()[1]) for line in f if line.startswith("VmRSS:"))
            found.append((cmd, rss))
        except (OSError, ValueError, StopIteration, IndexError):
            continue
    return found


def memory(sid):
    """MB of the server's own process, and of npm when npx started it."""
    def program(cmd):
        return os.path.basename(cmd.split(" ")[0]).lower()

    def is_browser(cmd):
        # chrome-devtools-mcp names its process after itself.
        return program(cmd) != "chrome-devtools-mcp" and any(b in program(cmd) for b in BROWSERS)

    def is_npm(cmd):
        return "npm exec" in cmd or "/npx-cli.js" in cmd or program(cmd) == "npx"

    own = [(cmd, rss) for cmd, rss in session_processes(sid) if not is_browser(cmd)]
    server = max((rss for cmd, rss in own if not is_npm(cmd)), default=0)
    npm = sum(rss for cmd, rss in own if is_npm(cmd))
    return server / 1024, npm / 1024


def measure(adapter_class, command, url, runs):
    """Medians: start (ms), first navigation (ms), memory idle and after
    four navigations (MB), npm's memory (MB)."""
    adapter_class(command, dict(os.environ)).close()  # npx fetches the package once
    rows = []
    for _ in range(runs):
        started = time.time()
        adapter = adapter_class(command, dict(os.environ))
        start = (time.time() - started) * 1000
        session = adapter.server.proc.pid  # its own session (mcp_client)
        try:
            time.sleep(0.5)
            idle, _ = memory(session)
            started = time.time()
            adapter.goto(url)
            first = (time.time() - started) * 1000
            for _ in range(3):
                adapter.goto(url)
            time.sleep(0.5)
            busy, npm = memory(session)
        finally:
            adapter.close()
        rows.append((start, first, idle, busy, npm))
    return [statistics.median(column) for column in zip(*rows)]


def report(label, medians):
    start, first, idle, busy, npm = medians
    print(f"{label}: initialize {start:.0f} ms | first navigation {first:.0f} ms | "
          f"server memory {idle:.0f} MB idle, {busy:.0f} MB after 4 navigations | "
          f"npm {npm:.0f} MB", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--servers", default="bc,pw,cd")
    parser.add_argument("--runs", type=int, default=5)
    parser.add_argument("--bc-bin", default=os.path.join(REPO, "target", "release", "browser-control-mcp"))
    args = parser.parse_args()
    site = Site()
    output = tempfile.mkdtemp(prefix="footprint-")
    url = site.url("/basic.html")
    try:
        for name in args.servers.split(","):
            adapter_class, command, direct = commands(args.bc_bin, output)[name]
            report(name if direct is None else f"{name} (npx)",
                   measure(adapter_class, command, url, args.runs))
            # The npx run put the package in npx's cache: run it without npm.
            path = direct and npx_bin(direct[0], direct[1])
            if path:
                command = ["node", path] + direct[2]
                report(f"{name} (node)", measure(adapter_class, command, url, args.runs))
    finally:
        site.stop()


if __name__ == "__main__":
    main()
