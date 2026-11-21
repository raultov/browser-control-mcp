"""Snapshots the reproductions of the closed GitHub issues into
tests/regressions/issues/<number>/, for run.py. Maintainer tool: it needs the
`gh` CLI and the network; run.py and `make regressions` do not.

Each issue the automated tester filed has a "Reproduce" section: files to
save in the repository root (a Python script, sometimes a WebSocket server or
a secrets file), the command that runs it, and how the output reads while
the bug is present ("exits with code 1, and its standard output matches the
regular expression `...`"). This writes the files as they are, and
meta.json: {number, title, family, command, bug_regex, exit_on_bug}.

Usage (from the repository root):
    python3 -I tests/regressions/fetch.py [--repo raultov/browser-control-mcp] [--state all]
"""

import argparse
import json
import os
import re
import shlex
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))

# The families of the issues filed most often (see the CHANGELOG of 0.23).
FAMILIES = {
    "top-level-await": {135, 138, 139, 140, 142, 146, 147, 151, 155, 156, 157, 162, 169, 179, 188},
    "headers": {144, 149, 161, 168, 175, 189},
    "secrets": {109, 114, 118, 126, 129, 137, 145, 148, 150, 160, 181, 182, 183, 184},
    "websockets": {154, 158, 163, 164, 167, 171, 172, 173, 174, 176, 178, 185, 186},
}

FILE = re.compile(r"`([\w.-]+)`:\s*\n\s*```(\w*)\n(.*?)```", re.S)
RUN = re.compile(r"Run[^\n]*:\s*\n\s*```(?:sh|bash)?\n(.*?)```", re.S)
BUG = re.compile(r"While the bug is present, (.*?)(?:\n|$)")


def family(number):
    return next((name for name, numbers in FAMILIES.items() if number in numbers), "other")


def command(run):
    """The last `python3 ...` of the run block, as an argument list without
    the interpreter (a build before it is left to the runner)."""
    for part in reversed(re.split(r"\n|&&|;", run)):
        words = shlex.split(part.strip())
        if words and words[0].startswith("python"):
            return [w for w in words[1:] if w != "-I"]
    return None


def snapshot(issue):
    body = issue.get("body") or ""
    files = FILE.findall(body)
    run, bug = RUN.search(body), BUG.search(body)
    if not files or not run or not bug or not any(name.endswith(".py") for name, _, _ in files):
        return False
    regex = re.search(r"regular expression `(.+)`\.?\s*$", bug.group(1))
    argv = command(run.group(1))
    if not regex or not argv:
        return False
    pattern = regex.group(1)
    # Written ``...`` when the expression holds a backtick.
    if len(pattern) > 2 and pattern.startswith("`") and pattern.endswith("`"):
        pattern = pattern[1:-1].strip()
    directory = os.path.join(HERE, "issues", str(issue["number"]))
    os.makedirs(directory, exist_ok=True)
    for name, _, text in files:
        with open(os.path.join(directory, name), "w") as out:
            out.write(text)
    meta = {
        "number": issue["number"],
        "title": issue["title"],
        "family": family(issue["number"]),
        "command": argv,
        "bug_regex": pattern,
        "exit_on_bug": "exits with code 1" in bug.group(1),
    }
    with open(os.path.join(directory, "meta.json"), "w") as out:
        json.dump(meta, out, indent=1)
        out.write("\n")
    return True


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--repo", default="raultov/browser-control-mcp")
    parser.add_argument("--state", default="closed", choices=["closed", "all"],
                        help="all: also the open issues, to check a fix before closing them")
    args = parser.parse_args()
    listed = subprocess.run(
        ["gh", "api", "--paginate", f"repos/{args.repo}/issues?state={args.state}&per_page=100",
         "--jq", ".[] | select(.pull_request == null) | {number, title, body}"],
        check=True, capture_output=True, text=True).stdout
    issues = [json.loads(line) for line in listed.splitlines() if line.strip()]
    saved = [i["number"] for i in issues if snapshot(i)]
    print("%d %s issues, %d with a reproduction saved" % (len(issues), args.state, len(saved)))
    return 0


if __name__ == "__main__":
    sys.exit(main())
