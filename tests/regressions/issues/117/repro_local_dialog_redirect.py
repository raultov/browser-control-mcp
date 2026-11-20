import os, sys, tempfile, subprocess, time
sys.path.insert(0, os.path.abspath('tests/compare'))
from mcp_client import Server
from siteserver import Site

BIN = os.path.abspath('target/release/browser-control-mcp')
browser = sys.argv[1] if len(sys.argv) > 1 else 'chrome'
N = tempfile.mkdtemp()
with open(N + '/alert.html', 'w') as f:
    f.write("<!doctype html><title>Remote</title><script>alert('QaRemoteAlert')</script><h1>QaRemoteMarker9931</h1>")
# A non-local origin for --local: 127.0.0.2 (not localhost/127.0.0.1)
hs = subprocess.Popen([sys.executable, '-m', 'http.server', '18781', '--bind', '127.0.0.2', '--directory', N],
                      stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
time.sleep(1)
S = Site()
U = S.url('/redirect?to=http://127.0.0.2:18781/alert.html')
leaks = 0
try:
    s = Server([BIN, '--headless', '--local'] + (['--browser', 'firefox'] if browser == 'firefox' else []))
    r = s.call('navigate', {'url': U})
    print('navigate', r.is_error, r.short(120))
    for name, args in (('handle_dialog', {'accept': True}),
                       ('evaluate_js', {'expression': 'location.href'}),
                       ('evaluate_js', {'expression': 'document.body ? document.body.innerText : ""'}),
                       ('list_tabs', {})):
        x = s.call(name, args)
        leak = 'QaRemoteMarker9931' in x.text or '127.0.0.2' in x.text
        leaks += leak
        print(name, 'LEAK' if leak else 'ok', x.short(120))
    s.close()
finally:
    hs.kill()
    S.stop()
print('BUG: non-local page readable under --local' if leaks else 'OK')
sys.exit(1 if leaks else 0)
