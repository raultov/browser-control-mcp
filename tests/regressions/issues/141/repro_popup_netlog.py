import os, sys, json, time, threading
sys.path.insert(0, os.path.join(os.getcwd(), 'tests/compare'))
from mcp_client import Server
from http.server import ThreadingHTTPServer, BaseHTTPRequestHandler
PAGES = {
  '/opener.html': ('text/html', b'<!doctype html><title>opener</title><a id="pop" href="/popup.html" target="_blank">open popup</a>'),
  '/popup.html': ('text/html', b'<!doctype html><title>popup</title><script src="/popup.js"></script><script>fetch("/api/popup-data")</script>'),
  '/popup.js': ('text/javascript', b'document.title = "popup ready";'),
  '/api/popup-data': ('application/json', b'{"ok":1}'),
}
class H(BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'
    def log_message(self, *a): pass
    def do_GET(self):
        ctype, body = PAGES.get(self.path.split('?')[0], ('text/plain', b'nf'))
        self.send_response(200 if body != b'nf' else 404); self.send_header('Content-Type', ctype)
        self.send_header('Cache-Control', 'no-store'); self.send_header('Content-Length', str(len(body)))
        self.end_headers(); self.wfile.write(body)
httpd = ThreadingHTTPServer(('127.0.0.1', 0), H); port = httpd.server_address[1]
threading.Thread(target=httpd.serve_forever, daemon=True).start()
base = f'http://127.0.0.1:{port}'
bad = 0
def popup_tab(s, marker):
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline:
        tabs = json.loads(s.call('list_tabs').text)['tabs']
        tab = next((t['tab_id'] for t in tabs if marker in (t.get('url') or '')), None)
        if tab is not None:
            return tab
        time.sleep(0.25)
    return None
def check(browser, variant, s, tab, page):
    global bad
    if tab is None:
        print(browser, variant, 'BAD no popup tab'); bad += 1; return
    paths = (page, '/popup.js', '/api/popup-data')
    found = {}
    deadline = time.monotonic() + 10
    while True:
        text = s.call('get_network_logs', {'tab_id': tab}).text
        found = {p: any(p in line and '=> 200' in line for line in text.splitlines()) for p in paths}
        if all(found.values()) or time.monotonic() > deadline:
            break
        time.sleep(0.5)
    for p in paths:
        print(browser, variant, 'OK ' if found[p] else 'BAD', p); bad += not found[p]
for browser in ('chrome',):
    s = Server(['target/release/browser-control-mcp', '--headless', '--browser', browser], timeout=120)
    try:
        s.call('navigate', {'url': base + '/opener.html'})
        r = s.call('click_element', {'selector': '#pop'})
        print(browser, 'link click', r.is_error, r.short(120))
        check(browser, 'link', s, popup_tab(s, '/popup.html'), '/popup.html')
        s.call('navigate', {'url': base + '/opener.html'})
        r = s.call('evaluate_js', {'expression': "window.open('/popup.html?w=2'); 1", 'user_gesture': True})
        print(browser, 'window.open', r.is_error, r.short(120))
        check(browser, 'window.open', s, popup_tab(s, '/popup.html?w=2'), '/popup.html?w=2')
    finally:
        s.close()
print('BAD_COUNT', bad)
sys.exit(1 if bad else 0)
