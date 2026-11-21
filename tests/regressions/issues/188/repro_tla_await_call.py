import os, sys
sys.path.insert(0, os.path.join(os.getcwd(), 'tests', 'compare'))
from mcp_client import Server
from siteserver import Site

site = Site()
s = Server(['target/release/browser-control-mcp', '--browser', 'chrome', '--headless'], timeout=120)
s.call('navigate', {'url': site.url('/basic.html')})
e = "await (Promise.resolve(7));" + " " * 65600 + "'v'"
r = s.call('evaluate_js', {'expression': e})
print('is_error=%s text=%s' % (r.is_error, r.text.strip()[:200]))
s.close()
sys.exit(1 if r.is_error else 0)
