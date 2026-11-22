"""mock_route upstream on Chrome: a status with no standard reason phrase
(299, 250, 599) leaves the request paused for ever.

Run from the repository root after `cargo build --release`:
    python3 repro_upstream_status_hang.py
Exits 1 while a fetch through an upstream route with status 299 never settles.
"""
import json, os, sys
sys.path.insert(0, os.path.join(os.getcwd(), "tests", "compare"))
from mcp_client import Server
from siteserver import Site

site = Site()
s = Server(["target/release/browser-control-mcp", "--browser", "chrome", "--headless",
            "--tool-groups", "network"], timeout=120)
try:
    s.call("navigate", {"url": site.url("/basic.html")})
    got = {}
    for status in (202, 299):
        s.call("remove_route", {})
        s.call("mock_route", {"url_pattern": "*/api/echo*", "upstream": True, "status": status})
        r = s.call("evaluate_js", {"expression":
            f"await Promise.race([fetch('/api/echo?s={status}').then(r => r.status),"
            " new Promise(res => setTimeout(() => res('no response after 5 s'), 5000))])"})
        got[status] = json.loads(r.text)["value"]
        print(f"upstream status {status}: page got {got[status]!r}")
    print(s.call("get_network_logs", {"url_filter": "s=299"}).text)
    if got[299] != 299:
        print("BUG: the request through an upstream route with status 299 never got a response")
        sys.exit(1)
    print("OK")
finally:
    s.close()
    site.stop()
