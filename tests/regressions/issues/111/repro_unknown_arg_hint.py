import sys, os
sys.path.insert(0, 'tests/compare')
from mcp_client import Server
srv = Server(['target/release/browser-control-mcp', '--headless'])
listed = {t['name']: set((t['inputSchema'].get('properties') or {}).keys()) for t in srv.tools()}
r = srv.call('navigate', {'url': 'about:blank', 'foo': 1}, timeout=60)
srv.close()
print('reply:', r.text.strip())
V = set(r.text.strip().split('Valid arguments: ', 1)[1].strip().rstrip('.').split(', '))
print('tools/list navigate properties:', sorted(listed['navigate']))
print('hint lists but tools/list hides:', sorted(V - listed['navigate']))
sys.exit(1 if V - listed['navigate'] else 0)
