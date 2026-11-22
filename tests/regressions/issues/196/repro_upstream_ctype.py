"""mock_route upstream: content_type is not applied on Firefox.

Run from the repository root after `cargo build --release`:
    python3 repro_upstream_ctype.py firefox
Exits 1 while the page still gets the server's Content-Type.
"""
import json, os, sys
sys.path.insert(0, os.path.join(os.getcwd(), "tests", "compare"))
from mcp_client import Server
from siteserver import Site

browser = sys.argv[1] if len(sys.argv) > 1 else "firefox"
site = Site()
s = Server(["target/release/browser-control-mcp", "--browser", browser, "--headless",
            "--tool-groups", "network"], timeout=120)
try:
    s.call("navigate", {"url": site.url("/basic.html")})
    s.call("mock_route", {"url_pattern": "*/api/echo*", "upstream": True,
                          "status": 202, "content_type": "text/plain"})
    r = s.call("evaluate_js", {"expression":
        "const r = await fetch('/api/echo?x=1'); [r.status, r.headers.get('content-type')]"})
    got = json.loads(r.text)["value"]
    print(f"{browser}: status, content-type = {got}")
    if got != [202, "text/plain"]:
        print("BUG: upstream route's content_type not applied (got %r)" % got[1])
        sys.exit(1)
    print("OK")
finally:
    s.close()
    site.stop()
