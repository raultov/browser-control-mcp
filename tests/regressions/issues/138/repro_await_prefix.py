import os, sys
sys.path.insert(0, os.path.join(os.getcwd(), 'tests/compare'))
from mcp_client import Server
s = Server(['target/release/browser-control-mcp', '--headless'], timeout=120)
bad = 0
try:
    for e, want in [('const done = await Promise.resolve(7071); done', '7071'),
                    ("const format = await Promise.resolve('fmt-ok'); format", 'fmt-ok'),
                    ('const r = await Promise.resolve(8383); r', '8383')]:
        r = s.call('evaluate_js', {'expression': e})
        ok = (not r.is_error) and want in r.text
        bad += not ok
        print('OK ' if ok else 'BAD', repr(e), '->', r.short(200))
finally:
    s.close()
print('BAD_COUNT', bad)
sys.exit(1 if bad else 0)
