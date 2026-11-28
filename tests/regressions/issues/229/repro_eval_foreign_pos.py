"""evaluate_js places an error thrown inside a function defined by an earlier
evaluate_js (or by new Function) at a line and column of the current expression."""
import re, sys
sys.path.insert(0, "tests/compare")
from mcp_client import Server

DEFINE = "globalThis.evalBoom = function () {\n  const a = 1;\n  return a.b.c;\n};\n1"
CASES = {"call evalBoom": "0;\n\nevalBoom()", "call new Function": "0;\n\nnew Function('return null.q')()"}
wrong = []
for browser in ("chrome", "firefox"):
    s = Server(["target/release/browser-control-mcp", "--headless", "--browser", browser])
    try:
        s.call("navigate", {"url": "data:text/html,<p>x</p>"})
        s.call("evaluate_js", {"expression": DEFINE})
        for name, code in CASES.items():
            r = s.call("evaluate_js", {"expression": code})
            m = re.search(r"\(line (\d+), column (\d+)\)", r.text)
            pos = (int(m.group(1)), int(m.group(2))) if m else None
            # The call is at line 3, column 1 of the expression; anything else is not in it.
            ok = r.is_error and pos in (None, (3, 1), (3, 30))
            print(f"{browser} {name}: {r.text.strip()} -> {'ok' if ok else 'WRONG POSITION'}")
            if not ok:
                wrong.append(f"{browser}/{name}")
    finally:
        s.close()
print("WRONG: " + ", ".join(wrong) if wrong else "ALL OK")
sys.exit(1 if wrong else 0)
