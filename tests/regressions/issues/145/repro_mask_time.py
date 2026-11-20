import os, sys, time, tempfile
sys.path.insert(0, os.path.join(os.getcwd(), 'tests/compare'))
from mcp_client import Server
env = os.path.join(tempfile.mkdtemp(), 'secrets.env')
with open(env, 'w', encoding='utf-8') as f:
    for i in range(100):
        f.write(f"S{i:03d}='Pw{i:03d}/ä&<>\"x= y'\n")
s = Server(['target/release/browser-control-mcp', '--headless', '--secrets', env], timeout=300)
try:
    s.call('evaluate_js', {'expression': '1'})
    t = time.time(); a = s.call('evaluate_js', {'expression': "'x'.repeat(99000) + 'nosecret'"}, timeout=300); base_ms = int((time.time() - t) * 1000)
    t = time.time(); b = s.call('evaluate_js', {'expression': "'x'.repeat(99000) + ['Pw050/', 'ä&<>\"x= y'].join('')"}, timeout=300); sec_ms = int((time.time() - t) * 1000)
    masked = '<secret:S050>' in b.text and 'Pw050/' not in b.text
    ok = masked and sec_ms < 3000 and not a.is_error
    print('base_ms', base_ms, 'secret_ms', sec_ms, 'masked', masked, 'OK' if ok else 'SLOW')
finally:
    s.close()
sys.exit(0 if ok else 1)
