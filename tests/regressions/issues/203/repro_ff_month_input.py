import os, sys, json, threading
sys.path.insert(0, os.path.join(os.getcwd(), 'tests/compare'))
from mcp_client import Server
from http.server import ThreadingHTTPServer, BaseHTTPRequestHandler
PAGE = (b"<!doctype html><input id=m type=month><input id=other>"
        b"<script>window.__ev=[];for(const el of document.querySelectorAll('input')){"
        b"el.addEventListener('input',()=>__ev.push(el.id+':input'));el.addEventListener('change',()=>__ev.push(el.id+':change'))}</script>")
class H(BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def do_GET(self):
        self.send_response(200); self.send_header('Content-Type', 'text/html')
        self.send_header('Content-Length', str(len(PAGE))); self.end_headers(); self.wfile.write(PAGE)
httpd = ThreadingHTTPServer(('127.0.0.1', 0), H)
threading.Thread(target=httpd.serve_forever, daemon=True).start()
base = 'http://127.0.0.1:%d' % httpd.server_address[1]
def value(s, expr):
    r = s.call('evaluate_js', {'expression': expr})
    return json.loads(r.text[r.text.index('{'):]).get('value')
results = {}
for browser in ('chrome', 'firefox'):
    s = Server(['target/release/browser-control-mcp', '--headless', '--browser', browser], timeout=60)
    try:
        s.call('navigate', {'url': base + '/'})
        ok = s.call('fill_input', {'selector': '#m', 'text': '2026-10'})
        events = value(s, '__ev.splice(0)')
        bad = s.call('fill_input', {'selector': '#m', 'text': 'not-a-month'})
        after = value(s, "document.getElementById('m').value")
        results[browser] = {'events': events, 'malformed_is_error': bad.is_error, 'value_after_malformed': after}
        print(browser, json.dumps(results[browser]), '|', bad.text.strip()[:160])
    finally:
        s.close()
want = {'events': ['m:input', 'm:change'], 'malformed_is_error': True, 'value_after_malformed': '2026-10'}
wrong = [b for b, r in results.items() if r != want]
if wrong:
    print('BUG: fill_input on <input type=month> breaks its promise on', wrong)
    sys.exit(1)
print('fill_input on type=month as documented on both browsers')
