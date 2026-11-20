"""--local: lighthouse_report redirected to a non-local page that alert()s leaves the page readable.
Needs: cargo build --release --all-features; Chrome installed. Run from the repo root."""
import os, sys, tempfile, threading, functools, http.server
sys.path.insert(0, os.path.abspath('tests/compare'))
from mcp_client import Server
from siteserver import Site

d = tempfile.mkdtemp()
open(os.path.join(d, 'alert.html'), 'w').write(
    '<!doctype html><title>QaRemote</title><script>alert("QaRemoteAlert")</script><p>QaRemoteMarker9931</p>')
class Quiet(http.server.SimpleHTTPRequestHandler):
    def log_message(self, *a):
        pass
remote = http.server.ThreadingHTTPServer(('127.0.0.2', 0), functools.partial(Quiet, directory=d))
threading.Thread(target=remote.serve_forever, daemon=True).start()
RURL = 'http://127.0.0.2:%d/alert.html' % remote.server_address[1]

BIN = os.path.abspath('target/release/browser-control-mcp')
S = Site()
srv = Server([BIN, '--headless', '--local', '--tool-groups', 'all'])
try:
    print('NAV0', srv.call('navigate', {'url': S.url('/basic.html')}).short())
    r = srv.call('lighthouse_report', {'url': S.url('/redirect?to=' + RURL)}, timeout=150)
    print('LH', r.is_error, r.text[:400])
    hd = srv.call('handle_dialog', {'accept': True})
    href = srv.call('evaluate_js', {'expression': 'location.href'})
    body = srv.call('evaluate_js', {'expression': "document.body?document.body.innerText:''"})
    print('HANDLE_DIALOG', hd.short(200))
    print('HREF', href.short(200))
    print('BODY', body.short(200))
    leaked = '127.0.0.2' in href.text or 'QaRemoteMarker9931' in body.text
    print('LEAKED' if leaked else 'OK')
finally:
    srv.close(); S.stop(); remote.shutdown()
sys.exit(1 if leaked else 0)
