#!/usr/bin/env python3
"""Gives agent CLIs the tasks of tasks.py with browser-control-mcp as their
only tool server, and reports per task whether the goal was reached, with
the calls, error replies, reply size and model input tokens each run took.

    python3 -I tests/compare/agent_eval/run.py --bin target/release/browser-control-mcp --label after
    python3 -I tests/compare/agent_eval/run.py --bin /tmp/bc-baseline --label baseline
    python3 -I tests/compare/agent_eval/run.py --bin ... --only 'form|select' --baseline <results.json>

`bench.py` and `../run.py` call tools through adapters: no model reads a
tool's description there. Here a model does, so a description that got
worse shows as a task an agent no longer finishes, or finishes with more
calls and errors. Not a gate: it spends model tokens. See ../README.md.
"""

import argparse
import concurrent.futures
import datetime
import json
import os
import re
import shutil
import socket
import subprocess
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
COMPARE = os.path.dirname(HERE)
sys.path.insert(0, HERE)
sys.path.insert(0, COMPARE)

from proxy import Proxy  # noqa: E402
from siteserver import Site  # noqa: E402
from tasks import TASKS  # noqa: E402

REPO = os.path.dirname(os.path.dirname(COMPARE))
RELAY = os.path.join(HERE, "relay.py")
PREAMBLE = "Use the browser tools you have. "


def free_port():
    """A TCP port nothing listens on now, for a browser of its own."""
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        return probe.getsockname()[1]


def clean_env():
    """The environment without the variables that would send the CLIs
    elsewhere (an API proxy, the session of the agent running this)."""
    return {k: v for k, v in os.environ.items() if not k.startswith(("ANTHROPIC_", "OPENCODE"))}


def claude(work, server, prompt, model):
    config = os.path.join(work, "mcp.json")
    with open(config, "w") as f:
        json.dump({"mcpServers": {"bc": {"command": server[0], "args": server[1:]}}}, f)
    command = ["claude", "-p", prompt, "--strict-mcp-config", "--mcp-config", config, "--tools", "",
               "--allowedTools", "mcp__bc", "--output-format", "json", "--no-session-persistence",
               "--setting-sources", "project", "--model", model]
    return command, read_claude


def read_claude(stdout):
    """(final answer, model input tokens) from `claude -p --output-format json`."""
    data = json.loads(stdout)
    usage = sum(u.get("inputTokens", 0) + u.get("cacheReadInputTokens", 0) + u.get("cacheCreationInputTokens", 0)
                for u in (data.get("modelUsage") or {}).values())
    return data.get("result") or "", usage, bool(data.get("is_error"))


def opencode(work, server, prompt, model):
    os.makedirs(os.path.join(work, ".opencode"), exist_ok=True)
    config = {"$schema": "https://opencode.ai/config.json",
              "mcp": {"servers": {"bc": {"type": "local", "command": server, "codemode": False}}},
              "agents": {"bceval": {"mode": "primary", "description": "Browser tasks",
                                    "permissions": [{"action": "*", "resource": "*", "effect": "deny"},
                                                    {"action": "bc_*", "resource": "*", "effect": "allow"}]}}}
    with open(os.path.join(work, ".opencode", "opencode.json"), "w") as f:
        json.dump(config, f)
    command = ["opencode2", "run", "--standalone", "--archived", "--agent", "bceval", "--format", "json",
               "-m", model, prompt]
    return command, read_opencode


def read_opencode(stdout):
    """(final answer, model input tokens) from `opencode run --format json`."""
    texts, usage, failed = [], 0, False
    for line in stdout.splitlines():
        try:
            event = json.loads(line)
        except ValueError:
            continue
        part = event.get("part") or {}
        if event.get("type") == "text":
            texts.append(part.get("text", ""))
        elif event.get("type") == "step_finish":
            tokens = part.get("tokens") or {}
            usage += tokens.get("input", 0) + (tokens.get("cache") or {}).get("read", 0)
        elif event.get("type") == "error":
            failed = True
    return "\n".join(texts), usage, failed


CLIENTS = {"claude": claude, "opencode": opencode}


def value_of(text):
    """The value of an `evaluate_js` reply."""
    try:
        return json.loads(text).get("value")
    except (ValueError, AttributeError):
        return None


def judge(oracle, proxy, answer):
    """(passed, what was seen)."""
    kind = oracle[0]
    if kind == "answer":
        lower = answer.lower()
        missing = [w for w in oracle[1] if w.lower() not in lower]
        return not missing, f"missing {missing}" if missing else "all words in the answer"
    if kind == "tabs":
        _, text = proxy.call("list_tabs")
        missing = [w for w in oracle[1] if w not in text]
        return not missing, f"missing {missing} in list_tabs" if missing else "tabs as expected"
    expression, expected = oracle[1], oracle[2]
    seen = None
    deadline = time.time() + 5
    while time.time() < deadline:
        error, text = proxy.call("evaluate_js", {"expression": expression})
        seen = None if error else value_of(text)
        if seen == expected:
            return True, repr(seen)
        time.sleep(0.3)
    return False, f"{expression} is {seen!r}, expected {expected!r}"


def one_run(args, site, task, client, index, out_dir):
    work = tempfile.mkdtemp(prefix=f"agent-{task['id']}-{client}-")
    log = os.path.join(out_dir, "logs", f"{task['id']}.{client}.{index}.jsonl")
    proxy = Proxy([args.bin, "--headless", "--port", str(free_port())], log)
    server = ["python3", "-I", RELAY, str(proxy.port)]
    prompt = PREAMBLE + task["prompt"].format(url=f"http://127.0.0.1:{site.port}")
    model = args.claude_model if client == "claude" else args.opencode_model
    command, read = CLIENTS[client](work, server, prompt, model)
    start = time.time()
    row = {"task": task["id"], "client": client, "run": index, "model": model}
    try:
        done = subprocess.run(command, capture_output=True, text=True, cwd=work, env=clean_env(),
                              timeout=args.timeout)
        row["seconds"] = round(time.time() - start, 1)
        try:
            answer, usage, failed = read(done.stdout)
        except ValueError:
            answer, usage, failed = "", 0, True
        row["model_input_tokens"] = usage
        row["answer"] = answer[-400:]
        if done.returncode != 0 or failed:
            row["status"] = "error"
            row["note"] = (done.stderr.strip() or done.stdout.strip())[-300:]
        else:
            passed, seen = judge(task["oracle"], proxy, answer)
            row["status"] = "pass" if passed else "fail"
            row["note"] = seen
    except subprocess.TimeoutExpired:
        row["seconds"] = round(time.time() - start, 1)
        row["status"], row["note"] = "error", f"no answer after {args.timeout} s"
    finally:
        proxy.stop()
        shutil.rmtree(work, ignore_errors=True)
    calls = []
    with open(log) as f:
        for line in f:
            entry = json.loads(line)
            if entry.get("event") == "call":
                calls.append(entry)
    row["calls"] = len(calls)
    row["error_replies"] = sum(1 for c in calls if c["error"])
    row["reply_tokens"] = sum(c["chars"] for c in calls) // 4
    row["tools"] = [c["tool"] for c in calls]
    row["errors"] = [f"{c['tool']}: {c.get('message', '')[:160]}" for c in calls if c["error"]]
    return row


def summary(rows):
    """Per (task, client): passes, runs, and the mean calls, error replies and tokens."""
    table = {}
    for row in rows:
        key = (row["task"], row["client"])
        cell = table.setdefault(key, {"pass": 0, "fail": 0, "error": 0, "calls": 0, "error_replies": 0,
                                      "model_input_tokens": 0, "reply_tokens": 0})
        cell[row["status"]] += 1
        if row["status"] != "error":
            for k in ("calls", "error_replies", "model_input_tokens", "reply_tokens"):
                cell[k] += row.get(k, 0)
    return table


def report(rows, baseline_rows, label):
    table = summary(rows)
    before = summary(baseline_rows) if baseline_rows else {}
    clients = sorted({r["client"] for r in rows})
    lines = [f"# Agent eval: {label}", "",
             "Per task and client: passes / judged runs; mean calls, error replies and model input tokens of "
             "the judged runs" + (" (baseline in brackets)." if before else "."), ""]
    lines.append("| Task | " + " | ".join(clients) + " |")
    lines.append("|---|" + "---|" * len(clients))
    for task in [t["id"] for t in TASKS]:
        cells = []
        for client in clients:
            cell = table.get((task, client))
            if not cell:
                cells.append(" ")
                continue
            judged = cell["pass"] + cell["fail"]
            text = f"{cell['pass']}/{judged}"
            if judged:
                text += f"; {cell['calls'] / judged:.1f} calls, {cell['error_replies'] / judged:.1f} err, " \
                        f"{cell['model_input_tokens'] / judged / 1000:.1f}k tok"
            if cell["error"]:
                text += f" ({cell['error']} E)"
            old = before.get((task, client))
            if old:
                oj = old["pass"] + old["fail"]
                if oj:
                    text += f" [{old['pass']}/{oj}; {old['calls'] / oj:.1f}, {old['error_replies'] / oj:.1f}, " \
                            f"{old['model_input_tokens'] / oj / 1000:.1f}k]"
            cells.append(text)
        if any(c.strip() for c in cells):
            lines.append(f"| `{task}` | " + " | ".join(cells) + " |")
    lines += ["", "## Totals", ""]
    for client in clients:
        mine = [r for r in rows if r["client"] == client and r["status"] != "error"]
        line = f"- **{client}**: {totals_text(mine)}"
        if baseline_rows:
            tasks_run = {r["task"] for r in rows}
            theirs = [r for r in baseline_rows if r["client"] == client and r["status"] != "error"
                      and r["task"] in tasks_run]
            line += f"; baseline {totals_text(theirs)}"
        lines.append(line)
    if baseline_rows:
        lines += ["", "## Verdict", ""] + verdict(table, before)
    lines += ["", "## Failures and errors", ""]
    for row in rows:
        if row["status"] != "pass":
            lines.append(f"- **{row['task']}** {row['client']} #{row['run']} {row['status']}: {row['note'][:200]}")
            for error in row["errors"][:4]:
                lines.append(f"  - {error}")
    return "\n".join(lines) + "\n"


def totals_text(rows):
    if not rows:
        return "no judged run"
    n = len(rows)
    passed = sum(1 for r in rows if r["status"] == "pass")
    calls = sum(r["calls"] for r in rows)
    errors = sum(r["error_replies"] for r in rows)
    tokens = sum(r["model_input_tokens"] for r in rows)
    return f"{passed}/{n} passed, {calls} calls, {errors} error replies, {tokens / n / 1000:.1f}k model input tokens per run"


def verdict(table, before):
    """The acceptance rule of the plan: total passes not lower; no task falls
    from all runs to a third or less; error replies and calls at most 10 %
    higher in total."""
    out = []
    ok = True
    clients = sorted({c for _, c in table})
    for client in clients:
        keys = [k for k in table if k[1] == client and k in before]
        now = {k: table[k] for k in keys}
        old = {k: before[k] for k in keys}
        passes_now = sum(c["pass"] for c in now.values())
        passes_old = sum(c["pass"] for c in old.values())
        calls_now = sum(c["calls"] for c in now.values())
        calls_old = sum(c["calls"] for c in old.values())
        err_now = sum(c["error_replies"] for c in now.values())
        err_old = sum(c["error_replies"] for c in old.values())
        problems = []
        if passes_now < passes_old:
            problems.append(f"passes {passes_now} < {passes_old}")
        for key in keys:
            o, n = old[key], now[key]
            oj, nj = o["pass"] + o["fail"], n["pass"] + n["fail"]
            if oj and nj and o["pass"] == oj and n["pass"] * 3 <= nj:
                problems.append(f"{key[0]} fell from {o['pass']}/{oj} to {n['pass']}/{nj}")
        if calls_now > calls_old * 1.1 + 1:
            problems.append(f"calls {calls_now} > {calls_old} + 10 %")
        if err_now > err_old * 1.1 + 1:
            problems.append(f"error replies {err_now} > {err_old} + 10 %")
        ok = ok and not problems
        out.append(f"- **{client}**: " + ("accepted" if not problems else "; ".join(problems)))
    out.append("")
    out.append("**Accepted.**" if ok else "**Not accepted.**")
    return out


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--bin", default=os.path.join(REPO, "target", "release", "browser-control-mcp"))
    parser.add_argument("--clients", default="claude,opencode")
    parser.add_argument("--claude-model", default="sonnet")
    parser.add_argument("--opencode-model", default="google/antigravity-gemini-3.8-flash-medium")
    parser.add_argument("--runs", type=int, default=3)
    parser.add_argument("--jobs", type=int, default=3, help="runs at once")
    parser.add_argument("--only", help="regex on task ids")
    parser.add_argument("--timeout", type=int, default=300, help="seconds per run")
    parser.add_argument("--label", default="run")
    parser.add_argument("--baseline", help="results.json of an earlier run to compare with")
    args = parser.parse_args()
    args.bin = os.path.abspath(args.bin)

    started = datetime.datetime.now().strftime("%Y-%m-%dT%H-%M-%S")
    out_dir = os.path.join(COMPARE, "results", f"agent-{started}-{args.label}")
    os.makedirs(os.path.join(out_dir, "logs"))
    tasks = [t for t in TASKS if not args.only or re.search(args.only, t["id"])]
    jobs = [(t, c, i) for i in range(1, args.runs + 1) for t in tasks for c in args.clients.split(",")]
    site = Site()
    rows = []
    try:
        with concurrent.futures.ThreadPoolExecutor(args.jobs) as pool:
            futures = [pool.submit(one_run, args, site, t, c, i, out_dir) for t, c, i in jobs]
            for future in concurrent.futures.as_completed(futures):
                row = future.result()
                rows.append(row)
                print(f"{row['status']:<5} {row['task']:<18} {row['client']:<8} #{row['run']} "
                      f"{row['calls']:>2} calls {row['error_replies']} err {row['seconds']:>6.1f}s  "
                      f"{row['note'][:100]}", flush=True)
    finally:
        site.stop()
    baseline_rows = None
    if args.baseline:
        with open(args.baseline) as f:
            baseline_rows = json.load(f)["rows"]
    with open(os.path.join(out_dir, "results.json"), "w") as f:
        json.dump({"label": args.label, "started": started, "bin": args.bin,
                   "models": {"claude": args.claude_model, "opencode": args.opencode_model}, "rows": rows},
                  f, indent=1, ensure_ascii=False)
    text = report(rows, baseline_rows, args.label)
    with open(os.path.join(out_dir, "report.md"), "w") as f:
        f.write(text)
    print(text)
    print("saved", out_dir)


if __name__ == "__main__":
    main()
