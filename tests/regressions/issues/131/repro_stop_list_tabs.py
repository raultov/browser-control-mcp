import sys, json
sys.path.insert(0, 'tests/compare')
from mcp_client import Server

s = Server(['target/release/browser-control-mcp', '--headless'], timeout=90)
try:
    s.call('navigate', {'url': 'about:blank'})
    s.call('stop_browser', {})
    r = s.call('list_tabs', {})
    print(r.text.strip())
    tabs = json.loads(r.text)['tabs']
finally:
    s.close()
if len(tabs) == 1 and tabs[0]['url'] != 'about:blank':
    print('START_TAB_NOT_BLANK: ' + tabs[0]['url'])
    sys.exit(1)
print('ok')
