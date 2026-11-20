"""Chrome open_tab drops the history entry a page pushes with pushState()
while loading (Firefox keeps it), so history back fails."""
import json, os, subprocess, sys, tempfile, time
sys.path.insert(0, 'tests/compare')
from mcp_client import Server

BIN = 'target/release/browser-control-mcp'
d = tempfile.mkdtemp()
open(os.path.join(d, 'p.html'), 'w').write(
    "<!doctype html><title>Push</title><script>history.pushState({s:2},'','p.html?step=2')</script><h1>Push page</h1>")
hs = subprocess.Popen([sys.executable, '-m', 'http.server', '18767', '--bind', '127.0.0.1', '--directory', d],
                      stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
time.sleep(1)
res = {}
try:
    for B in ['chrome', 'firefox']:
        srv = Server([BIN, '--headless'] + (['--browser', 'firefox'] if B == 'firefox' else []))
        try:
            o = srv.call('open_tab', {'url': 'http://127.0.0.1:18767/p.html'}, timeout=60)
            T = json.loads(o.text)['tab_id']
            L = srv.call('evaluate_js', {'tab_id': T, 'expression': 'history.length'})
            b = srv.call('navigate', {'tab_id': T, 'history': 'back'}, timeout=30)
            length = json.loads(L.text)['value']
            print(B, 'history.length =', length, '| back:', b.is_error, b.text.strip().splitlines()[0])
            res[B] = (length, b.is_error)
        finally:
            srv.close()
finally:
    hs.terminate()
if res.get('chrome') != (2, False) and res.get('firefox') == (2, False):
    print('PARITY BUG: Chrome dropped the pushState entry after open_tab')
    sys.exit(1)
print('OK')
