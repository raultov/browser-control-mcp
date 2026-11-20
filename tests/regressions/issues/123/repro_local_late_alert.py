"""Chrome --local: navigate to a local redirect whose non-local target alert()s from a
0 ms timer after load reports success and leaves the non-local page readable.
Needs: cargo build --release --all-features; Chrome installed. Run from the repo root.
Tries up to 5 fresh servers; exits 1 at the first leak."""
import os, sys, time, tempfile, threading, functools, http.server
sys.path.insert(0, os.path.abspath('tests/compare'))
from mcp_client import Server
from siteserver import Site

d = tempfile.mkdtemp()
open(os.path.join(d, 'late0.html'), 'w').write(
    "<!doctype html><p>QaRemoteMarker7272</p>"
    "<script>onload=function(){setTimeout(function(){alert('QaLate')},0)}</script>")
class Quiet(http.server.SimpleHTTPRequestHandler):
    def log_message(self, *a):
        pass
remote = http.server.ThreadingHTTPServer(('127.0.0.2', 0), functools.partial(Quiet, directory=d))
threading.Thread(target=remote.serve_forever, daemon=True).start()
RU = 'http://127.0.0.2:%d/late0.html' % remote.server_address[1]
BIN = os.path.abspath('target/release/browser-control-mcp')

leaked = False
for attempt in range(1, 6):
    S = Site()
    srv = Server([BIN, '--headless', '--local'])
    try:
        srv.call('navigate', {'url': S.url('/basic.html')})
        r = srv.call('navigate', {'url': S.url('/redirect?to=' + RU)}, timeout=60)
        time.sleep(1)
        hd = srv.call('handle_dialog', {'accept': True})
        href = srv.call('evaluate_js', {'expression': 'location.href'})
        body = srv.call('evaluate_js', {'expression': "document.body?document.body.innerText:''"})
        print(attempt, 'NAV', r.is_error, r.short(200))
        print(attempt, 'HANDLE_DIALOG', hd.short(120))
        print(attempt, 'HREF', href.short(120))
        print(attempt, 'BODY', body.short(120))
        if (not r.is_error) or '127.0.0.2' in href.text or 'QaRemoteMarker7272' in body.text:
            print('LEAKED')
            leaked = True
            break
    finally:
        srv.close(); S.stop()
remote.shutdown()
sys.exit(1 if leaked else 0)
