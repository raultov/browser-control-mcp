"""Two emulate calls sent at once to one Chrome tab: one of them is lost,
although both answer success."""
import json, select, sys, time

sys.path.insert(0, "tests/compare")
from mcp_client import Server

s = Server(["target/release/browser-control-mcp", "--headless", "--tool-groups", "emulation"])
lost = 0
try:
    s.call("navigate", {"url": "data:text/html,<p>x</p>"})
    for rnd in range(5):
        s.call("emulate", {"reset": True})
        calls = [{"color_scheme": "dark"}, {"reduced_motion": True}, {"device_scale_factor": 2}, {"width": 480, "height": 600}]
        for k, a in enumerate(calls):
            s.proc.stdin.write(json.dumps({"jsonrpc": "2.0", "id": 1000 + k, "method": "tools/call",
                                           "params": {"name": "emulate", "arguments": a}}) + "\n")
        s.proc.stdin.flush()
        got, t0 = {}, time.time()
        while len(got) < len(calls) and time.time() - t0 < 60:
            if select.select([s.proc.stdout], [], [], 1)[0]:
                m = json.loads(s.proc.stdout.readline())
                if m.get("id", 0) >= 1000:
                    got[m["id"]] = bool(m.get("result", {}).get("isError"))
        v = json.loads(s.call("evaluate_js", {"expression":
            "({dark: matchMedia('(prefers-color-scheme: dark)').matches,"
            " reduce: matchMedia('(prefers-reduced-motion: reduce)').matches,"
            " dpr: devicePixelRatio, w: innerWidth})"}).text)["value"]
        ok = v == {"dark": True, "reduce": True, "dpr": 2, "w": 480}
        print(f"round {rnd}: replies isError={got} page={v} {'ok' if ok else 'OPTION LOST'}")
        lost += not ok
finally:
    s.close()
print(f"LOST in {lost} of 5 rounds" if lost else "ALL KEPT")
sys.exit(1 if lost else 0)
