import sys, json
sys.path.insert(0, 'tests/compare')
from mcp_client import Server
from siteserver import Site
S = Site()
srv = Server(['target/release/browser-control-mcp', '--headless'])
r = srv.call('open_tab', {'url': S.url('/basic.html')}, timeout=60)
print('open_tab:', r.text.strip())
T = json.loads(r.text)['tab_id']
print('history.length:', srv.call('evaluate_js', {'tab_id': T, 'expression': 'history.length'}).text.strip().replace('\n', ' '))
b = srv.call('navigate', {'tab_id': T, 'history': 'back'}, timeout=60)
print('back from the first page:', 'ERROR' if b.is_error else 'ok', b.text.strip().replace('\n', ' '))
srv.close(); S.stop()
sys.exit(1 if (not b.is_error and 'about:blank' in b.text) else 0)
