import sys
sys.path.insert(0, 'tests/compare')
from mcp_client import Server
from siteserver import Site

site = Site()
s = Server(['target/release/browser-control-mcp', '--headless'], timeout=180)
ms = {}
try:
    s.call('navigate', {'url': site.url('/basic.html')})
    for n in (10000, 40000):
        r = s.call('evaluate_js', {'expression': 'await 0;\n' + '(/[' * n}, timeout=180)
        ms[n] = r.ms
        # The browser parses (and rejects) the same text in a few ms:
        p = s.call('evaluate_js', {'expression': "{const t=performance.now();try{(0,eval)('0;\\n'+'(/['.repeat(%d))}catch(e){};performance.now()-t}" % n})
        print("n=%d evaluate_js %d ms; in-page parse of same text: %s" % (n, r.ms, p.short(80)))
finally:
    s.close()
ratio = ms[40000] / max(ms[10000], 1)
print('ratio 4x input -> %.1fx time' % ratio)
if ms[40000] > 5000 and ratio > 8:
    print('QUADRATIC: top-level await scan time grows with the square of the input')
    sys.exit(1)
