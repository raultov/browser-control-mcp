import sys, json, time, select
sys.path.insert(0, 'tests/compare')
from mcp_client import Server

# Several tool calls sent at once as the very first calls of a fresh server.
for attempt in range(1, 11):
    s = Server(['target/release/browser-control-mcp', '--headless'], timeout=90)
    ids = list(range(901, 909))
    data = ''.join(json.dumps({'jsonrpc': '2.0', 'id': i, 'method': 'tools/call',
                               'params': {'name': 'list_tabs', 'arguments': {}}}) + '\n' for i in ids)
    s.proc.stdin.write(data); s.proc.stdin.flush()
    got = {}
    deadline = time.time() + 60
    while len(got) < len(ids) and time.time() < deadline:
        ready, _, _ = select.select([s.proc.stdout], [], [], 1)
        if not ready:
            continue
        line = s.proc.stdout.readline()
        if not line:
            break
        msg = json.loads(line)
        if msg.get('id') in ids:
            got[msg['id']] = msg
    s.close()
    errors = [m['result']['content'][0]['text'] for m in got.values() if m.get('result', {}).get('isError')]
    print(f'attempt {attempt}: {len(got)} replies, errors: {errors}')
    if any('Failed to register default session' in e for e in errors):
        print('RACE: a concurrent first call failed with: ' + errors[0])
        sys.exit(1)
print('no failure in 10 attempts')
