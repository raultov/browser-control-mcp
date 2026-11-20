import sys, os, subprocess, time
# Run from the repository root.
ROOT = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(ROOT, 'tests/compare'))
os.chdir(os.path.join(ROOT, 'tests/compare'))
from mcp_client import Server
from siteserver import Site

os.makedirs('/tmp/qa-mark', exist_ok=True)
open('/tmp/qa-mark/m.html', 'w').write('<title>NONLOCAL</title>\n')
log = open('/tmp/qa-mark.log', 'w')
srv = subprocess.Popen(['python3', '-m', 'http.server', '18766', '--bind', '127.0.0.2', '--directory', '/tmp/qa-mark'], stderr=log, stdout=subprocess.DEVNULL)
time.sleep(1)
site = Site()
R = site.url('/redirect?to=http://127.0.0.2:18766/m.html')
def reset(): open('/tmp/qa-mark.log', 'w').close()
def hits(): return open('/tmp/qa-mark.log').read().count('GET /m.html')
fails = []
browsers = sys.argv[1:] or ['chrome', 'firefox']
for name in browsers:
    B = [] if name == 'chrome' else ['--browser', 'firefox']
    s = Server(['../../target/release/browser-control-mcp', '--headless', '--local', '--tool-groups', 'perf'] + B)
    out = {}
    for key, tool, args, to in [('navigate', 'navigate', {'url': R}, None), ('open_tab', 'open_tab', {'url': R}, None),
                                ('lighthouse_report', 'lighthouse_report', {'url': R, 'categories': ['seo']}, 90)]:
        reset()
        r = s.call(tool, args, timeout=to)
        time.sleep(0.5)
        n = hits()
        h = s.call('evaluate_js', {'expression': 'location.href'})
        print(f'{name} {key}: hits={n} href={h.short()} | r.is_error={r.is_error} r={r.short(200)}', flush=True)
        if n or '127.0.0.2' in h.text:
            fails.append(f'{name}:{key}')
    s.close()
srv.kill()
print('FAILS', fails)
sys.exit(1 if fails else 0)
