import os, sys, tempfile
sys.path.insert(0, os.path.abspath('tests/compare'))
from mcp_client import Server
from siteserver import Site

BIN = os.path.abspath('target/release/browser-control-mcp')
env = tempfile.NamedTemporaryFile('w', suffix='.env', delete=False)
env.write('PW=Qx"y\\z7731\n')  # value: Qx"y\z7731
env.close()
S = Site()
s = Server([BIN, '--headless', '--secrets', env.name])
s.call('navigate', {'url': S.url('/login.html')})
print('fill', s.call('fill_input', {'selector': '#pass', 'text': '<secret:PW>'}).short(80))
V = "document.querySelector('#pass').value"
leaks = 0
for e in (V, "JSON.stringify({pw: %s})" % V):
    r = s.call('evaluate_js', {'expression': e})
    leak = 'z7731' in r.text
    leaks += leak
    print('LEAK' if leak else 'ok', e, '->', r.short(120))
s.close()
S.stop()
os.unlink(env.name)
print('BUG: secret value shown' if leaks else 'OK')
sys.exit(1 if leaks else 0)
