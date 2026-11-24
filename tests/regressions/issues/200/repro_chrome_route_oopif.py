import json, os, sys, tempfile
sys.path.insert(0, os.path.join(os.getcwd(), "tests", "compare"))
import siteserver
from mcp_client import Server

browser = sys.argv[1] if len(sys.argv) > 1 else "chrome"
pages = tempfile.mkdtemp()
with open(os.path.join(pages, "inner.html"), "w") as f:
    f.write("<script>fetch('/api/mock-me').then(async r => parent.postMessage("
            "{host: location.hostname, v: r.status + ' ' + (await r.text()).slice(0, 12)}, '*'))</script>")
with open(os.path.join(pages, "host.html"), "w") as f:
    f.write("<!doctype html><body><script>window.R = {}; addEventListener('message', e => R[e.data.host] = e.data.v);"
            "function start() { for (const o of [location.origin, 'http://localhost:' + location.port]) {"
            " const i = document.createElement('iframe'); i.src = o + '/inner.html'; document.body.appendChild(i); } }"
            "</script></body>")
siteserver.SITE = pages
site = siteserver.Site()
cmd = ["target/release/browser-control-mcp", "--headless", "--tool-groups", "all"]
if browser == "firefox":
    cmd += ["--browser", "firefox"]
srv = Server(cmd)
try:
    srv.call("navigate", {"url": site.url("/host.html")})
    srv.call("mock_route", {"url_pattern": "*/api/mock-me*", "body": "MOCKED"})
    r = srv.call("evaluate_js", {"expression": "start(); await new Promise(d => { const t = Date.now() + 8000;"
                 " const c = () => Object.keys(R).length >= 2 || Date.now() > t ? d(JSON.stringify(R)) : setTimeout(c, 100); c() })"})
    print(browser, r.short(300))
    print(browser, srv.call("list_routes", {}).short(300))
    text = r.text[r.text.find("{"):]
    cells = json.loads(json.loads(text)["value"])
    if cells.get("localhost") != "200 MOCKED":
        print("BUG: the cross-origin iframe's request was not mocked:", cells.get("localhost"))
        sys.exit(1)
    print("OK")
finally:
    srv.close()
    site.stop()
