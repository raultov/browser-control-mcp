import os, re, sys, tempfile
sys.path.insert(0, os.path.join(os.getcwd(), 'tests', 'compare'))
from mcp_client import Server
from siteserver import Site

env = os.path.join(tempfile.mkdtemp(), 'dom.env')
open(env, 'w').write("DOMS='QaDomSecret8842xyz'\n")
site = Site()
s = Server(['target/release/browser-control-mcp', '--headless', '--secrets', env], timeout=120)
s.call('navigate', {'url': site.url('/basic.html')})
s.call('evaluate_js', {'expression': "document.body.innerHTML = '<p id=\"pad\"></p><p id=\"s\"></p>'; "
                       "document.getElementById('pad').textContent = 'x'.repeat(90000); "
                       "document.getElementById('s').textContent = 'QaDomSecret8842xyz'; 1"})
r = s.call('inspect_dom', {'query': '<p id="s">'})
S = int(re.search(r'at position (\d+)', r.text).group(1)) + 10
# Pad so that the secret starts at character 99 990 of the HTML: the 100 000 cut splits it.
s.call('evaluate_js', {'expression': "document.getElementById('pad').textContent = 'x'.repeat(%d); 1" % (90000 + 99990 - S)})
full = s.call('inspect_dom', {})
s.close()
print('head:', repr(full.text[:80]))
print('tail:', repr(full.text[-40:]))
leak = 'QaDomSec' in full.text
print('LEAK: secret prefix shown' if leak else 'masked')
sys.exit(1 if leak else 0)
