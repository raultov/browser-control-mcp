import os, sys
sys.path.insert(0, os.path.join(os.getcwd(), 'tests/compare'))
from mcp_client import Server
CASES = [(r'''const n = 'a"b'.replace(/"/g, '').length; await (n + 4000)''', '4002'),
         (r'''/'/.test("it's"); await Promise.resolve(4141)''', '4141')]
bad = 0
s = Server(['target/release/browser-control-mcp', '--headless', '--browser', 'chrome'], timeout=120)
try:
    for expr, want in CASES:
        r = s.call('evaluate_js', {'expression': expr})
        ok = (not r.is_error) and want in r.text
        bad += not ok
        print('OK ' if ok else 'BAD', repr(expr), '->', r.is_error, r.short(200))
finally:
    s.close()
print('BAD_COUNT', bad)
sys.exit(1 if bad else 0)
