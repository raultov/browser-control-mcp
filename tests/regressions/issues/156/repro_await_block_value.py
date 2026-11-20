import sys
sys.path.insert(0, 'tests/compare')
from mcp_client import Server
from siteserver import Site

site = Site()
s = Server(['target/release/browser-control-mcp', '--headless'])
bad = 0
try:
    s.call('navigate', {'url': site.url('/basic.html')})
    for expr, want in [("for(let i=0;i<2;i++){0}'done'", 'done'),          # control: no await
                       ("for(let i=0;i<2;i++){await 0}'done'", 'done'),
                       ("if(1){await 0}'x'", 'x')]:
        r = s.call('evaluate_js', {'expression': expr})
        print(expr, '->', r.short())
        if '"%s"' % want not in r.text:
            bad += 1
finally:
    s.close()
if bad:
    print('BUG: %d top-level-await expression(s) lost the value after a closing brace' % bad)
    sys.exit(1)
