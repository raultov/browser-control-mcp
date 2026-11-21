import json, os, sys
sys.path.insert(0, os.path.join(os.getcwd(), 'tests', 'compare'))
from mcp_client import Server

s = Server(['target/release/browser-control-mcp', '--browser', 'chrome', '--headless'], timeout=120)
tid = lambda r: ('ERR:' + r.text[:100]) if r.is_error else json.loads(r.text)['tab_id']
a = tid(s.call('open_tab', {}))
closed = s.call('close_tab', {'tab_id': a})
c = tid(s.call('open_tab', {}))
d = tid(s.call('open_tab', {}))
tabs = json.loads(s.call('list_tabs', {}).text)['tabs']
s.close()
print('first', a, 'close_error', closed.is_error, 'second', c, 'third', d, 'listed', len(tabs))
if c == d:
    print('BUG: two open_tab calls returned the same tab', c)
    sys.exit(1)
print('ok')
