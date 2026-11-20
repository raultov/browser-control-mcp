import sys, time
sys.path.insert(0, 'tests/compare')
from mcp_client import Server

delay = float(sys.argv[1]) if len(sys.argv) > 1 else 0
s = Server(['target/release/browser-control-mcp', '--headless'], timeout=90)
try:
    s.call('navigate', {'url': 'data:text/html,<title>x</title><p>x</p>'})
    x = s.call('close_tab', {})
    print('close_tab:', x.text.strip())
    time.sleep(delay)
    a = s.call('list_tabs', {})
    print('list_tabs: is_error=%s %s' % (a.is_error, a.text.strip()))
finally:
    s.close()
if a.is_error:
    print('LIST_TABS_ERROR: list_tabs failed instead of listing no tab')
    sys.exit(1)
print('ok')
