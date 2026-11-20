import sys, json
sys.path.insert(0, 'tests/compare')
from mcp_client import Server
from siteserver import Site

browser = sys.argv[1] if len(sys.argv) > 1 else 'chrome'
cases = [('await 0; function f(){ return 7 } f()', 7),
         ('await 0; class A { v() { return 8 } } new A().v()', 8),
         ('async function g(){ return 9 } await g()', 9),
         ('await 0; function h(){ return 7 }\nh()', 7),
         ('const of = await Promise.resolve(3)\nconst y = of\ny * 2', 6)]
site = Site()
s = Server(['target/release/browser-control-mcp', '--headless'] + (['--browser', 'firefox'] if browser == 'firefox' else []), timeout=60)
bad = 0
try:
    s.call('navigate', {'url': site.url('/basic.html')})
    for e, want in cases:
        r = s.call('evaluate_js', {'expression': e})
        got = None if r.is_error else json.loads(r.text).get('value')
        ok = not r.is_error and got == want
        bad += not ok
        print('OK ' if ok else 'BAD', repr(e), 'want', want, '->', r.short(200))
finally:
    s.close(); site.stop()
print('BAD', bad)
