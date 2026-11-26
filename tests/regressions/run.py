"""Runs the reproductions of closed GitHub issues (saved by fetch.py) against
the server, and fails when one of them shows its bug again.

Each reproduction runs as its issue says, "from the repository root": in a
scratch directory holding its files, `tests` (a link to this repository's)
and `target/release/browser-control-mcp` (a link to the binary under test),
so its relative paths work unchanged. Its bug is back when its output
matches the issue's regular expression, or when it exits with a code other
than 0 and the issue says it exits with 1 while the bug is present. A
reproduction that does not finish in time fails too.

Usage (from the repository root):
    python3 -I tests/regressions/run.py [--bin target/debug/browser-control-mcp]
        [--family top-level-await|headers|secrets|websockets|console|other]
        [--issue N ...] [--jobs 4] [--timeout 300]
"""

import argparse
import concurrent.futures
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(os.path.dirname(HERE))

# Reproductions whose check still fails by design: the issue was closed
# with a documented deviation, or as outside what the feature promises (the
# README's "Scope of the convenience features"). Reported as SKIP, never run.
CHANGED_BY_DESIGN = {
    181: "--secrets masks whole values only: a value cut by evaluate_js's 100 000-character limit is a part of one",
    182: "--secrets masks whole values only: an inspect_dom window chosen to end inside a value shows a part of one",
    183: "--secrets masks whole values only: a value cut by inspect_dom's 100 000-character limit is a part of one",
    185: "Firefox cannot pause a popup: its WebSocket frames before the debugger attaches are not recorded (documented)",
    186: "Firefox cannot pause a popup: a socket that opens and ends before the debugger attaches is not recorded "
         "(documented); the popup of a popup is recorded like any popup once attached",
    188: "code over 64 KB runs once as written: the browser's error is the answer, with a note on the limit "
         "(a second run after a runtime error would run the code before it twice)",
    105: "the redirected request is still sent under --local; the tab is checked where it ended and "
         "sent back to about:blank, which the script's 0-requests criterion does not see",
    108: "Firefox's offline emulation fails mocked routes; documented, and set_offline and mock_route say so",
    116: "CDP cannot drop one history entry: Chrome's history.length still counts the blank creation page "
         "when the page pushState()s while loading (documented), which the script checks",
    # mock_route upstream was removed in 1.0.0-rc.1. #194 and #195 also
    # happened with a plain mock: make e2e checks those (status 299, and
    # remove_route with 50 requests in flight).
    194: "its script uses mock_route upstream, removed in 1.0.0-rc.1 (make e2e checks a plain mock with status 299)",
    195: "its script uses mock_route upstream, removed in 1.0.0-rc.1 (make e2e checks remove_route with 50 requests in flight)",
    196: "its script uses mock_route upstream, removed in 1.0.0-rc.1",
    197: "its script uses mock_route upstream, removed in 1.0.0-rc.1",
}


def issues(families, numbers):
    base = os.path.join(HERE, "issues")
    found = []
    for name in sorted(os.listdir(base), key=int):
        with open(os.path.join(base, name, "meta.json")) as meta:
            info = json.load(meta)
        if families and info["family"] not in families:
            continue
        if numbers and info["number"] not in numbers:
            continue
        found.append((os.path.join(base, name), info))
    return found


def serial(directory, info):
    """Whether a reproduction must run alone: it writes a page into the
    shared test site, or it measures time or memory, which other browsers
    running beside it would skew."""
    if re.search(r"SLOW|QUADRATIC|RSS|ratio", info["bug_regex"]):
        return True
    for name in os.listdir(directory):
        if not os.path.isfile(os.path.join(directory, name)):
            continue  # a __pycache__ a reproduction left
        with open(os.path.join(directory, name), errors="replace") as source:
            text = source.read()
        if re.search(r"'tests', 'compare', 'site'|tests/compare/site", text):
            return True
    return False


def run_one(directory, info, binary, timeout):
    """`(verdict, seconds, detail)` of one reproduction."""
    if info["number"] in CHANGED_BY_DESIGN:
        return "SKIP", 0.0, CHANGED_BY_DESIGN[info["number"]]
    scratch = tempfile.mkdtemp(prefix="bcm-regression-%d-" % info["number"])
    started = time.time()
    try:
        for name in os.listdir(directory):
            if name != "meta.json" and os.path.isfile(os.path.join(directory, name)):
                shutil.copy(os.path.join(directory, name), scratch)
        os.symlink(os.path.join(ROOT, "tests"), os.path.join(scratch, "tests"))
        os.makedirs(os.path.join(scratch, "target", "release"))
        os.symlink(binary, os.path.join(scratch, "target", "release", "browser-control-mcp"))
        try:
            done = subprocess.run([sys.executable] + info["command"], cwd=scratch, capture_output=True,
                                  text=True, timeout=timeout, start_new_session=True)
        except subprocess.TimeoutExpired:
            return "FAIL", time.time() - started, "no answer in %d s" % timeout
        output = done.stdout
        matched = re.search(info["bug_regex"], output, re.M)
        failed_exit = info["exit_on_bug"] and done.returncode != 0
        elapsed = time.time() - started
        if matched or failed_exit:
            why = ("matches %r" % matched.group(0)[:80]) if matched else "exit code %d" % done.returncode
            tail = " | ".join((output + done.stderr).strip().splitlines()[-4:])[:400]
            return "FAIL", elapsed, "%s: %s" % (why, tail)
        return "PASS", elapsed, ""
    finally:
        shutil.rmtree(scratch, ignore_errors=True)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--bin", default=os.path.join(ROOT, "target", "debug", "browser-control-mcp"))
    parser.add_argument("--family", action="append", default=[])
    parser.add_argument("--issue", type=int, action="append", default=[])
    parser.add_argument("--jobs", type=int, default=4)
    parser.add_argument("--timeout", type=int, default=300)
    args = parser.parse_args()
    binary = os.path.abspath(args.bin)
    if not os.path.exists(binary):
        print("no binary at %s: build it first" % binary)
        return 2
    selected = issues(set(args.family), set(args.issue))
    alone = [(d, i) for d, i in selected if serial(d, i)]
    together = [(d, i) for d, i in selected if not serial(d, i)]
    failures = 0
    for batch, jobs in ((together, args.jobs), (alone, 1)):
        with concurrent.futures.ThreadPoolExecutor(max_workers=jobs) as pool:
            futures = {pool.submit(run_one, d, i, binary, args.timeout): i for d, i in batch}
            for future in concurrent.futures.as_completed(futures):
                info = futures[future]
                verdict, seconds, detail = future.result()
                failures += verdict == "FAIL"
                print("%s #%d %s (%s, %.0f s)%s" % (verdict, info["number"], info["title"][:70], info["family"],
                                                  seconds, "\n      " + detail if detail else ""), flush=True)
    print("regressions: %d reproduction(s), %d failure(s)" % (len(selected), failures))
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
