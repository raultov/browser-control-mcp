import json, os, sys
sys.path.insert(0, os.path.join(os.getcwd(), 'tests', 'compare'))
from mcp_client import Server
from siteserver import Site

site = Site()
s = Server(['target/release/browser-control-mcp', '--browser', 'chrome', '--headless'], timeout=120)
a = json.loads(s.call('open_tab', {}).text)['tab_id']
b = json.loads(s.call('open_tab', {'url': site.url('/second.html')}).text)['tab_id']
tabs = json.loads(s.call('list_tabs', {}).text)['tabs']
s.close()
print('first open_tab:', a, 'second open_tab:', b)
print('list_tabs:', [(t['tab_id'], t['url']) for t in tabs])
if a == b:
    print('BUG: both open_tab calls returned the same tab; the first tab was navigated away')
    sys.exit(1)
