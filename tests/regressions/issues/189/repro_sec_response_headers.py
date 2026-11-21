import base64, json, os, sys
sys.path.insert(0, os.path.join(os.getcwd(), 'tests', 'compare'))
from mcp_client import Server
from siteserver import Site

site = Site()
b = lambda t: base64.urlsafe_b64encode(t.encode()).decode()
url = '/headers?h=' + b('Sec-Fetch-Token: sfetch-secret-1') + '&h=' + b('Sec-CH-Session: sch-secret-2') + '&h=' + b('X-Build: build-42')
s = Server(['target/release/browser-control-mcp', '--browser', 'chrome', '--headless'], timeout=120)
s.call('navigate', {'url': site.url('/basic.html')})
s.call('evaluate_js', {'expression': "await fetch(%s).then(r => r.status)" % json.dumps(url)})
r = s.call('get_network_logs', {'url_filter': '/headers', 'include_details': True})
s.close()
headers = {}
for req in json.loads(r.text).get('requests', {}).values():
    headers = req.get('response_headers', {})
for name in ('Sec-Fetch-Token', 'Sec-CH-Session', 'X-Build'):
    print('%s: %s' % (name, headers.get(name)))
shown = [v for v in ('sfetch-secret-1', 'sch-secret-2') if v in r.text]
print('SHOWN without include_sensitive_headers: %s' % shown if shown else 'all redacted')
sys.exit(1 if shown else 0)
