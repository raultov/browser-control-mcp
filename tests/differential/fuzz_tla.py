"""Differential check of evaluate_js's top-level await: the browser's own
indirect `eval` of each generated code (no `await`) is the oracle for what
evaluate_js answers for `await 0; <code>`.

The codes are a statement (or two) before a final expression that evaluates
to the string 'v', joined by separators that test automatic semicolons,
comments and line breaks; edge cases written by hand; and random programs
of statements nested up to three deep. A code whose `eval` does not give
'v' is left out.

Usage (from the repository root):
    python3 -I tests/differential/fuzz_tla.py [--browser chrome|firefox]
        [--bin target/debug/browser-control-mcp] [--sample N] [--seed S]

`--sample 0` checks every code (about 15 000, a few minutes per browser).
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

# Whole codes for the corners a statement and a final expression leave out:
# characters outside ASCII, every line terminator, labels on labels,
# sequences, functions and classes as the last value, Annex B comments.
EDGE = [
    "const s1 = '\u017dlu\u0165\U0001F600'; s1.length > 0 ? 'v' : 0",
    "/* \u00fcn\u00efc\u00f6d\u00e9 \U0001F600 */ var n\u0303 = 'v'\nn\u0303",
    "var a1 = 1\r\nvar b1 = 2\r\n'v'\r\n",
    "var a2 = 1\u2028var b2 = 2\u2029'v'",
    "\ufeffvar c3 = 1; 'v'",
    "var d4 = 1;\t\x0b\x0c\u00a0'v'",
    "a5: b5: 'v'",
    "a6: { b6: 'v' }; 'v'",
    "0, 'u', 'v'",
    "(() => 'v')",
    "var f7 = () => 'v'; f7()",
    "(class { static v = 'v' }).v",
    "`${'v'}`",
    "String.raw`v`",
    "[...'v'][0]",
    "({ v: 'v' })?.v",
    "null ?? 'v'",
    "var g8 = { get v() { return 'v' } }; g8.v",
    "var h9 = 1 <!-- an HTML comment opener\n'v'",
    "var p1 = 1\n--> an HTML comment closer at a line start\n'v'",
    "var i10 = 2 / 2 / 1; i10 === 1 ? 'v' : 0",
    "var j11 = /[/]/.source + /\\//.source; 'v'",
    "if (1) { 'x' } 'v'",
    "do { } while (0) 'v'",
    "do ; while (0)\n'v'",
    "try { throw 1 } catch { } 'v'",
    "switch (1) { default: } 'v'",
    "function k12() { return 'x' }\n'v'",
    "async function l13() { await 0 } 'v'",
    "var m14 = async () => { await 0 }\n'v'",
    "class N15 { static { } }\n'v'",
    "var yield16 = 'v'; yield16",
    "var let17 = 'v'; let17",
    "var of18 = 'v'; of18",
    "var get19 = 'v'; get19",
    "var o20 = {if: 'v'}; o20.if",
    "var o21 = {await: 'v'}; o21.await",
    "void 0, 'v'",
    "typeof typeof 0 === 'string' ? 'v' : 0",
    "'w'\n.replace('w', 'v')",
]

# A random program: statements built from these, nested up to three deep,
# joined by the separators above, then a final expression.
STATEMENTS = [
    "E;", "var vN = E;", ";", "{ S S }", "if (E) S", "if (E) S else S", "while (0) S", "do S while (0)",
    "for (;0;) S", "for (var kN in {}) S", "for (var qN of []) S", "lN: S", "try { S } catch (e) { S }",
    "try { S } finally { S }", "switch (E) { case 1: S default: S }", "function fN() { S }",
    "class CN { m() { S } }", "with ({}) S", "zN = E", "E",
]
EXPRESSIONS = [
    "0", "1", "'s'", "x => x", "(E)", "E ? E : E", "[E, E]", "({a: E})", "`t${E}`", "/r/g", "typeof E",
    "E + E", "function () {}", "class {}", "async () => {}", "new Date", "void E", "a.b", "o?.p", "E, E",
]


def expression(rnd, depth):
    template = rnd.choice(EXPRESSIONS if depth < 3 else EXPRESSIONS[:4])
    return re.sub("E", lambda _: expression(rnd, depth + 1), template)


def statement(rnd, depth, counter):
    template = rnd.choice(STATEMENTS if depth < 3 else ["E;", ";", "zN = E"])
    counter[0] += 1
    text = template.replace("N", str(counter[0]))
    text = re.sub("E", lambda _: expression(rnd, depth + 1), text)
    return re.sub("S", lambda _: statement(rnd, depth + 1, counter), text)


def programs(rnd, count):
    out = []
    for _ in range(count):
        counter = [0]
        body = "".join(statement(rnd, 0, counter) + rnd.choice(SEP) for _ in range(rnd.randint(1, 3)))
        out.append(body + rnd.choice(TAIL))
    return out


def cases(seed):
    out = [p + s + t for p, s, t in itertools.product(PREFIX, SEP, TAIL)]
    rnd = random.Random(seed)
    for _ in range(6000):
        out.append(rnd.choice(PREFIX) + rnd.choice(SEP) + rnd.choice(PREFIX) + rnd.choice(SEP) + rnd.choice(TAIL))
    out += EDGE
    out += programs(rnd, 6000)
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
        server.call("evaluate_js", {"expression": "window.a = {b: 1}; window.o = {p: 1}; 1"})
        codes = cases(args.seed)
        valid = [c for c, answer in zip(codes, oracle(server, codes)) if answer == "S:v"]
        if args.sample and args.sample < len(valid):
            # Every edge case, and a sample of the generated ones.
            edges = [c for c in valid if c in EDGE]
            rest = [c for c in valid if c not in EDGE]
            valid = edges + random.Random(args.seed).sample(rest, args.sample - len(edges))
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
