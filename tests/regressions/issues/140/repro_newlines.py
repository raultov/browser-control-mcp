import os, sys, time
sys.path.insert(0, os.path.join(os.getcwd(), 'tests/compare'))
from mcp_client import Server
s = Server(['target/release/browser-control-mcp', '--headless'], timeout=300)
try:
    s.call('evaluate_js', {'expression': '0'})  # start the browser first
    res = {}
    for k in (20000, 200000):
        e = '1' + '\n' * k + '+1'
        t = time.time()
        r = s.call('evaluate_js', {'expression': e}, timeout=300)
        res[k] = int((time.time() - t) * 1000)
        print('K', k, 'ms', res[k], 'error', r.is_error, r.short(120))
finally:
    s.close()
slow = res[200000] > 5000
print('SLOW' if slow else 'FAST', 'ratio', round(res[200000] / max(res[20000], 1)))
sys.exit(1 if slow else 0)
