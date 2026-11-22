"""get_network_logs on Chrome lists a header an upstream route removed.

Run from the repository root after `cargo build --release`:
    python3 repro_upstream_removed_header_logged.py chrome
Exits 1 while get_network_logs shows the removed ETag the page never got.
"""
import base64, json, os, sys, time
sys.path.insert(0, os.path.join(os.getcwd(), "tests", "compare"))
from mcp_client import Server
from siteserver import Site

browser = sys.argv[1] if len(sys.argv) > 1 else "chrome"
site = Site()
s = Server(["target/release/browser-control-mcp", "--browser", browser, "--headless",
            "--tool-groups", "network"], timeout=120)
try:
    url = site.url("/headers?h=" + base64.urlsafe_b64encode(b'ETag: "v9"').decode())
    s.call("navigate", {"url": site.url("/basic.html")})
    s.call("mock_route", {"url_pattern": "*/headers*", "upstream": True,
                          "remove_headers": ["etag"], "headers": [{"name": "X-Up", "value": "yes"}]})
    r = s.call("evaluate_js", {"expression":
        f"const r = await fetch({json.dumps(url)}); [r.headers.get('etag'), r.headers.get('x-up')]"})
    page = json.loads(r.text)["value"]
    time.sleep(0.5)
    t = s.call("get_network_logs", {"url_filter": "/headers", "include_details": True,
                                    "include_sensitive_headers": True}).text
    reqs = json.loads(t[t.index("{"):])["requests"]
    names = sorted(n.lower() for v in reqs.values() for n in v.get("response_headers", {}))
    print(f"{browser}: page sees etag, x-up = {page}; get_network_logs response headers = {names}")
    if page[0] is None and "etag" in names:
        print("BUG: get_network_logs lists the ETag the upstream route removed; the page never got it")
        sys.exit(1)
    print("OK")
finally:
    s.close()
    site.stop()
