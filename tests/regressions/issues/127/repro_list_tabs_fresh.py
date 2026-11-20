import sys
sys.path.insert(0, 'tests/compare')
from mcp_client import Server

s = Server(['target/release/browser-control-mcp', '--headless'], timeout=90)
try:
    r1 = s.call('list_tabs', {})
    print('list_tabs (first call):', r1.text.strip())
    s.call('open_instance', {'label': 'qa2'})
    r2 = s.call('list_tabs', {'instance_id': 'qa2'})
    print('list_tabs qa2:', r2.text.strip())
    s.call('close_instance', {'instance_id': 'qa2'})
    if '"tabs":[]' in r1.text or '"tabs":[]' in r2.text:
        print('NO_TABS: Chrome list_tabs lists no tab on a fresh instance')
        sys.exit(1)
    print('ok')
finally:
    s.close()
