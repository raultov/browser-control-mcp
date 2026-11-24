import json, os, sys
sys.path.insert(0, os.path.join(os.getcwd(), "tests", "compare"))
from mcp_client import Server
from siteserver import Site

site = Site()
srv = Server(["target/release/browser-control-mcp", "--headless", "--browser", "firefox", "--tool-groups", "all"])
try:
    srv.call("navigate", {"url": site.url("/basic.html")})
    srv.call("mock_route", {"url_pattern": "*/r1*", "status": 302,
                            "headers": [{"name": "Location", "value": "/api/echo?via=r1"}]})
    print("fetch:", srv.call("evaluate_js", {"expression": "await fetch('/r1').then(r => r.status + ' ' + r.url)"}).short())
    r = srv.call("get_network_logs", {"url_filter": "r1", "include_details": True})
    text = r.text[r.text.find("{"):]
    hops = [h for req in json.loads(text)["requests"].values() for h in req.get("redirects", [])]
    print("hops:", json.dumps(hops))
    if not hops or any("status" not in h or "location" not in h for h in hops):
        print("BUG: mocked redirect hop has no status/location")
        sys.exit(1)
    print("OK")
finally:
    srv.close()
    site.stop()
