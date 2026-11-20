import sys, json, time, select
sys.path.insert(0, 'tests/compare')
from mcp_client import Server, Result
from siteserver import Site
S = Site()
srv = Server(['target/release/browser-control-mcp', '--headless'])
print(srv.call('open_instance', {'label': 'wf', 'browser': 'firefox', 'headless': True}, timeout=60).text.strip())
print(srv.call('open_tab', {'instance_id': 'wf', 'url': S.url('/basic.html')}, timeout=60).text.strip())
def send(i, name, args):
    srv.proc.stdin.write(json.dumps({'jsonrpc': '2.0', 'id': i, 'method': 'tools/call', 'params': {'name': name, 'arguments': args}}) + '\n')
    srv.proc.stdin.flush()
send(9001, 'wait_for', {'instance_id': 'wf', 'text': 'NeverThere', 'timeout_ms': 30000})
time.sleep(1)
t0 = time.time()
send(9002, 'close_instance', {'instance_id': 'wf'})
got = {}
while time.time() < t0 + 40 and len(got) < 2:
    r, _, _ = select.select([srv.proc.stdout], [], [], 0.2)
    if not r: continue
    m = json.loads(srv.proc.stdout.readline())
    if m.get('id') in (9001, 9002):
        got[m['id']] = time.time() - t0
        print(m['id'], 'after %.1f s:' % got[m['id']], Result('', {}, m, 0).text.strip()[:400])
srv.close(); S.stop()
late = [i for i in (9001, 9002) if got.get(i, 99) > 10]
print('answered later than 10 s after close_instance:', late)
sys.exit(1 if late else 0)
