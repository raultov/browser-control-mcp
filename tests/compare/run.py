#!/usr/bin/env python3
"""Runs the scenarios on browser MCP servers and reports, per scenario and
server, whether the task worked, and the calls, time and tokens it took.

    python3 tests/compare/run.py                      # every server
    python3 tests/compare/run.py --servers bc,pw      # some of them
    python3 tests/compare/run.py --only 'frames|shadow' --level 2
    python3 tests/compare/run.py --save-baseline      # after a run you trust

See tests/compare/README.md.
"""

import argparse
import datetime
import json
import os
import re
import shutil
import socket
import sys
import time
import traceback

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

from adapters import Adapter, BrowserControl, DevTools, Fail, Playwright, Puppeteer, Unsupported  # noqa: E402
from scenarios import SCENARIOS  # noqa: E402
from siteserver import Site  # noqa: E402

REPO = os.path.dirname(os.path.dirname(HERE))
RESULTS = os.path.join(HERE, "results")
BASELINE = os.path.join(HERE, "baseline.json")

# The other servers, pinned: a new version is a deliberate change here.
PLAYWRIGHT = "@playwright/mcp@0.0.84"
DEVTOOLS = "chrome-devtools-mcp@1.12.0"
PUPPETEER = "@hisma/server-puppeteer@0.6.5"

MARK = {"pass": "✓", "fail": "✗", "unsupported": "–", "error": "E", None: " "}


def free_port():
    """A TCP port nothing listens on now, for a browser of its own."""
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        return probe.getsockname()[1]


def servers(args, output):
    """name -> (adapter class, command, env, extra keyword arguments)."""
    npx = shutil.which("npx") or "npx"
    chrome = args.chrome or shutil.which("google-chrome") or shutil.which("google-chrome-stable") or "google-chrome"
    env = dict(os.environ)
    # A port of its own: before #96 (an older --bc-bin) the server attached
    # to whatever Chrome ran on 9222 (another MCP session's) without --port.
    bc = [args.bc_bin, "--headless", "--port", str(free_port()), "--tool-groups", "all"]
    puppeteer_env = {**env, "PUPPETEER_LAUNCH_OPTIONS": json.dumps({"headless": True, "executablePath": chrome})}
    return {
        "bc": (BrowserControl, bc, env, {}),
        "bc-ff": (BrowserControl, bc, env, {"firefox": True}),
        "pw": (Playwright, [npx, "-y", PLAYWRIGHT, "--headless", "--isolated", "--browser", "chrome",
                            "--caps", "network,storage,devtools,vision,pdf,testing,config",
                            "--output-dir", os.path.join(output, "playwright")], env, {}),
        "cd": (DevTools, [npx, "-y", DEVTOOLS, "--headless", "--isolated",
                          "--usageStatistics=false", "--performanceCrux=false"], env, {}),
        "pp": (Puppeteer, [npx, "-y", PUPPETEER], puppeteer_env, {}),
    }


class Context:
    """What a scenario gets besides its adapter: the site and the files."""

    def __init__(self, site, files):
        self.site, self.files = site, files

    def url(self, path):
        return self.site.url(path)


def upload_files(folder):
    os.makedirs(folder, exist_ok=True)
    contents = {"a.txt": b"hello", "p1.jpg": b"\xff\xd8 one", "p2.jpg": b"\xff\xd8 two"}
    paths = []
    for name, data in contents.items():
        path = os.path.join(folder, name)
        with open(path, "wb") as f:
            f.write(data)
        paths.append(path)
    return paths


def recover(adapter, site):
    """After a failed scenario: accept a dialog left open (a beforeunload
    one lets the page go), drop a beforeunload guard, and leave the page."""
    adapter.counting = False
    try:
        try:
            adapter.dialog(True)
        except Exception:
            pass
        try:
            adapter.eval_raw("window.onbeforeunload = null")
        except Exception:
            pass
        try:
            adapter.goto(site.url("/second.html"))
        except Exception:
            pass
    finally:
        adapter.counting = True


def run_server(name, spec, chosen, context, log):
    cls, command, env, extra = spec
    rows = {}
    try:
        adapter = cls(command, env, **extra)
    except Exception as error:
        log(f"{name}: did not start: {error}")
        return {"version": None, "error": str(error), "results": {}}
    version = adapter.server.info.get("version")
    log(f"== {adapter.name} {version}")
    try:
        for item in chosen:
            adapter.calls = []
            start = time.time()
            status, note = "pass", ""
            try:
                item["run"](adapter, context)
            except Unsupported as error:
                status, note = "unsupported", f"no tool to {error}"
            except Fail as error:
                status, note = "fail", str(error)
            except Exception as error:
                status, note = "error", f"{type(error).__name__}: {error}"
                log(traceback.format_exc(limit=2))
            row = {"status": status, "note": note[:300], "seconds": round(time.time() - start, 2), **adapter.totals()}
            rows[item["id"]] = row
            log(f"  {MARK[status]} {item['id']:<22} {row['calls']:>3} calls {row['tokens']:>7} tok {row['seconds']:>6.1f}s  {note[:120]}")
            if "no reply after" in note:
                # A server stuck on a call answers nothing more: a new one, so
                # one hang does not fail every later scenario.
                log(f"  {name}: no reply; restarting it")
                adapter.close()
                adapter = cls(command, env, **extra)
            elif status != "pass":
                recover(adapter, context.site)
    finally:
        adapter.close()
    return {"version": version, "name": adapter.name, "results": rows}


def report(run, baseline):
    """The Markdown report of `run`, compared with `baseline` when given."""
    names = list(run["servers"])
    lines = [f"# Browser MCP comparison — {run['started']}", ""]
    lines.append("| Server | Version | Pass | Fail | Unsupported | Error |")
    lines.append("|---|---|---|---|---|---|")
    for name in names:
        server = run["servers"][name]
        counts = {k: sum(1 for r in server["results"].values() if r["status"] == k)
                  for k in ("pass", "fail", "unsupported", "error")}
        lines.append(f"| {server.get('name', name)} | {server.get('version')} | {counts['pass']} | {counts['fail']} | "
                     f"{counts['unsupported']} | {counts['error']} |")
    lines += ["", "✓ pass, ✗ fail, – no tool for it, E harness or server error. Calls / tokens of passed scenarios.", ""]
    lines.append("| Level | Scenario | " + " | ".join(names) + " |")
    lines.append("|---|---|" + "---|" * len(names))
    for item in run["scenarios"]:
        cells = []
        for name in names:
            row = run["servers"][name]["results"].get(item["id"])
            if not row:
                cells.append(" ")
                continue
            cell = MARK[row["status"]]
            if row["status"] == "pass":
                cell += f" {row['calls']}/{row['tokens']}"
            cells.append(cell)
        lines.append(f"| {item['level']} | `{item['id']}` {item['title']} | " + " | ".join(cells) + " |")
    lines += ["", "## Failures", ""]
    for name in names:
        for sid, row in run["servers"][name]["results"].items():
            if row["status"] in ("fail", "error"):
                lines.append(f"- **{name}** `{sid}`: {row['note']}")
    if baseline:
        lines += ["", "## Against the baseline", ""]
        changes = []
        for name in names:
            before = baseline.get("servers", {}).get(name, {}).get("results", {})
            for sid, row in run["servers"][name]["results"].items():
                old = before.get(sid, {}).get("status")
                if old and old != row["status"]:
                    changes.append(f"- **{name}** `{sid}`: {old} → {row['status']}")
        lines += changes or ["No scenario changed status."]
    return "\n".join(lines) + "\n"


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--servers", default="bc,bc-ff,pw,cd,pp", help="comma-separated: bc, bc-ff, pw, cd, pp")
    parser.add_argument("--only", help="regex on scenario ids")
    parser.add_argument("--level", type=int, default=4, help="run levels up to this one (1 = most common)")
    parser.add_argument("--bc-bin", default=os.path.join(REPO, "target", "release", "browser-control-mcp"))
    parser.add_argument("--chrome", help="Chrome for Puppeteer (default: google-chrome on PATH)")
    parser.add_argument("--save-baseline", action="store_true", help="keep this run as the baseline")
    args = parser.parse_args()

    started = datetime.datetime.now().strftime("%Y-%m-%dT%H-%M-%S")
    output = os.path.join(RESULTS, started)
    os.makedirs(output, exist_ok=True)
    log_file = open(os.path.join(output, "run.log"), "w")

    def log(message):
        print(message, flush=True)
        log_file.write(message + "\n")
        log_file.flush()

    chosen = [s for s in SCENARIOS if s["level"] <= args.level and (not args.only or re.search(args.only, s["id"]))]
    site = Site()
    context = Context(site, upload_files(os.path.join(output, "files")))
    Adapter.roots = [os.path.join(output, "files")]
    specs = servers(args, output)
    run = {"started": started, "scenarios": [{k: s[k] for k in ("id", "level", "title")} for s in chosen], "servers": {}}
    try:
        for name in args.servers.split(","):
            run["servers"][name] = run_server(name, specs[name], chosen, context, log)
    finally:
        site.stop()
    baseline = json.load(open(BASELINE)) if os.path.exists(BASELINE) else None
    with open(os.path.join(output, "results.json"), "w") as f:
        json.dump(run, f, indent=1, ensure_ascii=False)
    with open(os.path.join(output, "report.md"), "w") as f:
        f.write(report(run, baseline))
    if args.save_baseline:
        with open(BASELINE, "w") as f:
            json.dump(run, f, indent=1, ensure_ascii=False)
    log(f"report: {os.path.join(output, 'report.md')}")


if __name__ == "__main__":
    main()
