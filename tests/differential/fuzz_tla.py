"""Differential check of evaluate_js's top-level await: the browser's own
indirect `eval` of each generated code (no `await`) is the oracle for what
evaluate_js answers for `await 0; <code>`.

The codes are a statement (or two) before a final expression that evaluates
to the string 'v', joined by separators that test automatic semicolons,
comments and line breaks. A code whose `eval` does not give 'v' is left out.

Usage (from the repository root):
    python3 -I tests/differential/fuzz_tla.py [--browser chrome|firefox]
        [--bin target/debug/browser-control-mcp] [--sample N] [--seed S]

`--sample 0` checks every code (about 12 000, a few minutes per browser).
Exits 1 when evaluate_js answers anything but "v" for one of them.
"""

import argparse
import itertools
import json
import os
import random
import re
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
sys.path.insert(0, os.path.join(ROOT, "tests", "compare"))

from mcp_client import Server  # noqa: E402
from siteserver import Site  # noqa: E402

PREFIX = [
    "if (0) ;", "if (1) {} else {}", "if (0) 1; else if (0) 2; else {}", "for (;0;) ;",
    "for (var i = 0; i < 1; i++) {}", "for (var k in {}) ;", "for (var q of []) {}",
    "while (0) ;", "while (0) {}", "do ; while (0)", "do {} while (0)", "do while (0) ; while (0)",
    "do if (0) ; else ; while (0)", "do for (;0;) ; while (0)", "do do ; while (0); while (0)",
    "do try {} finally {} while (0)", "lbl: for (;0;) {}", "l2: { }", "l3: l4: ;",
    "switch (1) { case 1: break; default: }", "try {} catch (e) {} finally {}", "try {} catch {}",
    "{ }", "{ let z = 1 }", ";", "var a = 1", "var f = function () {}", "var g = () => {}",
    "var g2 = () => ({})", "var h = async () => {}", "var o = {a: 1}", "var c = class {}",
    "function fn() {}", "async function afn() {}", "function* gen() {}", "async function* agen() {}",
    "class K {}", "class K2 { static { } m() {} static s = 1 }", "var t = `x${1}y`", "var t2 = `${`${1}`}`",
    "var r = /[/]/g", "var r2 = /=/", "var d = 4 / 2 / 1", "var p = ({if: 1}).if", "var w = ({})?.while",
    "var n = new Date", "var ty = typeof 1", "var u = 1 ? {b: 1} : [2]", "var m = [1, 2].map(x => x * 2)",
    "var ar = x => { return x }", "with ({}) {}", "var x1 = 1, y1 = 2", "var v0 = void 0",
    "var gt = 1 > 2", "var sh = 1 >> 2", "var obj2 = {get a() { return 1 }, set a(v) {}, async b() {}, *c() {}}",
    "var of = 1", "var async = 1", "var let_ = 1; let_", "var get = 1", "var yield_ = function* () { yield }",
    "if (0) {} else for (;0;) {}", "for (;0;) if (0) {} else {}", "var s = 'it\\'s'", "var re3 = 1 /2/ 3",
    "var e = 2 ** 3", "var nn = null ?? 1", "var la = (a, b) => a", "var ia = (function () {})()",
    "var cm = 1 // c", "var cm2 = 1 /* c */", "/* lead */ var cm3 = 1", "x2: while (0) continue x2",
    "var z9 = {} /1/ 1", "var i2 = 0; i2++", "var i3 = 0; i3\n++i3",
]
SEP = ["; ", "\n", " ", ";\n", "\n\n", " /* c */ ", " // c\n"]
TAIL = [
    "'v'", "('v')", "['v'][0]", "`v`", "/v/.source", "'v' // t", "'v';", "'v'\n", "(() => 'v')()",
    "({v: 'v'}).v", "String('v')", "'v'.at(-1)", "typeof 0 ? 'v' : 0", "!0 && 'v'", "new String('v') + ''",
    "'u', 'v'", "[1, 'v']\n[1]", "'v'\n/* e */", "'V'\n.toLowerCase()", "void 0 ?? 'v'",
]


def cases(seed):
    out = [p + s + t for p, s, t in itertools.product(PREFIX, SEP, TAIL)]
    rnd = random.Random(seed)
    for _ in range(6000):
        out.append(rnd.choice(PREFIX) + rnd.choice(SEP) + rnd.choice(PREFIX) + rnd.choice(SEP) + rnd.choice(TAIL))
    return list(dict.fromkeys(out))


def oracle(server, codes):
    """What the page's indirect eval gives for each code: 'S:<string>',
    'T:<type>' or 'ERR:<name>'."""
    answers = []
    for i in range(0, len(codes), 500):
        chunk = codes[i:i + 500]
        js = ("JSON.stringify(%s.map(c => { try { const r = (0, eval)(c); "
              "return typeof r === 'string' ? 'S:' + r : 'T:' + typeof r } "
              "catch (e) { return 'ERR:' + e.name } }))" % json.dumps(chunk))
        reply = server.call("evaluate_js", {"expression": js})
        answers += json.loads(json.loads(reply.text)["value"])
    return answers


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--browser", default="chrome", choices=["chrome", "firefox"])
    parser.add_argument("--bin", default=os.path.join(ROOT, "target", "debug", "browser-control-mcp"))
    parser.add_argument("--sample", type=int, default=2500)
    parser.add_argument("--seed", type=int, default=7)
    parser.add_argument("--visible", action="store_true")
    args = parser.parse_args()

    command = [args.bin, "--browser", args.browser] + ([] if args.visible else ["--headless"])
    site = Site()
    server = Server(command, timeout=120)
    failures = {}
    try:
        server.call("navigate", {"url": site.url("/basic.html")})
        codes = cases(args.seed)
        valid = [c for c, answer in zip(codes, oracle(server, codes)) if answer == "S:v"]
        if args.sample and args.sample < len(valid):
            valid = random.Random(args.seed).sample(valid, args.sample)
        for code in valid:
            reply = server.call("evaluate_js", {"expression": "await 0; " + code})
            if reply.is_error or '"value": "v"' not in reply.text:
                failures[code] = re.sub(r"\s+", " ", reply.text)[:110]
    finally:
        server.close()
        site.stop()
    print("top-level await differential (%s): %d codes, %d failures" % (args.browser, len(valid), len(failures)))
    for code in sorted(failures, key=len)[:15]:
        print("  %r => %s" % (code, failures[code]))
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
