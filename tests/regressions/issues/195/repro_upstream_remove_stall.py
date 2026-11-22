"""remove_route while an upstream route's responses are in flight leaves
requests paused for ever (Firefox).

Run from the repository root after `cargo build --release`:
    python3 repro_upstream_remove_stall.py firefox
Exits 1 while some of the page's 50 fetches never settle within 30 s of the removal.
"""
import json, os, sys, time
sys.path.insert(0, os.path.join(os.getcwd(), "tests", "compare"))
from mcp_client import Server
from siteserver import Site

browser = sys.argv[1] if len(sys.argv) > 1 else "firefox"
site = Site()
s = Server(["target/release/browser-control-mcp", "--browser", browser, "--headless",
            "--tool-groups", "network"], timeout=120)

def ev(code):
    return json.loads(s.call("evaluate_js", {"expression": code}).text)["value"]

stalled = 0
try:
    s.call("navigate", {"url": site.url("/basic.html")})
    for k in range(3):
        s.call("mock_route", {"url_pattern": "*/slow*", "upstream": True, "status": 202})
        ev("window.__done = null; window.__n = 0; Promise.allSettled([...Array(50)].map((_, i) =>"
           f" fetch('/slow?ms=300&r={k}&i=' + i).then(r => {{ window.__n++; return r.status }})))"
           ".then(v => { window.__done = v.length }); 1")
        s.call("remove_route", {})
        start = time.time()
        while time.time() - start < 30 and ev("window.__done") != 50:
            time.sleep(0.5)
        n = ev("window.__n")
        print(f"{browser} round {k}: {n}/50 responses after {time.time() - start:.0f} s")
        if n != 50:
            stalled += 1
        s.call("remove_route", {})
    if stalled:
        print(f"BUG: {stalled} round(s) left fetches paused after remove_route")
        sys.exit(1)
    print("OK")
finally:
    s.close()
    site.stop()
