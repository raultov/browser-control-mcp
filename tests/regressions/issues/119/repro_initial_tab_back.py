import os, sys, json
sys.path.insert(0, os.path.abspath('tests/compare'))
from mcp_client import Server
from siteserver import Site

BIN = os.path.abspath('target/release/browser-control-mcp')
browser = sys.argv[1] if len(sys.argv) > 1 else 'chrome'
S = Site()
A, B = S.url('/basic.html'), S.url('/second.html')


def where(s):
    r = s.call('evaluate_js', {'expression': 'location.href + " " + history.length'})
    try:
        return json.loads(r.text).get('value')
    except Exception:
        return r.short(100)


s = Server([BIN, '--headless'] + (['--browser', 'firefox'] if browser == 'firefox' else []))
print('initial tab:', where(s))
s.call('navigate', {'url': A})
s.call('navigate', {'url': B})
bad = False
for i in range(3):
    r = s.call('navigate', {'history': 'back'})
    loc = where(s)
    print('back', i + 1, 'isError', r.is_error, r.short(90), '=>', loc)
    if not loc.startswith('http://127.0.0.1'):
        bad = True
    if r.is_error:
        break
s.close()
S.stop()
print('BUG: history back left the visited pages' if bad else 'OK')
sys.exit(1 if bad else 0)
