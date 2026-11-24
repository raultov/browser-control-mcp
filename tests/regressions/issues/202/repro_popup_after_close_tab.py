import os, sys, json, threading, time
sys.path.insert(0, os.path.join(os.getcwd(), 'tests/compare'))
from mcp_client import Server
from http.server import ThreadingHTTPServer, BaseHTTPRequestHandler
class H(BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def do_GET(self):
        body = ('<!doctype html><title>%s</title><p>%s</p>' % (self.path, self.path)).encode()
        self.send_response(200); self.send_header('Content-Type', 'text/html')
        self.send_header('Cache-Control', 'no-store')
        self.send_header('Content-Length', str(len(body))); self.end_headers(); self.wfile.write(body)
httpd = ThreadingHTTPServer(('127.0.0.1', 0), H)
threading.Thread(target=httpd.serve_forever, daemon=True).start()
base = 'http://127.0.0.1:%d' % httpd.server_address[1]
ADD = ("const b = document.createElement('button'); b.id = 'pop'; b.textContent = 'pop';"
       " b.onclick = () => { window.__w = window.open('/popup.html'); }; document.body.append(b); 1")
def value(s, expr):
    r = s.call('evaluate_js', {'expression': expr})
    return json.loads(r.text[r.text.index('{'):]).get('value')
def tab_ids(s):
    t = s.call('list_tabs').text
    return [x['tab_id'] for x in json.loads(t[t.index('{'):])['tabs']]
missed = 0
runs = 8
for run in range(runs):
    s = Server(['target/release/browser-control-mcp', '--headless', '--browser', 'chrome'], timeout=60)
    try:
        s.call('navigate', {'url': base + '/a.html'})
        t2 = json.loads(s.call('open_tab', {'url': base + '/b.html'}).text)['tab_id']
        s.call('switch_tab', {'tab_id': 'tab-1'})
        s.call('close_tab', {'tab_id': t2})
        s.call('evaluate_js', {'expression': ADD})
        click = s.call('click_element', {'selector': '#pop'})
        noted = 'opened tab' in click.text
        time.sleep(1)
        ids = tab_ids(s)
        popup_url = value(s, 'window.__w && !window.__w.closed ? window.__w.location.href : null')
        later = None
        if len(ids) != 2:
            missed += 1
            time.sleep(5)
            later = tab_ids(s)
        print('run %d: click %d ms, popup noted in reply: %s; popup at %s; list_tabs 1 s after: %s; 5 s later: %s'
              % (run, click.ms, noted, popup_url and popup_url.replace(base, 'BASE'), ids, later))
    finally:
        s.close()
httpd.shutdown()
if missed:
    print('BUG: after open_tab + close_tab, the popup the page opened was missing from list_tabs in %d of %d runs' % (missed, runs))
    sys.exit(1)
print('popup listed in every run')
