"""RUST_LOG=trace: a --secrets value typed with fill_input reaches stderr
(tungstenite::protocol logs every websocket frame to the browser)."""
import os, re, sys, tempfile, collections
sys.path.insert(0, 'tests/compare')
from mcp_client import Server
from siteserver import Site

BIN = os.path.abspath('target/release/browser-control-mcp')
d = tempfile.mkdtemp()
env = os.path.join(d, 'qa.env'); log = os.path.join(d, 'stderr.log')
open(env, 'w').write('PW=QaSecret7731zz\n')
S = Site()
srv = Server(['sh', '-c', 'exec "$0" --headless --secrets "$1" 2>"$2"', BIN, env, log],
             env=dict(os.environ, RUST_LOG='trace'))
try:
    srv.call('navigate', {'url': S.url('/login.html')}, timeout=60)
    srv.call('fill_input', {'selector': '#pass', 'text': '<secret:PW>'})
    e = srv.call('evaluate_js', {'expression': "document.getElementById('pass').value"})
    print('evaluate_js reply:', e.text.strip().replace('\n', ' '))
finally:
    srv.close()
lines = [l for l in open(log, errors='replace') if 'QaSecret7731zz' in l]
targets = collections.Counter()
for l in lines:
    m = re.search(r'(?:TRACE|DEBUG|INFO|WARN|ERROR)\s+(?:\x1b\[[0-9;]*m)*([A-Za-z0-9_:]+)', l)
    targets[m.group(1) if m else '?'] += 1
print('log bytes', os.path.getsize(log), 'lines with secret', len(lines), 'targets', dict(targets))
if lines:
    print('SECRET IN LOG')
    sys.exit(1)
print('OK')
