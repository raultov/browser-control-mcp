import sys
sys.path.insert(0, 'tests/compare')
from mcp_client import Server
from siteserver import Site

site = Site()
s = Server(['target/release/browser-control-mcp', '--headless'])
bad = 0
try:
    s.call('navigate', {'url': site.url('/basic.html')})
    for expr in ["await Promise.resolve(40) +\n  2",
                 "window.qaV =\n  await Promise.resolve('v')",
                 "const f = await Promise.resolve(3)\nf ===\n  3 ?\n  'yes' :\n  'no'"]:
        r = s.call('evaluate_js', {'expression': expr})
        print(repr(expr), '->', 'isError' if r.is_error else 'ok', r.short())
        if r.is_error and 'SyntaxError' in r.text:
            bad += 1
finally:
    s.close()
if bad:
    print('BUG: %d expression(s) split after a trailing operator -> SyntaxError' % bad)
    sys.exit(1)
