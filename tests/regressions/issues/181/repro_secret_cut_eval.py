import os, sys, tempfile
sys.path.insert(0, os.path.join(os.getcwd(), 'tests', 'compare'))
from mcp_client import Server
from siteserver import Site

env = os.path.join(tempfile.mkdtemp(), 'cut.env')
open(env, 'w').write("CUTS='QaCutSecret7731xyz'\n")
site = Site()
s = Server(['target/release/browser-control-mcp', '--headless', '--secrets', env], timeout=120)
s.call('navigate', {'url': site.url('/basic.html')})
r = s.call('evaluate_js', {'expression': "'x'.repeat(99990) + 'QaCutSecret7731xyz'"})
s.close()
print('truncated:', '"truncated": true' in r.text)
print('reply ends with:', repr(r.text.strip()[-60:]))
leak = 'QaCutSec' in r.text
print('LEAK: secret prefix shown' if leak else 'masked')
sys.exit(1 if leak else 0)
