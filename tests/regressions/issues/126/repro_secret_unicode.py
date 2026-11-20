import os, sys, tempfile
sys.path.insert(0, 'tests/compare')
from mcp_client import Server
from siteserver import Site

env = os.path.join(tempfile.mkdtemp(), 'secrets.env')
open(env, 'w', encoding='utf-8').write('QA_TOKEN=Žluťoučký7731\n')
site = Site()
s = Server(['target/release/browser-control-mcp', '--headless', '--secrets', env], timeout=90)
try:
    s.call('navigate', {'url': site.url('/basic.html')})
    s.call('evaluate_js', {'expression': "document.body.insertAdjacentHTML('beforeend','<input id=qs>');1"})
    s.call('fill_input', {'selector': '#qs', 'text': '<secret:QA_TOKEN>'})
    r = s.call('evaluate_js', {'expression': "const r = await fetch('/api/echo?p='+encodeURIComponent(document.querySelector('#qs').value)); await r.text()"})
    print(r.text)
    if '017dlu' in r.text.lower():
        print('LEAKED: secret value returned as JSON \\u escapes')
        sys.exit(1)
    print('masked')
finally:
    s.close()
    site.stop()
