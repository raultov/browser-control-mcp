"""Chrome --local: a redirect to a non-local page that pauses on `debugger;` while
loading fails naming the pause; after resume the non-local page is readable.
Needs: cargo build --release --all-features; Chrome installed. Run from the repo root."""
import os, sys, tempfile, threading, functools, http.server
sys.path.insert(0, os.path.abspath('tests/compare'))
from mcp_client import Server
from siteserver import Site

d = tempfile.mkdtemp()
open(os.path.join(d, 'pause.html'), 'w').write(
    "<!doctype html><title>QaPause</title><script>debugger;</script><p>QaRemoteMarker4242</p>")
class Quiet(http.server.SimpleHTTPRequestHandler):
    def log_message(self, *a):
        pass
remote = http.server.ThreadingHTTPServer(('127.0.0.2', 0), functools.partial(Quiet, directory=d))
threading.Thread(target=remote.serve_forever, daemon=True).start()
RU = 'http://127.0.0.2:%d/pause.html' % remote.server_address[1]
local = http.server.ThreadingHTTPServer(('127.0.0.1', 0), functools.partial(Quiet, directory=d))
threading.Thread(target=local.serve_forever, daemon=True).start()
LU = 'http://127.0.0.1:%d/pause.html' % local.server_address[1]
BIN = os.path.abspath('target/release/browser-control-mcp')

S = Site()
srv = Server([BIN, '--headless', '--local', '--tool-groups', 'all'])
try:
    srv.call('navigate', {'url': S.url('/basic.html')})
    # Any breakpoint makes the server stop skipping pauses, so `debugger;` pauses.
    bp = srv.call('set_breakpoint', {'url': 'qa-none.js', 'line_number': 0})
    print('BP', bp.is_error, bp.short(120))
    # A local page that pauses first (as an agent debugging its own app would).
    c = srv.call('navigate', {'url': LU}, timeout=60)
    print('LOCAL', c.is_error, c.short(160))
    print('LOCAL_RESUME', srv.call('resume', {}).short(80))
    srv.call('navigate', {'url': S.url('/basic.html')}, timeout=60)
    r = srv.call('navigate', {'url': S.url('/redirect?to=' + RU)}, timeout=60)
    print('NAV', r.is_error, r.short(300))
    print('RESUME', srv.call('resume', {}).short(120))
    href = srv.call('evaluate_js', {'expression': 'location.href'})
    body = srv.call('evaluate_js', {'expression': "document.body?document.body.innerText:''"})
    print('HREF', href.short(120))
    print('BODY', body.short(120))
    leaked = '127.0.0.2' in href.text or 'QaRemoteMarker4242' in body.text
    print('LEAKED' if leaked else 'OK')
finally:
    srv.close(); S.stop(); remote.shutdown(); local.shutdown()
sys.exit(1 if leaked else 0)
