import sys, json
sys.path.insert(0, 'tests/compare')
from mcp_client import Server
from siteserver import Site
S = Site()
srv = Server(['target/release/browser-control-mcp', '--headless', '--browser', 'firefox', '--tool-groups', 'network'])
def call(name, args):
    r = srv.call(name, args, timeout=60)
    print(name, json.dumps(args), '->', 'ERROR' if r.is_error else 'ok', r.text.strip().replace('\n', ' ')[:200])
    return r
call('navigate', {'url': S.url('/basic.html')})
call('mock_route', {'url_pattern': '*/mocked/*', 'body': '{"ok":1}', 'content_type': 'application/json'})
call('set_offline', {'offline': True})
r = call('evaluate_js', {'expression': "fetch('/mocked/x').then(r => r.text())"})
call('list_routes', {})
srv.close(); S.stop()
print('mocked fetch while offline failed:', r.is_error)
sys.exit(1 if r.is_error else 0)
