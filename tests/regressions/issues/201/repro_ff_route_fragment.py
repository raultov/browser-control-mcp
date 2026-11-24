import json, os, sys
sys.path.insert(0, os.path.join(os.getcwd(), "tests", "compare"))
from mcp_client import Server
from siteserver import Site

browser = sys.argv[1] if len(sys.argv) > 1 else "firefox"
site = Site()
cmd = ["target/release/browser-control-mcp", "--headless", "--tool-groups", "all"]
if browser == "firefox":
    cmd += ["--browser", "firefox"]
srv = Server(cmd)
try:
    srv.call("navigate", {"url": site.url("/basic.html")})
    srv.call("mock_route", {"url_pattern": "*/frag.txt", "body": "G"})
    plain = srv.call("evaluate_js", {"expression": "await fetch('/frag.txt').then(r => r.status + ' ' + r.url)"}).short()
    frag = srv.call("evaluate_js", {"expression": "await fetch('/frag.txt#part').then(async r => r.status + ' ' + (await r.text()).slice(0, 20))"}).short()
    print(browser, "without fragment:", plain)
    print(browser, "with fragment:", frag)
    if '"200 G"' not in frag:
        print("BUG: a request whose URL has a #fragment is not matched by the route")
        sys.exit(1)
    print("OK")
finally:
    srv.close()
    site.stop()
